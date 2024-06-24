//!
//! WAL redo. This service runs PostgreSQL in a special wal_redo mode
//! to apply given WAL records over an old page image and return new
//! page image.
//!
//! We rely on Postgres to perform WAL redo for us. We launch a
//! postgres process in special "wal redo" mode that's similar to
//! single-user mode. We then pass the previous page image, if any,
//! and all the WAL records we want to apply, to the postgres
//! process. Then we get the page image back. Communication with the
//! postgres process happens via stdin/stdout
//!
//! See pgxn/neon_walredo/walredoproc.c for the other side of
//! this communication.
//!
//! The Postgres process is assumed to be secure against malicious WAL
//! records. It achieves it by dropping privileges before replaying
//! any WAL records, so that even if an attacker hijacks the Postgres
//! process, he cannot escape out of it.

/// Process lifecycle and abstracction for the IPC protocol.
mod process;

/// Code to apply [`NeonWalRecord`]s.
pub(crate) mod apply_neon;

use crate::config::PageServerConf;
use crate::metrics::{
    WAL_REDO_BYTES_HISTOGRAM, WAL_REDO_PROCESS_LAUNCH_DURATION_HISTOGRAM,
    WAL_REDO_RECORDS_HISTOGRAM, WAL_REDO_TIME,
};
use crate::repository::Key;
use crate::walrecord::NeonWalRecord;
use anyhow::Context;
use bytes::{Bytes, BytesMut};
use pageserver_api::models::{WalRedoManagerProcessStatus, WalRedoManagerStatus};
use pageserver_api::shard::TenantShardId;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::*;
use utils::lsn::Lsn;
use utils::sync::gate::Gate;
use utils::sync::heavier_once_cell;

pub struct GlobalState {
    conf: &'static PageServerConf,
    /// Watched by the [`Self::actor`].
    shutdown: CancellationToken,
    /// Set by [`Self::actor`] when [`Self::shutdown`] is cancelled.
    /// We do this to avoid the Mutex lock inside the `CancellationToken`.
    shutdown_bool: AtomicBool,
    pub(self) spawn_gate: Gate,
}

impl GlobalState {
    pub async fn spawn(
        conf: &'static PageServerConf,
        shutdown: CancellationToken,
    ) -> Arc<GlobalState> {
        let state = Arc::new(GlobalState {
            conf,
            shutdown,
            shutdown_bool: AtomicBool::new(false), // if `shutdown` is cancelled already, the task spawned below will set it promptly
            spawn_gate: Gate::default(),
        });
        tokio::spawn({
            let state = Arc::clone(&state);
            async move {
                info!("starting");
                state.actor().await;
                info!("done");
            }
            .instrument(info_span!(parent: None, "walredo_global_state"))
        });
        state
    }
    async fn actor(self: Arc<Self>) {
        self.shutdown.cancelled().await;
        info!("propagating cancellation");
        self.shutdown_bool.store(true, Ordering::Relaxed);
        self.spawn_gate.close().await;
        info!("all walredo processes have been killed and no new ones will be spawned");
    }
    pub(self) fn is_shutdown_requested(self: &Arc<Self>) -> bool {
        self.shutdown_bool.load(Ordering::Relaxed)
    }
}

///
/// This is the real implementation that uses a Postgres process to
/// perform WAL replay. Only one thread can use the process at a time,
/// that is controlled by the Mutex. In the future, we might want to
/// launch a pool of processes to allow concurrent replay of multiple
/// records.
///
pub struct PostgresRedoManager {
    global_state: Arc<GlobalState>,
    tenant_shard_id: TenantShardId,
    last_redo_at: std::sync::Mutex<Option<Instant>>,
    /// The current [`process::WalRedoProcess`] that is used by new redo requests.
    /// We use [`heavier_once_cell`] for coalescing the spawning, but the redo
    /// requests don't use the [`heavier_once_cell::Guard`] to keep ahold of the
    /// their process object; we use [`Arc::clone`] for that.
    /// This is primarily because earlier implementations that didn't  use [`heavier_once_cell`]
    /// had that behavior; it's probably unnecessary.
    /// The only merit of it is that if one walredo process encounters an error,
    /// it can take it out of rotation (= using [`heavier_once_cell::Guard::take_and_deinit`].
    /// and retry redo, thereby starting the new process, while other redo tasks might
    /// still be using the old redo process. But, those other tasks will most likely
    /// encounter an error as well, and errors are an unexpected condition anyway.
    /// So, probably we could get rid of the `Arc` in the future.
    redo_process: heavier_once_cell::OnceCell<Arc<process::WalRedoProcess>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::walredo::Error::Other(::anyhow::anyhow!($($arg)*)));
    }
}
pub(self) use bail;

