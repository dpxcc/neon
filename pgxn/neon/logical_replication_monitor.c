#include <dirent.h>
#include <limits.h>
#include <string.h>
#include <signal.h>
#include <sys/stat.h>

#include "postgres.h"

#include "miscadmin.h"
#include "postmaster/bgworker.h"
#include "postmaster/interrupt.h"
#include "replication/slot.h"
#include "storage/fd.h"
#include "storage/procsignal.h"
#include "tcop/tcopprot.h"
#include "utils/guc.h"
#include "utils/wait_event.h"

#include "logical_replication_monitor.h"

#define LS_MONITOR_CHECK_INTERVAL 10000 /* ms */

static int	logical_replication_max_snap_files = 300;
static int	logical_replication_max_logicalsnapdir_size = 128;

/*
 * A primitive description of a logical snapshot file including the LSN of the
 * file and its size.
 */
typedef struct SnapDesc {
	XLogRecPtr	lsn;
	off_t		sz;
} SnapDesc;

PGDLLEXPORT void LogicalSlotsMonitorMain(Datum main_arg);

/*
 * Sorts an array of snapshot descriptors by their LSN.
 */
static int
SnapDescComparator(const void *a, const void *b)
{
	const SnapDesc	*desc1 = a;
	const SnapDesc	*desc2 = b;

	if (desc1->lsn < desc2->lsn)
		return 1;
	else if (desc1->lsn == desc2->lsn)
		return 0;
	else
		return -1;
}

/*
 * Look at .snap files and calculate minimum allowed restart_lsn of slot so that
 * next gc would leave not more than logical_replication_max_snap_files; all
 * slots having lower restart_lsn should be dropped.
 */
static XLogRecPtr
get_snapshots_cutoff_lsn(void)
{
#define SNAPDIR "pg_logical/snapshots"

	DIR		   *dirdesc;
	int			dirdesc_fd;
	struct dirent *de;
	size_t		snapshot_index = 0;
	SnapDesc   *snapshot_descriptors;
	size_t		descriptors_allocated = 1024;
	XLogRecPtr	cutoff = 0;
	off_t		logicalsnapdir_size = 0;
	const int	logical_replication_max_logicalsnapdir_size_bytes = logical_replication_max_logicalsnapdir_size * 1000;

	if (logical_replication_max_snap_files < 0 || logical_replication_max_logicalsnapdir_size < 0)
		return 0;

	snapshot_descriptors = palloc(sizeof(*snapshot_descriptors) * descriptors_allocated);

	dirdesc = AllocateDir(SNAPDIR);
	dirdesc_fd = dirfd(dirdesc);
	if (dirdesc_fd == -1)
		ereport(ERROR, errmsg("failed to get a file descriptor for " SNAPDIR ": %m"));

	/* find all .snap files and get their lsns */
	while ((de = ReadDir(dirdesc, SNAPDIR)) != NULL)
	{
		uint32		hi;
		uint32		lo;
		struct stat	st;
		XLogRecPtr	lsn;
		SnapDesc   *desc;

		if (strcmp(de->d_name, ".") == 0 ||
			strcmp(de->d_name, "..") == 0)
			continue;

		if (sscanf(de->d_name, "%X-%X.snap", &hi, &lo) != 2)
		{
			ereport(LOG,
					(errmsg("could not parse file name as .snap file \"%s\"", de->d_name)));
			continue;
		}

		lsn = ((uint64) hi) << 32 | lo;
		elog(DEBUG5, "found snap file %X/%X", LSN_FORMAT_ARGS(lsn));

		if (fstatat(dirdesc_fd, de->d_name, &st, 0) == -1)
			ereport(ERROR, errmsg("failed to get the size of " SNAPDIR "/%s: %m", de->d_name));

		if (descriptors_allocated == snapshot_index)
		{
			descriptors_allocated *= 2;
			snapshot_descriptors = repalloc(snapshot_descriptors, sizeof(*snapshot_descriptors) * descriptors_allocated);
		}

		desc = &snapshot_descriptors[snapshot_index++];
		desc->lsn = lsn;
		desc->sz = st.st_size;
	}

	qsort(snapshot_descriptors, snapshot_index, sizeof(*snapshot_descriptors), SnapDescComparator);

	/* Are there more snapshot files than specified? */
	if (logical_replication_max_snap_files <= snapshot_index)
	{
		cutoff = snapshot_descriptors[logical_replication_max_snap_files - 1].lsn;
		elog(LOG,
			"ls_monitor: dropping logical slots with restart_lsn lower %X/%X, found %zu snapshot files, limit is %d",
			LSN_FORMAT_ARGS(cutoff), snapshot_index, logical_replication_max_snap_files);
	}

	/* Is the size of the logical snapshots directory larger than specified?
	 *
	 * It's possible we could hit both thresholds, so remove any extra files
	 * first, and then truncate based on size of the remaining files.
	 */
	if (logical_replication_max_logicalsnapdir_size_bytes > logicalsnapdir_size)
	{
		/* Unfortunately, iterating the directory does not guarantee any order
		 * so we can't cache an index in the preceding loop.
		 */

		off_t		sz = 0;
		const XLogRecPtr original = cutoff;

		for (size_t i = 0; i < logical_replication_max_snap_files; ++i)
		{
			sz += snapshot_descriptors[i].sz;
			if (sz > logical_replication_max_logicalsnapdir_size_bytes)
			{
				/* TODO: What should we do if i == 0? This would mean that the
				 * very first snapshot file (based on LSN) is too large, which
				 * will probably never be the case.
				 */
				cutoff = snapshot_descriptors[i - 1].lsn;
				break;
			}
		}

		if (cutoff != original)
			elog(LOG, "ls_monitor: dropping logical slots with restart_lsn lower than %X/%X, " SNAPDIR " is larger than %d KB",
					LSN_FORMAT_ARGS(cutoff), logical_replication_max_logicalsnapdir_size);
	}

	pfree(snapshot_descriptors);
	FreeDir(dirdesc);

	return cutoff;

#undef SNAPDIR
}

