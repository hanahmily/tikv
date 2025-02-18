// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::borrow::Borrow;
use std::marker::PhantomData;
use std::time::Duration;
use std::{mem, thread, u64};

use futures::future;
use keys::{Key, Value};
use kvproto::kvrpcpb::{CommandPri, Context, LockInfo};

use crate::storage::kv::with_tls_engine;
use crate::storage::kv::{CbContext, Modify, Result as EngineResult};
use crate::storage::lock_manager::{self, Lock, LockManager};
use crate::storage::mvcc::{
    has_data_in_range, Error as MvccError, ErrorInner as MvccErrorInner, Lock as MvccLock,
    MvccReader, MvccTxn, TimeStamp, Write, MAX_TXN_WRITE_SIZE,
};
use crate::storage::txn::{sched_pool::*, scheduler::Msg, Error, ErrorInner, Result};
use crate::storage::types::ProcessResult;
use crate::storage::{
    metrics::{self, KV_COMMAND_KEYWRITE_HISTOGRAM_VEC, SCHED_STAGE_COUNTER_VEC},
    Command, CommandKind, Engine, Error as StorageError, ErrorInner as StorageErrorInner, MvccInfo,
    Result as StorageResult, ScanMode, Snapshot, Statistics, TxnStatus,
};
use engine::CF_WRITE;
use tikv_util::collections::HashMap;
use tikv_util::time::{Instant, SlowTimer};

pub const FORWARD_MIN_MUTATIONS_NUM: usize = 12;

// To resolve a key, the write size is about 100~150 bytes, depending on key and value length.
// The write batch will be around 32KB if we scan 256 keys each time.
pub const RESOLVE_LOCK_BATCH_SIZE: usize = 256;

/// Task is a running command.
pub struct Task {
    pub cid: u64,
    pub tag: metrics::CommandKind,

    cmd: Command,
    ts: TimeStamp,
    region_id: u64,
}

impl Task {
    /// Creates a task for a running command.
    pub fn new(cid: u64, cmd: Command) -> Task {
        Task {
            cid,
            tag: cmd.tag(),
            region_id: cmd.ctx.get_region_id(),
            ts: cmd.ts(),
            cmd,
        }
    }

    pub fn cmd(&self) -> &Command {
        &self.cmd
    }

    pub fn priority(&self) -> CommandPri {
        self.cmd.priority()
    }

    pub fn context(&self) -> &Context {
        &self.cmd.ctx
    }
}

pub trait MsgScheduler: Clone + Send + 'static {
    fn on_msg(&self, task: Msg);
}

pub struct Executor<E: Engine, S: MsgScheduler, L: LockManager> {
    // We put time consuming tasks to the thread pool.
    sched_pool: Option<SchedPool>,
    // And the tasks completes we post a completion to the `Scheduler`.
    scheduler: Option<S>,
    // If the task releases some locks, we wake up waiters waiting for them.
    lock_mgr: Option<L>,

    _phantom: PhantomData<E>,
}

impl<E: Engine, S: MsgScheduler, L: LockManager> Executor<E, S, L> {
    pub fn new(scheduler: S, pool: SchedPool, lock_mgr: Option<L>) -> Self {
        Executor {
            sched_pool: Some(pool),
            scheduler: Some(scheduler),
            lock_mgr,
            _phantom: Default::default(),
        }
    }

    fn take_pool(&mut self) -> SchedPool {
        self.sched_pool.take().unwrap()
    }

    fn clone_pool(&mut self) -> SchedPool {
        self.sched_pool.clone().unwrap()
    }

    fn take_scheduler(&mut self) -> S {
        self.scheduler.take().unwrap()
    }

    fn take_lock_mgr(&mut self) -> Option<L> {
        self.lock_mgr.take()
    }

    /// Start the execution of the task.
    pub fn execute(mut self, cb_ctx: CbContext, snapshot: EngineResult<E::Snap>, task: Task) {
        debug!(
            "receive snapshot finish msg";
            "cid" => task.cid, "cb_ctx" => ?cb_ctx
        );

        match snapshot {
            Ok(snapshot) => {
                SCHED_STAGE_COUNTER_VEC.get(task.tag).snapshot_ok.inc();

                self.process_by_worker(cb_ctx, snapshot, task);
            }
            Err(err) => {
                SCHED_STAGE_COUNTER_VEC.get(task.tag).snapshot_err.inc();

                info!("get snapshot failed"; "cid" => task.cid, "err" => ?err);
                self.take_pool()
                    .pool
                    .spawn(move || {
                        notify_scheduler(
                            self.take_scheduler(),
                            Msg::FinishedWithErr {
                                cid: task.cid,
                                err: Error::from(err),
                                tag: task.tag,
                            },
                        );
                        future::ok::<_, ()>(())
                    })
                    .unwrap();
            }
        }
    }