///
/// Public interface of WAL redo manager
///
impl PostgresRedoManager {
    ///
    /// Request the WAL redo manager to apply some WAL records
    ///
    /// The WAL redo is handled by a separate thread, so this just sends a request
    /// to the thread and waits for response.
    ///
    /// # Cancel-Safety
    ///
    /// This method is cancellation-safe.
    pub async fn request_redo(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<(Lsn, Bytes)>,
        records: Vec<(Lsn, NeonWalRecord)>,
        pg_version: u32,
    ) -> Result<Bytes, Error> {
        if records.is_empty() {
            bail!("invalid WAL redo request with no records");
        }

        let base_img_lsn = base_img.as_ref().map(|p| p.0).unwrap_or(Lsn::INVALID);
        let mut img = base_img.map(|p| p.1);
        let mut batch_neon = apply_neon::can_apply_in_neon(&records[0].1);
        let mut batch_start = 0;
        for (i, record) in records.iter().enumerate().skip(1) {
            let rec_neon = apply_neon::can_apply_in_neon(&record.1);

            if rec_neon != batch_neon {
                let result = if batch_neon {
                    self.apply_batch_neon(key, lsn, img, &records[batch_start..i])
                } else {
                    self.apply_batch_postgres(
                        key,
                        lsn,
                        img,
                        base_img_lsn,
                        &records[batch_start..i],
                        self.global_state.conf.wal_redo_timeout,
                        pg_version,
                    )
                    .await
                };
                img = Some(result?);

                batch_neon = rec_neon;
                batch_start = i;
            }
        }
        // last batch
        if batch_neon {
            self.apply_batch_neon(key, lsn, img, &records[batch_start..])
        } else {
            self.apply_batch_postgres(
                key,
                lsn,
                img,
                base_img_lsn,
                &records[batch_start..],
                self.global_state.conf.wal_redo_timeout,
                pg_version,
            )
            .await
        }
    }

    pub fn status(&self) -> WalRedoManagerStatus {
        WalRedoManagerStatus {
            last_redo_at: {
                let at = *self.last_redo_at.lock().unwrap();
                at.and_then(|at| {
                    let age = at.elapsed();
                    // map any chrono errors silently to None here
                    chrono::Utc::now().checked_sub_signed(chrono::Duration::from_std(age).ok()?)
                })
            },
            process: self
                .redo_process
                .get()
                .map(|p| WalRedoManagerProcessStatus { pid: p.id() }),
        }
    }
}

impl PostgresRedoManager {
    ///
    /// Create a new PostgresRedoManager.
    ///
    pub fn new(
        global_state: Arc<GlobalState>,
        tenant_shard_id: TenantShardId,
    ) -> PostgresRedoManager {
        // The actual process is launched lazily, on first request.
        PostgresRedoManager {
            global_state,
            tenant_shard_id,
            last_redo_at: std::sync::Mutex::default(),
            redo_process: heavier_once_cell::OnceCell::default(),
        }
    }

    /// This type doesn't have its own background task to check for idleness: we
    /// rely on our owner calling this function periodically in its own housekeeping
    /// loops.
    pub(crate) fn maybe_quiesce(&self, idle_timeout: Duration) {
        if let Ok(g) = self.last_redo_at.try_lock() {
            if let Some(last_redo_at) = *g {
                if last_redo_at.elapsed() >= idle_timeout {
                    drop(g);
                    drop(self.redo_process.get().map(|guard| guard.take_and_deinit()));
                }
            }
        }
    }