void
InitLogicalReplicationMonitor(void)
{
	BackgroundWorker bgw;

	DefineCustomIntVariable(
							"neon.logical_replication_max_snap_files",
							"Maximum allowed logical replication .snap files. When exceeded, slots are dropped until the limit is met. -1 disables the limit.",
							NULL,
							&logical_replication_max_snap_files,
							300, -1, INT_MAX,
							PGC_SIGHUP,
							0,
							NULL, NULL, NULL);

	DefineCustomIntVariable(
							"neon.logical_replication_max_logicalsnapdir_size",
							"Maximum allowed size of the pg_logical/snapshots directory (KB). When exceeded, slots are dropped until the limit is met. -1 disables the limit.",
							NULL,
							&logical_replication_max_logicalsnapdir_size,
							128, -1, INT_MAX,
							PGC_SIGHUP,
							GUC_UNIT_KB,
							NULL, NULL, NULL);

	memset(&bgw, 0, sizeof(bgw));
	bgw.bgw_flags = BGWORKER_SHMEM_ACCESS;
	bgw.bgw_start_time = BgWorkerStart_RecoveryFinished;
	snprintf(bgw.bgw_library_name, BGW_MAXLEN, "neon");
	snprintf(bgw.bgw_function_name, BGW_MAXLEN, "LogicalSlotsMonitorMain");
	snprintf(bgw.bgw_name, BGW_MAXLEN, "Logical replication monitor");
	snprintf(bgw.bgw_type, BGW_MAXLEN, "Logical replication monitor");
	bgw.bgw_restart_time = 5;
	bgw.bgw_notify_pid = 0;
	bgw.bgw_main_arg = (Datum) 0;

	RegisterBackgroundWorker(&bgw);
}

/*
 * Unused logical replication slots pins WAL and prevents deletion of snapshots.
 * WAL bloat is guarded by max_slot_wal_keep_size; this bgw removes slots which
 * need too many .snap files.
 */