    /// Delivers a command to a worker thread for processing.
    fn process_by_worker(mut self, cb_ctx: CbContext, snapshot: E::Snap, mut task: Task) {
        SCHED_STAGE_COUNTER_VEC.get(task.tag).process.inc();
        debug!(
            "process cmd with snapshot";
            "cid" => task.cid, "cb_ctx" => ?cb_ctx
        );
        let tag = task.tag;
        if let Some(term) = cb_ctx.term {
            task.cmd.ctx.set_term(term);
        }
        let sched_pool = self.clone_pool();
        let readonly = task.cmd.readonly();
        sched_pool
            .pool
            .spawn(move || {
                fail_point!("scheduler_async_snapshot_finish");

                let read_duration = Instant::now_coarse();

                let region_id = task.region_id;
                let ts = task.ts;
                let timer = SlowTimer::new();

                let statistics = if readonly {
                    self.process_read(snapshot, task)
                } else {
                    // Safety: `self.sched_pool` ensures a TLS engine exists.
                    unsafe { with_tls_engine(|engine| self.process_write(engine, snapshot, task)) }
                };
                tls_collect_scan_details(tag.get_str(), &statistics);
                slow_log!(
                    timer,
                    "[region {}] scheduler handle command: {}, ts: {}",
                    region_id,
                    tag,
                    ts
                );

                tls_collect_read_duration(tag.get_str(), read_duration.elapsed());
                future::ok::<_, ()>(())
            })
            .unwrap();
    }

    /// Processes a read command within a worker thread, then posts `ReadFinished` message back to the
    /// `Scheduler`.
    fn process_read(mut self, snapshot: E::Snap, task: Task) -> Statistics {
        fail_point!("txn_before_process_read");
        debug!("process read cmd in worker pool"; "cid" => task.cid);
        let tag = task.tag;
        let cid = task.cid;
        let mut statistics = Statistics::default();
        let pr = match process_read_impl::<E>(task.cmd, snapshot, &mut statistics) {
            Err(e) => ProcessResult::Failed { err: e.into() },
            Ok(pr) => pr,
        };
        notify_scheduler(self.take_scheduler(), Msg::ReadFinished { cid, pr, tag });
        statistics
    }

    /// Processes a write command within a worker thread, then posts either a `WriteFinished`
    /// message if successful or a `FinishedWithErr` message back to the `Scheduler`.
    fn process_write(mut self, engine: &E, snapshot: E::Snap, task: Task) -> Statistics {
        fail_point!("txn_before_process_write");
        let tag = task.tag;
        let cid = task.cid;
        let ts = task.ts;
        let mut statistics = Statistics::default();
        let scheduler = self.take_scheduler();
        let lock_mgr = self.take_lock_mgr();
        let msg = match process_write_impl(task.cmd, snapshot, lock_mgr, &mut statistics) {
            // Initiates an async write operation on the storage engine, there'll be a `WriteFinished`
            // message when it finishes.
            Ok(WriteResult {
                ctx,
                to_be_write,
                rows,
                pr,
                lock_info,
            }) => {
                SCHED_STAGE_COUNTER_VEC.get(tag).write.inc();

                if let Some(lock_info) = lock_info {
                    let (lock, is_first_lock, wait_timeout) = lock_info;
                    Msg::WaitForLock {
                        cid,
                        start_ts: ts,
                        pr,
                        lock,
                        is_first_lock,
                        wait_timeout,
                    }
                } else if to_be_write.is_empty() {
                    Msg::WriteFinished {
                        cid,
                        pr,
                        result: Ok(()),
                        tag,
                    }
                } else {
                    let sched = scheduler.clone();
                    let sched_pool = self.take_pool();
                    // The callback to receive async results of write prepare from the storage engine.
                    let engine_cb = Box::new(move |(_, result)| {
                        sched_pool
                            .pool
                            .spawn(move || {
                                notify_scheduler(
                                    sched,
                                    Msg::WriteFinished {
                                        cid,
                                        pr,
                                        result,
                                        tag,
                                    },
                                );
                                KV_COMMAND_KEYWRITE_HISTOGRAM_VEC
                                    .get(tag)
                                    .observe(rows as f64);
                                future::ok::<_, ()>(())
                            })
                            .unwrap()
                    });

                    if let Err(e) = engine.async_write(&ctx, to_be_write, engine_cb) {
                        SCHED_STAGE_COUNTER_VEC.get(tag).async_write_err.inc();

                        info!("engine async_write failed"; "cid" => cid, "err" => ?e);
                        let err = e.into();
                        Msg::FinishedWithErr { cid, err, tag }
                    } else {
                        return statistics;
                    }
                }
            }
            // Write prepare failure typically means conflicting transactions are detected. Delivers the
            // error to the callback, and releases the latches.
            Err(err) => {
                SCHED_STAGE_COUNTER_VEC.get(tag).prepare_write_err.inc();

                debug!("write command failed at prewrite"; "cid" => cid);
                Msg::FinishedWithErr { cid, err, tag }
            }
        };
        notify_scheduler(scheduler, msg);
        statistics
    }
}