    ///
    /// Process one request for WAL redo using wal-redo postgres
    ///
    /// # Cancel-Safety
    ///
    /// Cancellation safe.
    #[allow(clippy::too_many_arguments)]
    async fn apply_batch_postgres(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<Bytes>,
        base_img_lsn: Lsn,
        records: &[(Lsn, NeonWalRecord)],
        wal_redo_timeout: Duration,
        pg_version: u32,
    ) -> Result<Bytes, Error> {
        *(self.last_redo_at.lock().unwrap()) = Some(Instant::now());

        let (rel, blknum) = key.to_rel_block().context("invalid record")?;
        const MAX_RETRY_ATTEMPTS: u32 = 1;
        let mut n_attempts = 0u32;
        loop {
            let proc: Arc<process::WalRedoProcess> =
                match self.redo_process.get_or_init_detached().await {
                    Ok(guard) => Arc::clone(&guard),
                    Err(permit) => {
                        // don't hold poison_guard, the launch code can bail
                        let start = Instant::now();
                        let proc = Arc::new(
                            process::WalRedoProcess::launch(
                                &self.global_state,
                                self.tenant_shard_id,
                                pg_version,
                            )
                            .map_err(|e| match e {
                                process::LaunchError::Cancelled => Error::Cancelled,
                                process::LaunchError::Other(e) => {
                                    Error::Other(e.context("launch walredo process"))
                                }
                            })?,
                        );
                        let duration = start.elapsed();
                        WAL_REDO_PROCESS_LAUNCH_DURATION_HISTOGRAM.observe(duration.as_secs_f64());
                        info!(
                            duration_ms = duration.as_millis(),
                            pid = proc.id(),
                            "launched walredo process"
                        );
                        self.redo_process.set(Arc::clone(&proc), permit);
                        proc
                    }
                };

            let started_at = std::time::Instant::now();

            // Relational WAL records are applied using wal-redo-postgres
            let result = proc
                .apply_wal_records(rel, blknum, &base_img, records, wal_redo_timeout)
                .await
                .context("apply_wal_records");
            if result.is_err() {
                // avoid
                if self.global_state.shutdown.is_cancelled() {
                    return Err(Error::Cancelled);
                }
            }

            let duration = started_at.elapsed();

            let len = records.len();
            let nbytes = records.iter().fold(0, |acumulator, record| {
                acumulator
                    + match &record.1 {
                        NeonWalRecord::Postgres { rec, .. } => rec.len(),
                        _ => unreachable!("Only PostgreSQL records are accepted in this batch"),
                    }
            });

            WAL_REDO_TIME.observe(duration.as_secs_f64());
            WAL_REDO_RECORDS_HISTOGRAM.observe(len as f64);
            WAL_REDO_BYTES_HISTOGRAM.observe(nbytes as f64);

            debug!(
                "postgres applied {} WAL records ({} bytes) in {} us to reconstruct page image at LSN {}",
                len,
                nbytes,
                duration.as_micros(),
                lsn
            );

            // If something went wrong, don't try to reuse the process. Kill it, and
            // next request will launch a new one.
            if let Err(e) = result.as_ref() {
                error!(
                    "error applying {} WAL records {}..{} ({} bytes) to key {key}, from base image with LSN {} to reconstruct page image at LSN {} n_attempts={}: {:?}",
                    records.len(),
                    records.first().map(|p| p.0).unwrap_or(Lsn(0)),
                    records.last().map(|p| p.0).unwrap_or(Lsn(0)),
                    nbytes,
                    base_img_lsn,
                    lsn,
                    n_attempts,
                    e,
                );
                // Avoid concurrent callers hitting the same issue by taking `proc` out of the rotation.
                // Note that there may be other tasks concurrent with us that also hold `proc`.
                // We have to deal with that here.
                // Also read the doc comment on field `self.redo_process`.
                //
                // NB: there may still be other concurrent threads using `proc`.
                // The last one will send SIGKILL when the underlying Arc reaches refcount 0.
                //
                // NB: the drop impl blocks the dropping thread with a wait() system call for
                // the child process. In some ways the blocking is actually good: if we
                // deferred the waiting into the background / to tokio if we used `tokio::process`,
                // it could happen that if walredo always fails immediately, we spawn processes faster
                // than we can SIGKILL & `wait` for them to exit. By doing it the way we do here,
                // we limit this risk of run-away to at most $num_runtimes * $num_executor_threads.
                // This probably needs revisiting at some later point.
                match self.redo_process.get() {
                    None => (),
                    Some(guard) => {
                        if Arc::ptr_eq(&proc, &*guard) {
                            // We're the first to observe an error from `proc`, it's our job to take it out of rotation.
                            guard.take_and_deinit();
                        } else {
                            // Another task already spawned another redo process (further up in this method)
                            // and put it into `redo_process`. Do nothing, our view of the world is behind.
                        }
                    }
                }
                // The last task that does this `drop()` of `proc` will do a blocking `wait()` syscall.
                drop(proc);
            } else if n_attempts != 0 {
                info!(n_attempts, "retried walredo succeeded");
            }
            n_attempts += 1;
            if n_attempts > MAX_RETRY_ATTEMPTS || result.is_ok() {
                return result.map_err(Error::Other);
            }
        }
    }