void
LogicalSlotsMonitorMain(Datum main_arg)
{
	/* Establish signal handlers. */
	pqsignal(SIGUSR1, procsignal_sigusr1_handler);
	pqsignal(SIGHUP, SignalHandlerForConfigReload);
	pqsignal(SIGTERM, die);

	BackgroundWorkerUnblockSignals();

	for (;;)
	{
		XLogRecPtr	cutoff_lsn;

		/* In case of a SIGHUP, just reload the configuration. */
		if (ConfigReloadPending)
		{
			ConfigReloadPending = false;
			ProcessConfigFile(PGC_SIGHUP);
		}

		/*
		 * If there are too many .snap files, just drop all logical slots to
		 * prevent aux files bloat.
		 */
		cutoff_lsn = get_snapshots_cutoff_lsn();
		if (cutoff_lsn > 0)
		{
			for (int i = 0; i < max_replication_slots; i++)
			{
				char		slot_name[NAMEDATALEN];
				ReplicationSlot *s = &ReplicationSlotCtl->replication_slots[i];
				XLogRecPtr	restart_lsn;

				/* find the name */
				LWLockAcquire(ReplicationSlotControlLock, LW_SHARED);
				/* Consider only logical repliction slots */
				if (!s->in_use || !SlotIsLogical(s))
				{
					LWLockRelease(ReplicationSlotControlLock);
					continue;
				}

				/* do we need to drop it? */
				SpinLockAcquire(&s->mutex);
				restart_lsn = s->data.restart_lsn;
				SpinLockRelease(&s->mutex);
				if (restart_lsn >= cutoff_lsn)
				{
					LWLockRelease(ReplicationSlotControlLock);
					continue;
				}

				strlcpy(slot_name, s->data.name.data, NAMEDATALEN);
				elog(LOG, "ls_monitor: dropping slot %s with restart_lsn %X/%X below horizon %X/%X",
					 slot_name, LSN_FORMAT_ARGS(restart_lsn), LSN_FORMAT_ARGS(cutoff_lsn));
				LWLockRelease(ReplicationSlotControlLock);

				/* now try to drop it, killing owner before if any */
				for (;;)
				{
					pid_t		active_pid;

					SpinLockAcquire(&s->mutex);
					active_pid = s->active_pid;
					SpinLockRelease(&s->mutex);

					if (active_pid == 0)
					{
						/*
						 * Slot is releasted, try to drop it. Though of course
						 * it could have been reacquired, so drop can ERROR
						 * out. Similarly it could have been dropped in the
						 * meanwhile.
						 *
						 * In principle we could remove pg_try/pg_catch, that
						 * would restart the whole bgworker.
						 */
						ConditionVariableCancelSleep();
						PG_TRY();
						{
							ReplicationSlotDrop(slot_name, true);
							elog(LOG, "ls_monitor: slot %s dropped", slot_name);
						}
						PG_CATCH();
						{
							/* log ERROR and reset elog stack */
							EmitErrorReport();
							FlushErrorState();
							elog(LOG, "ls_monitor: failed to drop slot %s", slot_name);
						}
						PG_END_TRY();
						break;
					}
					else
					{
						/* kill the owner and wait for release */
						elog(LOG, "ls_monitor: killing slot %s owner %d", slot_name, active_pid);
						(void) kill(active_pid, SIGTERM);
						/* We shouldn't get stuck, but to be safe add timeout. */
						ConditionVariableTimedSleep(&s->active_cv, 1000, WAIT_EVENT_REPLICATION_SLOT_DROP);
					}
				}
			}
		}

		(void) WaitLatch(MyLatch,
						 WL_LATCH_SET | WL_EXIT_ON_PM_DEATH | WL_TIMEOUT,
						 LS_MONITOR_CHECK_INTERVAL,
						 PG_WAIT_EXTENSION);
		ResetLatch(MyLatch);
		CHECK_FOR_INTERRUPTS();
	}
}