fn process_read_impl<E: Engine>(
    mut cmd: Command,
    snapshot: E::Snap,
    statistics: &mut Statistics,
) -> Result<ProcessResult> {
    let tag = cmd.tag();
    match cmd.kind {
        CommandKind::MvccByKey { ref key } => {
            let mut reader = MvccReader::new(
                snapshot,
                Some(ScanMode::Forward),
                !cmd.ctx.get_not_fill_cache(),
                cmd.ctx.get_isolation_level(),
            );
            let result = find_mvcc_infos_by_key(&mut reader, key, TimeStamp::max());
            statistics.add(reader.get_statistics());
            let (lock, writes, values) = result?;
            Ok(ProcessResult::MvccKey {
                mvcc: MvccInfo {
                    lock,
                    writes,
                    values,
                },
            })
        }
        CommandKind::MvccByStartTs { start_ts } => {
            let mut reader = MvccReader::new(
                snapshot,
                Some(ScanMode::Forward),
                !cmd.ctx.get_not_fill_cache(),
                cmd.ctx.get_isolation_level(),
            );
            match reader.seek_ts(start_ts)? {
                Some(key) => {
                    let result = find_mvcc_infos_by_key(&mut reader, &key, TimeStamp::max());
                    statistics.add(reader.get_statistics());
                    let (lock, writes, values) = result?;
                    Ok(ProcessResult::MvccStartTs {
                        mvcc: Some((
                            key,
                            MvccInfo {
                                lock,
                                writes,
                                values,
                            },
                        )),
                    })
                }
                None => Ok(ProcessResult::MvccStartTs { mvcc: None }),
            }
        }
        // Scans locks with timestamp <= `max_ts`
        CommandKind::ScanLock {
            max_ts,
            ref start_key,
            limit,
            ..
        } => {
            let mut reader = MvccReader::new(
                snapshot,
                Some(ScanMode::Forward),
                !cmd.ctx.get_not_fill_cache(),
                cmd.ctx.get_isolation_level(),
            );
            let result = reader.scan_locks(start_key.as_ref(), |lock| lock.ts <= max_ts, limit);
            statistics.add(reader.get_statistics());
            let (kv_pairs, _) = result?;
            let mut locks = Vec::with_capacity(kv_pairs.len());
            for (key, lock) in kv_pairs {
                let mut lock_info = LockInfo::default();
                lock_info.set_primary_lock(lock.primary);
                lock_info.set_lock_version(lock.ts.into_inner());
                lock_info.set_key(key.into_raw()?);
                lock_info.set_lock_ttl(lock.ttl);
                lock_info.set_txn_size(lock.txn_size);
                locks.push(lock_info);
            }

            tls_collect_keyread_histogram_vec(tag.get_str(), locks.len() as f64);

            Ok(ProcessResult::Locks { locks })
        }
        CommandKind::ResolveLock {
            ref mut txn_status,
            ref scan_key,
            ..
        } => {
            let mut reader = MvccReader::new(
                snapshot,
                Some(ScanMode::Forward),
                !cmd.ctx.get_not_fill_cache(),
                cmd.ctx.get_isolation_level(),
            );
            let result = reader.scan_locks(
                scan_key.as_ref(),
                |lock| txn_status.contains_key(&lock.ts),
                RESOLVE_LOCK_BATCH_SIZE,
            );
            statistics.add(reader.get_statistics());
            let (kv_pairs, has_remain) = result?;
            tls_collect_keyread_histogram_vec(tag.get_str(), kv_pairs.len() as f64);

            if kv_pairs.is_empty() {
                Ok(ProcessResult::Res)
            } else {
                let next_scan_key = if has_remain {
                    // There might be more locks.
                    kv_pairs.last().map(|(k, _lock)| k.clone())
                } else {
                    // All locks are scanned
                    None
                };
                Ok(ProcessResult::NextCommand {
                    cmd: Command {
                        ctx: cmd.ctx.clone(),
                        kind: CommandKind::ResolveLock {
                            txn_status: mem::replace(txn_status, Default::default()),
                            scan_key: next_scan_key,
                            key_locks: kv_pairs,
                        },
                    },
                })
            }
        }
        _ => panic!("unsupported read command"),
    }
}

