use std::{path::Path, str::FromStr};

use anyhow::{bail, ensure, Context};
use bytes::Bytes;
use camino::{Utf8Path, Utf8PathBuf};

use itertools::Itertools;
use pageserver_api::{key::rel_block_to_key, reltag::RelTag};
use postgres_ffi::{pg_constants, relfile_utils::parse_relfilename, ControlFileData, DBState_DB_SHUTDOWNED, Oid, BLCKSZ};
use tokio::io::AsyncRead;
use tracing::{debug, trace, warn};
use utils::{id::{NodeId, TenantId, TimelineId}, shard::{ShardCount, ShardNumber, TenantShardId}};
use walkdir::WalkDir;

use crate::{context::{DownloadBehavior, RequestContext}, import_datadir, task_mgr::TaskKind, tenant::storage_layer::ImageLayerWriter};
use crate::config::PageServerConf;
use tokio::io::AsyncReadExt;

use pageserver_api::key::Key;

pub struct PgImportEnv {
    ctx: RequestContext,
    conf: &'static PageServerConf,
    tli: TimelineId,
    tsi: TenantShardId,
}

impl PgImportEnv {

    pub async fn init() -> anyhow::Result<PgImportEnv> {
        let ctx: RequestContext = RequestContext::new(TaskKind::DebugTool, DownloadBehavior::Error);
        let config = toml_edit::Document::new();
        let conf = PageServerConf::parse_and_validate(
            NodeId(42), 
            &config,
            &Utf8PathBuf::from("layers")
        )?;
        let conf = Box::leak(Box::new(conf));

        let tni = TenantId::from_str("42424242424242424242424242424242")?;
        let tli = TimelineId::from_str("42424242424242424242424242424242")?;
        let tsi = TenantShardId {
            tenant_id: tni,
            shard_number: ShardNumber(0),
            shard_count: ShardCount(0),
        };

        Ok(PgImportEnv {
            ctx,
            conf, 
            tli,
            tsi,
        })
    }

    pub async fn import_datadir(&mut self, pgdata_path: &Utf8Path, _tenant_path: &Utf8Path) -> anyhow::Result<()> {

        let pgdata_lsn = import_datadir::get_lsn_from_controlfile(&pgdata_path)?.align();

        let range = Key::MIN..Key::NON_L0_MAX;
        let mut one_big_layer = ImageLayerWriter::new(
            &self.conf,
            self.tli,
            self.tsi,
            &range,
            pgdata_lsn,
            &self.ctx,
        ).await?;

        // Import ordinary databases, DEFAULTTABLESPACE_OID is smaller than GLOBALTABLESPACE_OID, so import them first
        // Traverse database in increasing oid order
        WalkDir::new(pgdata_path.join("base"))
            .max_depth(1)
            .into_iter()
            .filter_map(|entry| {
                entry.ok().and_then(|path| {
                    path.file_name().to_string_lossy().parse::<i32>().ok()
                })
            })
            .sorted()
            .for_each(|dboid| {
                let path = pgdata_path.join("base").join(dboid.to_string());
                self.import_db(&mut one_big_layer, &path, pg_constants::DEFAULTTABLESPACE_OID).await;
            });

        // global catalogs now
        self.import_db(&mut one_big_layer, &pgdata_path.join("global"), postgres_ffi::pg_constants::GLOBALTABLESPACE_OID).await?;

        
        one_big_layer.finish_layer(&self.ctx).await?;

        // should we anything about the wal?

        Ok(())
    }

    async fn import_db(
        &mut self,
        layer_writer: &mut ImageLayerWriter,
        path: &Utf8PathBuf,
        spcnode: u32
    ) -> anyhow::Result<()> {

        WalkDir::new(path)
            .max_depth(1)
            .into_iter()
            .filter_map(|entry| {
                entry.ok().and_then(|path| {
                    let relfile = path.file_name().to_string_lossy();
                    parse_relfilename(&relfile).ok()
                })
            })
            .sorted()
            .for_each(|a|{
                self.import_rel_file();
            });

        Ok(())
    }