    ///
    /// Process a batch of WAL records using bespoken Neon code.
    ///
    fn apply_batch_neon(
        &self,
        key: Key,
        lsn: Lsn,
        base_img: Option<Bytes>,
        records: &[(Lsn, NeonWalRecord)],
    ) -> Result<Bytes, Error> {
        let start_time = Instant::now();

        let mut page = BytesMut::new();
        if let Some(fpi) = base_img {
            // If full-page image is provided, then use it...
            page.extend_from_slice(&fpi[..]);
        } else {
            // All the current WAL record types that we can handle require a base image.
            bail!("invalid neon WAL redo request with no base image");
        }

        // Apply all the WAL records in the batch
        for (record_lsn, record) in records.iter() {
            self.apply_record_neon(key, &mut page, *record_lsn, record)?;
        }
        // Success!
        let duration = start_time.elapsed();
        // FIXME: using the same metric here creates a bimodal distribution by default, and because
        // there could be multiple batch sizes this would be N+1 modal.
        WAL_REDO_TIME.observe(duration.as_secs_f64());

        debug!(
            "neon applied {} WAL records in {} us to reconstruct page image at LSN {}",
            records.len(),
            duration.as_micros(),
            lsn
        );

        Ok(page.freeze())
    }

    fn apply_record_neon(
        &self,
        key: Key,
        page: &mut BytesMut,
        record_lsn: Lsn,
        record: &NeonWalRecord,
    ) -> anyhow::Result<()> {
        apply_neon::apply_in_neon(record, record_lsn, key, page)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::PostgresRedoManager;
    use crate::repository::Key;
    use crate::{config::PageServerConf, walrecord::NeonWalRecord};
    use bytes::Bytes;
    use pageserver_api::shard::TenantShardId;
    use std::str::FromStr;
    use tracing::Instrument;
    use utils::{id::TenantId, lsn::Lsn};

    #[tokio::test]
    async fn short_v14_redo() {
        let expected = std::fs::read("test_data/short_v14_redo.page").unwrap();

        let h = RedoHarness::new().unwrap();

        let page = h
            .manager
            .request_redo(
                Key {
                    field1: 0,
                    field2: 1663,
                    field3: 13010,
                    field4: 1259,
                    field5: 0,
                    field6: 0,
                },
                Lsn::from_str("0/16E2408").unwrap(),
                None,
                short_records(),
                14,
            )
            .instrument(h.span())
            .await
            .unwrap();

        assert_eq!(&expected, &*page);
    }

    #[tokio::test]
    async fn short_v14_fails_for_wrong_key_but_returns_zero_page() {
        let h = RedoHarness::new().unwrap();

        let page = h
            .manager
            .request_redo(
                Key {
                    field1: 0,
                    field2: 1663,
                    // key should be 13010
                    field3: 13130,
                    field4: 1259,
                    field5: 0,
                    field6: 0,
                },
                Lsn::from_str("0/16E2408").unwrap(),
                None,
                short_records(),
                14,
            )
            .instrument(h.span())
            .await
            .unwrap();

        // TODO: there will be some stderr printout, which is forwarded to tracing that could
        // perhaps be captured as long as it's in the same thread.
        assert_eq!(page, crate::ZERO_PAGE);
    }

    #[tokio::test]
    async fn test_stderr() {
        let h = RedoHarness::new().unwrap();
        h
            .manager
            .request_redo(
                Key::from_i128(0),
                Lsn::INVALID,
                None,
                short_records(),
                16, /* 16 currently produces stderr output on startup, which adds a nice extra edge */
            )
            .instrument(h.span())
            .await
            .unwrap_err();
    }

    #[allow(clippy::octal_escapes)]
    fn short_records() -> Vec<(Lsn, NeonWalRecord)> {
        vec![
            (
                Lsn::from_str("0/16A9388").unwrap(),
                NeonWalRecord::Postgres {
                    will_init: true,
                    rec: Bytes::from_static(b"j\x03\0\0\0\x04\0\0\xe8\x7fj\x01\0\0\0\0\0\n\0\0\xd0\x16\x13Y\0\x10\0\04\x03\xd4\0\x05\x7f\x06\0\0\xd22\0\0\xeb\x04\0\0\0\0\0\0\xff\x03\0\0\0\0\x80\xeca\x01\0\0\x01\0\xd4\0\xa0\x1d\0 \x04 \0\0\0\0/\0\x01\0\xa0\x9dX\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0.\0\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\00\x9f\x9a\x01P\x9e\xb2\x01\0\x04\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x02\0!\0\x01\x08 \xff\xff\xff?\0\0\0\0\0\0@\0\0another_table\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x98\x08\0\0\x02@\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0@\0\0\0\0\0\0\0\0\0\0\0\0\x80\xbf\0\0\0\0\0\0\0\0\0\0pr\x01\0\0\0\0\0\0\0\0\x01d\0\0\0\0\0\0\x04\0\0\x01\0\0\0\0\0\0\0\x0c\x02\0\0\0\0\0\0\0\0\0\0\0\0\0\0/\0!\x80\x03+ \xff\xff\xff\x7f\0\0\0\0\0\xdf\x04\0\0pg_type\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x0b\0\0\0G\0\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0\0\0\0\0\0\0\0\x0e\0\0\0\0@\x16D\x0e\0\0\0K\x10\0\0\x01\0pr \0\0\0\0\0\0\0\0\x01n\0\0\0\0\0\xd6\x02\0\0\x01\0\0\0[\x01\0\0\0\0\0\0\0\t\x04\0\0\x02\0\0\0\x01\0\0\0\n\0\0\0\n\0\0\0\x7f\0\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0\0\0C\x01\0\0\x15\x01\0\0\0\0\0\0\0\0\0\0\0\0\0\0.\0!\x80\x03+ \xff\xff\xff\x7f\0\0\0\0\0;\n\0\0pg_statistic\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x0b\0\0\0\xfd.\0\0\0\0\0\0\n\0\0\0\x02\0\0\0;\n\0\0\0\0\0\0\x13\0\0\0\0\0\xcbC\x13\0\0\0\x18\x0b\0\0\x01\0pr\x1f\0\0\0\0\0\0\0\0\x01n\0\0\0\0\0\xd6\x02\0\0\x01\0\0\0C\x01\0\0\0\0\0\0\0\t\x04\0\0\x01\0\0\0\x01\0\0\0\n\0\0\0\n\0\0\0\x7f\0\0\0\0\0\0\x02\0\x01")
                }
            ),
            (
                Lsn::from_str("0/16D4080").unwrap(),
                NeonWalRecord::Postgres {
                    will_init: false,
                    rec: Bytes::from_static(b"\xbc\0\0\0\0\0\0\0h?m\x01\0\0\0\0p\n\0\09\x08\xa3\xea\0 \x8c\0\x7f\x06\0\0\xd22\0\0\xeb\x04\0\0\0\0\0\0\xff\x02\0@\0\0another_table\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\x98\x08\0\0\x02@\0\0\0\0\0\0\n\0\0\0\x02\0\0\0\0@\0\0\0\0\0\0\x05\0\0\0\0@zD\x05\0\0\0\0\0\0\0\0\0pr\x01\0\0\0\0\0\0\0\0\x01d\0\0\0\0\0\0\x04\0\0\x01\0\0\0\x02\0")
                }
            )
        ]
    }

    struct RedoHarness {
        // underscored because unused, except for removal at drop
        _repo_dir: camino_tempfile::Utf8TempDir,
        manager: PostgresRedoManager,
        tenant_shard_id: TenantShardId,
    }

    impl RedoHarness {
        fn new() -> anyhow::Result<Self> {
            crate::tenant::harness::setup_logging();

            let repo_dir = camino_tempfile::tempdir()?;
            let conf = PageServerConf::dummy_conf(repo_dir.path().to_path_buf());
            let conf = Box::leak(Box::new(conf));
            let tenant_shard_id = TenantShardId::unsharded(TenantId::generate());

            let manager = PostgresRedoManager::new(conf, tenant_shard_id);

            Ok(RedoHarness {
                _repo_dir: repo_dir,
                manager,
                tenant_shard_id,
            })
        }
        fn span(&self) -> tracing::Span {
            tracing::info_span!("RedoHarness", tenant_id=%self.tenant_shard_id.tenant_id, shard_id=%self.tenant_shard_id.shard_slug())
        }
    }
}