// If lock_mgr has waiters, there may be some transactions waiting for these keys,
// so calculates keys' hashes to wake up them.
fn gen_key_hashes_if_needed<L: LockManager, K: Borrow<Key>>(
    lock_mgr: &Option<L>,
    keys: &[K],
) -> Option<Vec<u64>> {
    lock_mgr.as_ref().and_then(|lm| {
        if lm.has_waiter() {
            Some(keys.iter().map(|key| key.borrow().gen_hash()).collect())
        } else {
            None
        }
    })
}

// Wake up pessimistic transactions that waiting for these locks
fn wake_up_waiters_if_needed<L: LockManager>(
    lock_mgr: &Option<L>,
    lock_ts: TimeStamp,
    key_hashes: Option<Vec<u64>>,
    commit_ts: TimeStamp,
    is_pessimistic_txn: bool,
) {
    if let Some(lm) = lock_mgr {
        lm.wake_up(lock_ts, key_hashes, commit_ts, is_pessimistic_txn);
    }
}

fn extract_lock_from_result(res: &StorageResult<()>) -> Lock {
    match res {
        Err(StorageError(box StorageErrorInner::Txn(Error(box ErrorInner::Mvcc(MvccError(
            box MvccErrorInner::KeyIsLocked(info),
        )))))) => Lock {
            ts: info.get_lock_version().into(),
            hash: Key::from_raw(info.get_key()).gen_hash(),
        },
        _ => panic!("unexpected mvcc error"),
    }
}

struct WriteResult {
    ctx: Context,
    to_be_write: Vec<Modify>,
    rows: usize,
    pr: ProcessResult,
    // (lock, is_first_lock, wait_timeout)
    lock_info: Option<(lock_manager::Lock, bool, i64)>,
}