    async fn import_rel_file(
        &mut self,
        layer_writer: &mut ImageLayerWriter,
        path: &Utf8PathBuf,
        spcnode: u32
    ) -> anyhow::Result<()> {

        let mut reader = tokio::fs::File::open(path).await?;
        let len = std::fs::metadata(path)?.len();

        let mut buf: [u8; 8192] = [0u8; 8192];

        ensure!(len % BLCKSZ as usize == 0);
        let nblocks = len / BLCKSZ as usize;

        let rel = RelTag {
            spcnode: spcoid,
            dbnode: dboid,
            relnode,
            forknum,
        };

        let mut blknum: u32 = segno * (1024 * 1024 * 1024 / BLCKSZ as u32);

        loop {
            let r = reader.read_exact(&mut buf).await;
            match r {
                Ok(_) => {
                    let key = rel_block_to_key(rel, blknum);
                    layer_writer.put_image(key, Bytes::copy_from_slice(&buf), &self.ctx).await?;
                }

                Err(err) => match err.kind() {
                    std::io::ErrorKind::UnexpectedEof => {
                        // reached EOF. That's expected.
                        let relative_blknum = blknum - segno * (1024 * 1024 * 1024 / BLCKSZ as u32);
                        ensure!(relative_blknum == nblocks as u32, "unexpected EOF");
                        break;
                    }
                    _ => {
                        bail!("error reading file {}: {:#}", path.as_display(), err);
                    }
                },
            };
            blknum += 1;
        }

        Ok(())
    }