fn process_write_impl<S: Snapshot, L: LockManager>(
    cmd: Command,
    snapshot: S,
    lock_mgr: Option<L>,
    statistics: &mut Statistics,
) -> Result<WriteResult> {
    let (pr, to_be_write, rows, ctx, lock_info) = match cmd.kind {
        CommandKind::Prewrite {
            mut mutations,
            primary,
            start_ts,
            mut options,
            ..
        } => {
            let mut scan_mode = None;
            let rows = mutations.len();
            if options.for_update_ts.is_zero() && rows > FORWARD_MIN_MUTATIONS_NUM {
                mutations.sort_by(|a, b| a.key().cmp(b.key()));
                let left_key = mutations.first().unwrap().key();
                let right_key = mutations
                    .last()
                    .unwrap()
                    .key()
                    .clone()
                    .append_ts(TimeStamp::zero());
                if !has_data_in_range(
                    snapshot.clone(),
                    CF_WRITE,
                    left_key,
                    &right_key,
                    &mut statistics.write,
                )? {
                    // If there is no data in range, we could skip constraint check, and use Forward seek for CF_LOCK.
                    // Because in most instances, there won't be more than one transaction write the same key. Seek
                    // operation could skip nonexistent key in CF_LOCK.
                    options.skip_constraint_check = true;
                    scan_mode = Some(ScanMode::Forward)
                }
            }
            let mut locks = vec![];
            let mut txn = if scan_mode.is_some() {
                MvccTxn::for_scan(snapshot, scan_mode, start_ts, !cmd.ctx.get_not_fill_cache())?
            } else {
                MvccTxn::new(snapshot, start_ts, !cmd.ctx.get_not_fill_cache())?
            };

            // If `options.for_update_ts` is 0, the transaction is optimistic
            // or else pessimistic.
            if options.for_update_ts.is_zero() {
                for m in mutations {
                    match txn.prewrite(m, &primary, &options) {
                        Ok(_) => {}
                        e @ Err(MvccError(box MvccErrorInner::KeyIsLocked { .. })) => {
                            locks.push(e.map_err(Error::from).map_err(StorageError::from));
                        }
                        Err(e) => return Err(Error::from(e)),
                    }
                }
            } else {
                for (i, m) in mutations.into_iter().enumerate() {
                    match txn.pessimistic_prewrite(
                        m,
                        &primary,
                        options.is_pessimistic_lock[i],
                        &options,
                    ) {
                        Ok(_) => {}
                        e @ Err(MvccError(box MvccErrorInner::KeyIsLocked { .. })) => {
                            locks.push(e.map_err(Error::from).map_err(StorageError::from));
                        }
                        Err(e) => return Err(Error::from(e)),
                    }
                }
            }

            statistics.add(&txn.take_statistics());
            if locks.is_empty() {
                let pr = ProcessResult::MultiRes { results: vec![] };
                let modifies = txn.into_modifies();
                (pr, modifies, rows, cmd.ctx, None)
            } else {
                // Skip write stage if some keys are locked.
                let pr = ProcessResult::MultiRes { results: locks };
                (pr, vec![], 0, cmd.ctx, None)
            }
        }
        CommandKind::AcquirePessimisticLock {
            keys,
            primary,
            start_ts,
            options,
            ..
        } => {
            let mut txn = MvccTxn::new(snapshot, start_ts, !cmd.ctx.get_not_fill_cache())?;
            let mut locks = vec![];
            let rows = keys.len();
            for (k, should_not_exist) in keys {
                match txn.acquire_pessimistic_lock(k, &primary, should_not_exist, &options) {
                    Ok(_) => {}
                    e @ Err(MvccError(box MvccErrorInner::KeyIsLocked { .. })) => {
                        locks.push(e.map_err(Error::from).map_err(StorageError::from));
                        break;
                    }
                    Err(e) => return Err(Error::from(e)),
                }
            }

            statistics.add(&txn.take_statistics());
            // no conflict
            if locks.is_empty() {
                let pr = ProcessResult::MultiRes { results: vec![] };
                let modifies = txn.into_modifies();
                (pr, modifies, rows, cmd.ctx, None)
            } else {
                let lock = extract_lock_from_result(&locks[0]);
                let pr = ProcessResult::MultiRes { results: locks };
                let lock_info = Some((lock, options.is_first_lock, options.wait_timeout));
                // Wait for lock released
                (pr, vec![], 0, cmd.ctx, lock_info)
            }
        }
        CommandKind::Commit {
            keys,
            lock_ts,
            commit_ts,
            ..
        } => {
            if commit_ts <= lock_ts {
                return Err(Error::from(ErrorInner::InvalidTxnTso {
                    start_ts: lock_ts,
                    commit_ts,
                }));
            }
            // Pessimistic txn needs key_hashes to wake up waiters
            let key_hashes = gen_key_hashes_if_needed(&lock_mgr, &keys);

            let mut txn = MvccTxn::new(snapshot, lock_ts, !cmd.ctx.get_not_fill_cache())?;
            let mut is_pessimistic_txn = false;
            let rows = keys.len();
            for k in keys {
                is_pessimistic_txn = txn.commit(k, commit_ts)?;
            }

            wake_up_waiters_if_needed(
                &lock_mgr,
                lock_ts,
                key_hashes,
                commit_ts,
                is_pessimistic_txn,
            );
            statistics.add(&txn.take_statistics());
            let pr = ProcessResult::TxnStatus {
                txn_status: TxnStatus::committed(commit_ts),
            };
            (pr, txn.into_modifies(), rows, cmd.ctx, None)
        }
        CommandKind::Cleanup {
            key,
            start_ts,
            current_ts,
            ..
        } => {
            let mut keys = vec![key];
            let key_hashes = gen_key_hashes_if_needed(&lock_mgr, &keys);

            let mut txn = MvccTxn::new(snapshot, start_ts, !cmd.ctx.get_not_fill_cache())?;
            let is_pessimistic_txn = txn.cleanup(keys.pop().unwrap(), current_ts)?;

            wake_up_waiters_if_needed(
                &lock_mgr,
                start_ts,
                key_hashes,
                TimeStamp::zero(),
                is_pessimistic_txn,
            );
            statistics.add(&txn.take_statistics());
            (ProcessResult::Res, txn.into_modifies(), 1, cmd.ctx, None)
        }
        CommandKind::Rollback { keys, start_ts, .. } => {
            let key_hashes = gen_key_hashes_if_needed(&lock_mgr, &keys);

            let mut txn = MvccTxn::new(snapshot, start_ts, !cmd.ctx.get_not_fill_cache())?;
            let mut is_pessimistic_txn = false;
            let rows = keys.len();
            for k in keys {
                is_pessimistic_txn = txn.rollback(k)?;
            }

            wake_up_waiters_if_needed(
                &lock_mgr,
                start_ts,
                key_hashes,
                TimeStamp::zero(),
                is_pessimistic_txn,
            );
            statistics.add(&txn.take_statistics());
            (ProcessResult::Res, txn.into_modifies(), rows, cmd.ctx, None)
        }
        CommandKind::PessimisticRollback {
            keys,
            start_ts,
            for_update_ts,
        } => {
            assert!(lock_mgr.is_some());
            let key_hashes = gen_key_hashes_if_needed(&lock_mgr, &keys);

            let mut txn = MvccTxn::new(snapshot, start_ts, !cmd.ctx.get_not_fill_cache())?;
            let rows = keys.len();
            for k in keys {
                txn.pessimistic_rollback(k, for_update_ts)?;
            }

            wake_up_waiters_if_needed(&lock_mgr, start_ts, key_hashes, TimeStamp::zero(), true);
            statistics.add(&txn.take_statistics());
            (
                ProcessResult::MultiRes { results: vec![] },
                txn.into_modifies(),
                rows,
                cmd.ctx,
                None,
            )
        }
        CommandKind::ResolveLock {
            txn_status,
            mut scan_key,
            key_locks,
        } => {
            // Map (txn's start_ts, is_pessimistic_txn) => Option<key_hashes>
            let (mut txn_to_keys, has_waiter) = if let Some(lm) = lock_mgr.as_ref() {
                (Some(HashMap::default()), lm.has_waiter())
            } else {
                (None, false)
            };

            let mut scan_key = scan_key.take();
            let mut modifies: Vec<Modify> = vec![];
            let mut write_size = 0;
            let rows = key_locks.len();
            for (current_key, current_lock) in key_locks {
                if let Some(txn_to_keys) = txn_to_keys.as_mut() {
                    txn_to_keys
                        .entry((current_lock.ts, !current_lock.for_update_ts.is_zero()))
                        .and_modify(|key_hashes: &mut Option<Vec<u64>>| {
                            if let Some(key_hashes) = key_hashes {
                                key_hashes.push(current_key.gen_hash());
                            }
                        })
                        .or_insert_with(|| {
                            if has_waiter {
                                Some(vec![current_key.gen_hash()])
                            } else {
                                None
                            }
                        });
                }

                let mut txn = MvccTxn::new(
                    snapshot.clone(),
                    current_lock.ts,
                    !cmd.ctx.get_not_fill_cache(),
                )?;
                let status = txn_status.get(&current_lock.ts);
                let commit_ts = match status {
                    Some(ts) => *ts,
                    None => panic!("txn status {} not found.", current_lock.ts),
                };
                if !commit_ts.is_zero() {
                    if current_lock.ts >= commit_ts {
                        return Err(Error::from(ErrorInner::InvalidTxnTso {
                            start_ts: current_lock.ts,
                            commit_ts,
                        }));
                    }
                    txn.commit(current_key.clone(), commit_ts)?;
                } else {
                    txn.rollback(current_key.clone())?;
                }
                write_size += txn.write_size();

                statistics.add(&txn.take_statistics());
                modifies.append(&mut txn.into_modifies());

                if write_size >= MAX_TXN_WRITE_SIZE {
                    scan_key = Some(current_key);
                    break;
                }
            }
            if let Some(txn_to_keys) = txn_to_keys {
                txn_to_keys
                    .into_iter()
                    .for_each(|((ts, is_pessimistic_txn), key_hashes)| {
                        wake_up_waiters_if_needed(
                            &lock_mgr,
                            ts,
                            key_hashes,
                            TimeStamp::zero(),
                            is_pessimistic_txn,
                        );
                    });
            }

            let pr = if scan_key.is_none() {
                ProcessResult::Res
            } else {
                ProcessResult::NextCommand {
                    cmd: Command {
                        ctx: cmd.ctx.clone(),
                        kind: CommandKind::ResolveLock {
                            txn_status,
                            scan_key: scan_key.take(),
                            key_locks: vec![],
                        },
                    },
                }
            };

            (pr, modifies, rows, cmd.ctx, None)
        }
        CommandKind::ResolveLockLite {
            start_ts,
            commit_ts,
            resolve_keys,
        } => {
            let key_hashes = gen_key_hashes_if_needed(&lock_mgr, &resolve_keys);

            let mut txn = MvccTxn::new(snapshot.clone(), start_ts, !cmd.ctx.get_not_fill_cache())?;
            let rows = resolve_keys.len();
            let mut is_pessimistic_txn = false;
            // ti-client guarantees the size of resolve_keys will not too large, so no necessary
            // to control the write_size as ResolveLock.
            for key in resolve_keys {
                if !commit_ts.is_zero() {
                    is_pessimistic_txn = txn.commit(key, commit_ts)?;
                } else {
                    is_pessimistic_txn = txn.rollback(key)?;
                }
            }

            wake_up_waiters_if_needed(
                &lock_mgr,
                start_ts,
                key_hashes,
                commit_ts,
                is_pessimistic_txn,
            );
            statistics.add(&txn.take_statistics());
            (ProcessResult::Res, txn.into_modifies(), rows, cmd.ctx, None)
        }
        CommandKind::TxnHeartBeat {
            primary_key,
            start_ts,
            advise_ttl,
        } => {
            // TxnHeartBeat never remove locks. No need to wake up waiters.
            let mut txn = MvccTxn::new(snapshot.clone(), start_ts, !cmd.ctx.get_not_fill_cache())?;
            let lock_ttl = txn.txn_heart_beat(primary_key, advise_ttl)?;

            statistics.add(&txn.take_statistics());
            let pr = ProcessResult::TxnStatus {
                txn_status: TxnStatus::uncommitted(lock_ttl, TimeStamp::zero()),
            };
            (pr, txn.into_modifies(), 1, cmd.ctx, None)
        }
        CommandKind::CheckTxnStatus {
            primary_key,
            lock_ts,
            caller_start_ts,
            current_ts,
            rollback_if_not_exist,
        } => {
            let mut txn = MvccTxn::new(snapshot.clone(), lock_ts, !cmd.ctx.get_not_fill_cache())?;
            let (txn_status, is_pessimistic_txn) = txn.check_txn_status(
                primary_key.clone(),
                caller_start_ts,
                current_ts,
                rollback_if_not_exist,
            )?;

            // The lock is possibly resolved here only when the `check_txn_status` cleaned up the
            // lock, and this may happen only when it returns `TtlExpire` or `LockNotExist`.
            match txn_status {
                TxnStatus::TtlExpire | TxnStatus::LockNotExist => {
                    let key_hashes = gen_key_hashes_if_needed(&lock_mgr, &[&primary_key]);
                    wake_up_waiters_if_needed(
                        &lock_mgr,
                        lock_ts,
                        key_hashes,
                        TimeStamp::zero(),
                        is_pessimistic_txn,
                    );
                }
                TxnStatus::RolledBack
                | TxnStatus::Committed { .. }
                | TxnStatus::Uncommitted { .. } => {}
            };

            statistics.add(&txn.take_statistics());
            let pr = ProcessResult::TxnStatus { txn_status };
            (pr, txn.into_modifies(), 1, cmd.ctx, None)
        }
        CommandKind::Pause { duration, .. } => {
            thread::sleep(Duration::from_millis(duration));
            (ProcessResult::Res, vec![], 0, cmd.ctx, None)
        }
        _ => panic!("unsupported write command"),
    };

    Ok(WriteResult {
        ctx,
        to_be_write,
        rows,
        pr,
        lock_info,
    })
}

pub fn notify_scheduler<S: MsgScheduler>(scheduler: S, msg: Msg) {
    scheduler.on_msg(msg);
}

type LockWritesVals = (
    Option<MvccLock>,
    Vec<(TimeStamp, Write)>,
    Vec<(TimeStamp, Value)>,
);

fn find_mvcc_infos_by_key<S: Snapshot>(
    reader: &mut MvccReader<S>,
    key: &Key,
    mut ts: TimeStamp,
) -> Result<LockWritesVals> {
    let mut writes = vec![];
    let mut values = vec![];
    let lock = reader.load_lock(key)?;
    loop {
        let opt = reader.seek_write(key, ts)?;
        match opt {
            Some((commit_ts, write)) => {
                ts = commit_ts.prev();
                writes.push((commit_ts, write));
            }
            None => break,
        };
    }
    for (ts, v) in reader.scan_values_in_default(key)? {
        values.push((ts, v));
    }
    Ok((lock, writes, values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::kv::{Snapshot, TestEngineBuilder};
    use crate::storage::{DummyLockManager, Mutation, Options};

    #[test]
    fn test_extract_lock_from_result() {
        let raw_key = b"key".to_vec();
        let key = Key::from_raw(&raw_key);
        let ts = 100;
        let mut info = LockInfo::default();
        info.set_key(raw_key);
        info.set_lock_version(ts);
        info.set_lock_ttl(100);
        let case = StorageError::from(StorageErrorInner::Txn(Error::from(ErrorInner::Mvcc(
            MvccError::from(MvccErrorInner::KeyIsLocked(info)),
        ))));
        let lock = extract_lock_from_result(&Err(case));
        assert_eq!(lock.ts, ts.into());
        assert_eq!(lock.hash, key.gen_hash());
    }

    fn inner_test_prewrite_skip_constraint_check(pri_key_number: u8, write_num: usize) {
        let mut mutations = Vec::default();
        let pri_key = &[pri_key_number];
        for i in 0..write_num {
            mutations.push(Mutation::Insert((
                Key::from_raw(&[i as u8]),
                b"100".to_vec(),
            )));
        }
        let mut statistic = Statistics::default();
        let engine = TestEngineBuilder::new().build().unwrap();
        prewrite(
            &engine,
            &mut statistic,
            vec![Mutation::Put((
                Key::from_raw(&[pri_key_number]),
                b"100".to_vec(),
            ))],
            pri_key.to_vec(),
            99,
        )
        .unwrap();
        assert_eq!(1, statistic.write.seek);
        let e = prewrite(
            &engine,
            &mut statistic,
            mutations.clone(),
            pri_key.to_vec(),
            100,
        )
        .err()
        .unwrap();
        assert_eq!(2, statistic.write.seek);
        match e {
            Error(box ErrorInner::Mvcc(MvccError(box MvccErrorInner::KeyIsLocked(_)))) => (),
            _ => panic!("error type not match"),
        }
        commit(
            &engine,
            &mut statistic,
            vec![Key::from_raw(&[pri_key_number])],
            99,
            102,
        )
        .unwrap();
        assert_eq!(2, statistic.write.seek);
        let e = prewrite(
            &engine,
            &mut statistic,
            mutations.clone(),
            pri_key.to_vec(),
            101,
        )
        .err()
        .unwrap();
        match e {
            Error(box ErrorInner::Mvcc(MvccError(box MvccErrorInner::WriteConflict {
                ..
            }))) => (),
            _ => panic!("error type not match"),
        }
        let e = prewrite(
            &engine,
            &mut statistic,
            mutations.clone(),
            pri_key.to_vec(),
            104,
        )
        .err()
        .unwrap();
        match e {
            Error(box ErrorInner::Mvcc(MvccError(box MvccErrorInner::AlreadyExist { .. }))) => (),
            _ => panic!("error type not match"),
        }

        statistic.write.seek = 0;
        let ctx = Context::default();
        engine
            .delete_cf(
                &ctx,
                CF_WRITE,
                Key::from_raw(&[pri_key_number]).append_ts(102.into()),
            )
            .unwrap();
        prewrite(
            &engine,
            &mut statistic,
            mutations.clone(),
            pri_key.to_vec(),
            104,
        )
        .unwrap();
        // All keys are prewrited successful with only one seek operations.
        assert_eq!(1, statistic.write.seek);
        let keys: Vec<Key> = mutations.iter().map(|m| m.key().clone()).collect();
        commit(&engine, &mut statistic, keys.clone(), 104, 105).unwrap();
        let snap = engine.snapshot(&ctx).unwrap();
        for k in keys {
            let v = snap.get_cf(CF_WRITE, &k.append_ts(105.into())).unwrap();
            assert!(v.is_some());
        }
    }

    #[test]
    fn test_prewrite_skip_constraint_check() {
        inner_test_prewrite_skip_constraint_check(0, FORWARD_MIN_MUTATIONS_NUM + 1);
        inner_test_prewrite_skip_constraint_check(5, FORWARD_MIN_MUTATIONS_NUM + 1);
        inner_test_prewrite_skip_constraint_check(
            FORWARD_MIN_MUTATIONS_NUM as u8,
            FORWARD_MIN_MUTATIONS_NUM + 1,
        );
    }

    fn prewrite<E: Engine>(
        engine: &E,
        statistics: &mut Statistics,
        mutations: Vec<Mutation>,
        primary: Vec<u8>,
        start_ts: u64,
    ) -> Result<()> {
        let ctx = Context::default();
        let snap = engine.snapshot(&ctx)?;
        let cmd = Command {
            ctx,
            kind: CommandKind::Prewrite {
                mutations,
                primary,
                start_ts: TimeStamp::from(start_ts),
                options: Options::default(),
            },
        };
        let m = DummyLockManager {};
        let ret = process_write_impl(cmd, snap, Some(m), statistics)?;
        if let ProcessResult::MultiRes { results } = ret.pr {
            if !results.is_empty() {
                let info = LockInfo::default();
                return Err(Error::from(ErrorInner::Mvcc(MvccError::from(
                    MvccErrorInner::KeyIsLocked(info),
                ))));
            }
        }
        let ctx = Context::default();
        engine.write(&ctx, ret.to_be_write).unwrap();
        Ok(())
    }

    fn commit<E: Engine>(
        engine: &E,
        statistics: &mut Statistics,
        keys: Vec<Key>,
        lock_ts: u64,
        commit_ts: u64,
    ) -> Result<()> {
        let ctx = Context::default();
        let snap = engine.snapshot(&ctx)?;
        let cmd = Command {
            ctx,
            kind: CommandKind::Commit {
                keys,
                lock_ts: TimeStamp::from(lock_ts),
                commit_ts: TimeStamp::from(commit_ts),
            },
        };
        let m = DummyLockManager {};
        let ret = process_write_impl(cmd, snap, Some(m), statistics)?;
        let ctx = Context::default();
        engine.write(&ctx, ret.to_be_write).unwrap();
        Ok(())
    }
}