    async fn import_file(
        // modification: &mut DatadirModification<'_>,
        &mut self,
        layer_writer: &mut ImageLayerWriter,
        file_path: &Path,
        reader: &mut (impl AsyncRead + Send + Sync + Unpin),
        len: usize,
    ) -> anyhow::Result<Option<ControlFileData>> {
        let file_name = match file_path.file_name() {
            Some(name) => name.to_string_lossy(),
            None => return Ok(None),
        };
    
        if file_name.starts_with('.') {
            // tar archives on macOs, created without COPYFILE_DISABLE=1 env var
            // will contain "fork files", skip them.
            return Ok(None);
        }
    
        if file_path.starts_with("global") {
            let spcnode = postgres_ffi::pg_constants::GLOBALTABLESPACE_OID;
            let dbnode = 0;
    
            match file_name.as_ref() {
                // "pg_control" => {
                //     let bytes = read_all_bytes(reader).await?;
    
                //     // Extract the checkpoint record and import it separately.
                //     let pg_control = ControlFileData::decode(&bytes[..])?;
                //     let checkpoint_bytes = pg_control.checkPointCopy.encode()?;
                //     // modification.put_checkpoint(checkpoint_bytes)?;
                //     debug!("imported control file");
    
                //     // Import it as ControlFile
                //     // modification.put_control_file(bytes)?;
                //     return Ok(Some(pg_control));
                // }
                // "pg_filenode.map" => {
                //     // let bytes = read_all_bytes(reader).await?;
                //     // modification
                //     //     .put_relmap_file(spcnode, dbnode, bytes, ctx)
                //     //     .await?;
                //     debug!("imported relmap file")
                // }
                "PG_VERSION" => {
                    debug!("ignored PG_VERSION file");
                }
                _ => {
                    self.import_rel(layer_writer, file_path, spcnode, dbnode, reader, len).await?;
                    debug!("imported rel creation");
                }
            }
        } else if file_path.starts_with("base") {
            let spcnode = pg_constants::DEFAULTTABLESPACE_OID;
            let dbnode: u32 = file_path
                .iter()
                .nth(1)
                .expect("invalid file path, expected dbnode")
                .to_string_lossy()
                .parse()?;
    
            match file_name.as_ref() {
                // "pg_filenode.map" => {
                //     let bytes = read_all_bytes(reader).await?;
                //     modification
                //         .put_relmap_file(spcnode, dbnode, bytes, ctx)
                //         .await?;
                //     debug!("imported relmap file")
                // }
                "PG_VERSION" => {
                    debug!("ignored PG_VERSION file");
                }
                _ => {
                    self.import_rel(layer_writer, file_path, spcnode, dbnode, reader, len).await?;
                    debug!("imported rel creation");
                }
            }
        // } else if file_path.starts_with("pg_xact") {
        //     let slru = SlruKind::Clog;
    
        //     import_slru(modification, slru, file_path, reader, len, ctx).await?;
        //     debug!("imported clog slru");
        // } else if file_path.starts_with("pg_multixact/offsets") {
        //     let slru = SlruKind::MultiXactOffsets;
    
        //     import_slru(modification, slru, file_path, reader, len, ctx).await?;
        //     debug!("imported multixact offsets slru");
        // } else if file_path.starts_with("pg_multixact/members") {
        //     let slru = SlruKind::MultiXactMembers;
    
        //     import_slru(modification, slru, file_path, reader, len, ctx).await?;
        //     debug!("imported multixact members slru");
        // } else if file_path.starts_with("pg_twophase") {
        //     let xid = u32::from_str_radix(file_name.as_ref(), 16)?;
    
        //     let bytes = read_all_bytes(reader).await?;
        //     modification
        //         .put_twophase_file(xid, Bytes::copy_from_slice(&bytes[..]), ctx)
        //         .await?;
        //     debug!("imported twophase file");
        } else if file_path.starts_with("pg_wal") {
            debug!("found wal file in base section. ignore it");
        // } else if file_path.starts_with("zenith.signal") {
        //     // Parse zenith signal file to set correct previous LSN
        //     let bytes = read_all_bytes(reader).await?;
        //     // zenith.signal format is "PREV LSN: prev_lsn"
        //     // TODO write serialization and deserialization in the same place.
        //     let zenith_signal = std::str::from_utf8(&bytes)?.trim();
        //     let prev_lsn = match zenith_signal {
        //         "PREV LSN: none" => Lsn(0),
        //         "PREV LSN: invalid" => Lsn(0),
        //         other => {
        //             let split = other.split(':').collect::<Vec<_>>();
        //             split[1]
        //                 .trim()
        //                 .parse::<Lsn>()
        //                 .context("can't parse zenith.signal")?
        //         }
        //     };
    
        //     // zenith.signal is not necessarily the last file, that we handle
        //     // but it is ok to call `finish_write()`, because final `modification.commit()`
        //     // will update lsn once more to the final one.
        //     let writer = modification.tline.writer().await;
        //     writer.finish_write(prev_lsn);
    
        //     debug!("imported zenith signal {}", prev_lsn);
        } else if file_path.starts_with("pg_tblspc") {
            // TODO Backups exported from neon won't have pg_tblspc, but we will need
            // this to import arbitrary postgres databases.
            bail!("Importing pg_tblspc is not implemented");
        } else {
            debug!(
                "ignoring unrecognized file \"{}\" in tar archive",
                file_path.display()
            );
        }
    
        Ok(None)
    }
    

    // subroutine of import_timeline_from_postgres_datadir(), to load one relation file.
    async fn import_rel(
        // modification: &mut DatadirModification<'_>,
        &self,
        layer_writer: &mut ImageLayerWriter,
        path: &Path,
        spcoid: Oid,
        dboid: Oid,
        reader: &mut (impl AsyncRead + Unpin),
        len: usize,
    ) -> anyhow::Result<()> {
        // Does it look like a relation file?
        trace!("importing rel file {}", path.display());

        let filename = &path
            .file_name()
            .expect("missing rel filename")
            .to_string_lossy();
        let (relnode, forknum, segno) = parse_relfilename(filename).map_err(|e| {
            warn!("unrecognized file in postgres datadir: {:?} ({})", path, e);
            e
        })?;

        let mut buf: [u8; 8192] = [0u8; 8192];

        ensure!(len % BLCKSZ as usize == 0);
        let nblocks = len / BLCKSZ as usize;

        let rel = RelTag {
            spcnode: spcoid,
            dbnode: dboid,
            relnode,
            forknum,
        };

        let mut blknum: u32 = segno * (1024 * 1024 * 1024 / BLCKSZ as u32);


        loop {
            let r = reader.read_exact(&mut buf).await;
            match r {
                Ok(_) => {
                    let key = rel_block_to_key(rel, blknum);
                    layer_writer.put_image(key, Bytes::copy_from_slice(&buf), &self.ctx).await?;
                    // if modification.tline.get_shard_identity().is_key_local(&key) {
                    //     modification.put_rel_page_image(rel, blknum, Bytes::copy_from_slice(&buf))?;
                    // }
                }

                Err(err) => match err.kind() {
                    std::io::ErrorKind::UnexpectedEof => {
                        // reached EOF. That's expected.
                        let relative_blknum = blknum - segno * (1024 * 1024 * 1024 / BLCKSZ as u32);
                        ensure!(relative_blknum == nblocks as u32, "unexpected EOF");
                        break;
                    }
                    _ => {
                        bail!("error reading file {}: {:#}", path.display(), err);
                    }
                },
            };
            blknum += 1;
        }

        // img_layer_writer.finish_layer(ctx).await?;

        // // Update relation size
        // //
        // // If we process rel segments out of order,
        // // put_rel_extend will skip the update.
        // modification.put_rel_extend(rel, blknum, ctx).await?;

        Ok(())
    }



}

async fn read_all_bytes(reader: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Bytes> {
    let mut buf: Vec<u8> = vec![];
    reader.read_to_end(&mut buf).await?;
    Ok(Bytes::from(buf))
}

///////////////////////////////
// Set up timeline
// most of that is needed for image_layer_writer.finish()
// refactoring finish might be a better idea
///////////////////////////////


// let shard_id = ShardIdentity::unsharded();
// let tli = TimelineId::generate();
// let aaa_atc = Arc::new(ArcSwap::from(Arc::new(atc)));
// let tl_metadata = TimelineMetadata::new(
//     Lsn(0),
//     None,
//     None,
//     Lsn(0),
//     Lsn(4242),
//     Lsn(4242),
//     16,
// );
// let tc = models::TenantConfig {
//     ..models::TenantConfig::default()
// };
// let atc = AttachedTenantConf::try_from(LocationConf::attached_single(
//     TenantConfOpt{
//         ..Default::default()
//     },
//     Generation::new(42),
//     &ShardParameters::default(),
// ))?;



// // let walredo_mgr = Arc::new(WalRedoManager::from(TestRedoManager));

// let config = RemoteStorageConfig {
//     storage: RemoteStorageKind::LocalFs {
//         local_path: Utf8PathBuf::from("remote")
//     },
//     timeout: RemoteStorageConfig::DEFAULT_TIMEOUT,
// };
// let remote_storage = GenericRemoteStorage::from_config(&config).await.unwrap();
// let deletion_queue = MockDeletionQueue::new(Some(remote_storage.clone()));

// let remote_client = RemoteTimelineClient::new(
//     remote_storage,
//     deletion_queue.new_client(),
//     &conf,
//     tsi,
//     tli,
//     Generation::Valid(42),
// );


// let resources = TimelineResources {
//     remote_client,
//     timeline_get_throttle: tenant.timeline_get_throttle.clone(),
//     l0_flush_global_state: tenant.l0_flush_global_state.clone(),
// };


// let timeline = Timeline::new(
//     &conf,
//     aaa_atc,
//     &tl_metadata,
//     None,
//     tli,
//     TenantShardId {
//         tenant_id: tni,
//         shard_number: ShardNumber(0),
//         shard_count: ShardCount(0)
//     },
//     Generation::Valid(42),
//     shard_id,
//     None,
//     resources,
//     16,
//     state,
//     last_aux_file_policy,
//     self.cancel.child_token(),
// );
