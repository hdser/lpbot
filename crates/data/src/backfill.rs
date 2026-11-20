//! Backfill: a pool's history from a node into the event store.
//!
//! For every UTC day from a start day to the head it finds the day's block
//! range, reads every log the pool emitted in it and writes one file. A day
//! that is already stored complete is skipped, so a re-run only fetches what
//! is new. The day in progress is stored as partial and replaced by the
//! next run.
//!
//! A replay needs the pool's state before its first stored day: a
//! snapshot. The backfill reuses the newest stored snapshot on or before
//! the start day when every day from it to the start is stored, and takes
//! a new one only when none is usable. A pool created after the start day
//! has no state there; its history starts on the first whole day after
//! its creation.

use std::collections::BTreeMap;

use alloy_primitives::Address;
use lpbot_chain::eth::Log;
use lpbot_chain::uniswap_v3::{fetch_snapshot, ChainError};
use lpbot_chain::RpcClient;
use lpbot_venues::uniswap_v3::pool::{Pool, PoolSnapshot, Replay};

use crate::day::Day;
use crate::lake::read_events;
use crate::snapshot::{SnapshotStore, StoredSnapshot};
use crate::store::{DayState, EventStore, StoreError, StoredLog};

/// Where the backfill reads the chain from. The node in production, a
/// scripted chain in tests.
pub trait ChainSource {
    /// Number and time of the newest block that is safe to store.
    fn head(&self) -> Result<(u64, u64), ChainError>;
    fn block_time(&self, block: u64) -> Result<u64, ChainError>;
    /// Times of several blocks, in the order asked.
    fn block_times(&self, blocks: &[u64]) -> Result<Vec<u64>, ChainError>;
    /// Every log of the pool in a block range, in chain order.
    fn logs(&self, pool: Address, from_block: u64, to_block: u64) -> Result<Vec<Log>, ChainError>;
    /// Every log of several pools in a block range, in chain order. A node
    /// answers it with one request that reads each block once, however
    /// many pools it asks about. This default asks pool by pool.
    fn logs_many(
        &self,
        pools: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>, ChainError> {
        let mut logs = Vec::new();
        for pool in pools {
            logs.extend(self.logs(*pool, from_block, to_block)?);
        }
        logs.sort_by_key(|l| (l.block_number, l.log_index));
        Ok(logs)
    }
    /// The pool's full state after a block.
    fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError>;
    /// Whether a contract is deployed at `address` after `block`. A pool
    /// that the factory created later has no code there yet. A source
    /// that cannot tell says yes, and a missing pool then shows up as a
    /// snapshot that cannot be read.
    fn has_code(&self, _address: Address, _block: u64) -> Result<bool, ChainError> {
        Ok(true)
    }
}

/// A node as the source, a few blocks behind its head so a reorg cannot
/// change what was stored.
pub struct NodeSource<'a> {
    pub client: &'a RpcClient,
    pub head_margin: u64,
    /// Blocks per log request. Shrinks by itself when the node refuses.
    pub log_chunk: u64,
}

impl ChainSource for NodeSource<'_> {
    fn head(&self) -> Result<(u64, u64), ChainError> {
        let head = self.client.block_number()?.saturating_sub(self.head_margin);
        Ok((head, self.client.block_header(head)?.0))
    }

    fn block_time(&self, block: u64) -> Result<u64, ChainError> {
        Ok(self.client.block_header(block)?.0)
    }

    fn block_times(&self, blocks: &[u64]) -> Result<Vec<u64>, ChainError> {
        Ok(self.client.block_times(blocks)?)
    }

    fn logs(&self, pool: Address, from_block: u64, to_block: u64) -> Result<Vec<Log>, ChainError> {
        Ok(self.client.get_logs_chunked(
            pool,
            &[],
            from_block,
            to_block,
            self.log_chunk,
            |_, _| {},
        )?)
    }

    fn logs_many(
        &self,
        pools: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>, ChainError> {
        Ok(self.client.get_logs_chunked_many(
            pools,
            &[],
            from_block,
            to_block,
            self.log_chunk,
            |_, _| {},
        )?)
    }

    fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
        fetch_snapshot(self.client, pool, block)
    }

    fn has_code(&self, address: Address, block: u64) -> Result<bool, ChainError> {
        Ok(!self.client.code_at(address, block)?.is_empty())
    }
}

/// A source seen at a head fixed for a whole run. Every pool of the run is
/// stored up to the same block, and a check made afterwards compares at
/// that block, however far the chain has moved meanwhile.
pub struct AtHead<'a, S> {
    pub source: &'a S,
    pub block: u64,
    /// Unix time of `block`.
    pub time: u64,
}

impl<S: ChainSource> ChainSource for AtHead<'_, S> {
    fn head(&self) -> Result<(u64, u64), ChainError> {
        Ok((self.block, self.time))
    }

    fn block_time(&self, block: u64) -> Result<u64, ChainError> {
        self.source.block_time(block)
    }

    fn block_times(&self, blocks: &[u64]) -> Result<Vec<u64>, ChainError> {
        self.source.block_times(blocks)
    }

    fn logs(&self, pool: Address, from_block: u64, to_block: u64) -> Result<Vec<Log>, ChainError> {
        self.source.logs(pool, from_block, to_block)
    }

    fn logs_many(
        &self,
        pools: &[Address],
        from_block: u64,
        to_block: u64,
    ) -> Result<Vec<Log>, ChainError> {
        self.source.logs_many(pools, from_block, to_block)
    }

    fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
        self.source.snapshot(pool, block)
    }

    fn has_code(&self, address: Address, block: u64) -> Result<bool, ChainError> {
        self.source.has_code(address, block)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BackfillError {
    #[error(transparent)]
    Chain(#[from] ChainError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(
        "cannot read the pool's state at block {block}: {source}. The pool must exist at that \
         block and the node must still have its state: an archive node has all of it, a pruned \
         node only recent blocks. Pick a later start or use an archive node"
    )]
    Snapshot { block: u64, source: ChainError },
    #[error("{day} starts before the chain's first block; pick a later start")]
    BeforeChain { day: Day },
    #[error("the node's answers do not fit together: {0}")]
    Inconsistent(String),
}

/// The pool to backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolRef {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
}

/// What a backfill did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BackfillReport {
    /// Days written, with their row count and whether they are complete.
    pub written: Vec<(Day, usize, bool)>,
    /// Days that were already stored complete.
    pub skipped: usize,
    /// Block of the snapshot written by this run, if one was needed.
    pub snapshot_block: Option<u64>,
    /// Day and block of the snapshot the stored history replays from:
    /// the one written by this run, or one stored before. `None` when
    /// nothing was stored.
    pub snapshot: Option<(Day, u64)>,
    /// The first day this run brought up to date, which is the start
    /// asked for unless the pool did not exist then.
    pub from: Option<Day>,
    /// The day the pool was created on, when that is after the start
    /// asked for.
    pub created: Option<Day>,
    pub head_block: u64,
    /// Unix time of `head_block`.
    pub head_time: u64,
}

impl BackfillReport {
    pub fn rows_written(&self) -> usize {
        self.written.iter().map(|(_, rows, _)| rows).sum()
    }

    /// What the backfill found about the pool's start and which snapshot
    /// its history replays from, one line each.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(created) = self.created {
            notes.push(match self.from {
                Some(from) => format!(
                    "created on {created}, after the start asked for: its history starts \
                     on {from}, its first whole day"
                ),
                None => format!(
                    "created on {created}, after the start asked for: its history will start \
                     on {}, its first whole day, once that day has begun",
                    created.next()
                ),
            });
        }
        if let (Some((day, block)), Some(from)) = (self.snapshot, self.from) {
            notes.push(if self.snapshot_block.is_some() {
                format!("snapshot of the pool before {day} taken at block {block}")
            } else if day == from {
                format!("using the snapshot stored for {day} (block {block})")
            } else {
                format!(
                    "using the snapshot stored for {day} (block {block}): every day from it \
                     to {from} is stored, so no new snapshot is needed"
                )
            });
        }
        notes
    }
}

/// The first block in `[lo, hi]` whose time is at or after `time`, or
/// `hi + 1` when every block in the range is earlier. Block times must not
/// decrease, which every chain guarantees.
pub fn first_block_at_or_after(
    source: &impl ChainSource,
    time: u64,
    lo: u64,
    hi: u64,
) -> Result<u64, ChainError> {
    if lo > hi {
        return Ok(hi + 1);
    }
    if source.block_time(lo)? >= time {
        return Ok(lo);
    }
    // Gallop forward from `lo` to bracket the answer: the block asked for is
    // usually about a day ahead, far closer to `lo` than to the head.
    let mut below = lo;
    let mut step = 1_024u64;
    let mut above = loop {
        let probe = below.saturating_add(step).min(hi);
        if source.block_time(probe)? >= time {
            break probe;
        }
        if probe == hi {
            return Ok(hi + 1);
        }
        below = probe;
        step = step.saturating_mul(2);
    };
    // `below` is before `time`, `above` is at or after it.
    while above - below > 1 {
        let middle = below + (above - below) / 2;
        if source.block_time(middle)? >= time {
            above = middle;
        } else {
            below = middle;
        }
    }
    Ok(above)
}

/// The first block in `[lo, hi]` after which `address` has code: the
/// block a pool was created in. `None` when it has none even at `hi`.
/// Code, once there, stays: a pool cannot destroy itself.
pub fn first_block_with_code(
    source: &impl ChainSource,
    address: Address,
    lo: u64,
    hi: u64,
) -> Result<Option<u64>, ChainError> {
    if lo > hi || !source.has_code(address, hi)? {
        return Ok(None);
    }
    if source.has_code(address, lo)? {
        return Ok(Some(lo));
    }
    // No code after `without`, code after `with`.
    let (mut without, mut with) = (lo, hi);
    while with - without > 1 {
        let middle = without + (with - without) / 2;
        if source.has_code(address, middle)? {
            with = middle;
        } else {
            without = middle;
        }
    }
    Ok(Some(with))
}

/// The time of every block the logs are in, each checked to fall on `day`.
pub(crate) fn block_times_on(
    source: &impl ChainSource,
    logs: &[Log],
    day: Day,
) -> Result<BTreeMap<u64, u64>, BackfillError> {
    let mut times: BTreeMap<u64, u64> = logs.iter().map(|l| (l.block_number, 0)).collect();
    let blocks: Vec<u64> = times.keys().copied().collect();
    let answers = source.block_times(&blocks)?;
    if answers.len() != blocks.len() {
        return Err(BackfillError::Inconsistent(format!(
            "asked the time of {} blocks, got {}",
            blocks.len(),
            answers.len()
        )));
    }
    for (block, time) in blocks.iter().zip(answers) {
        if Day::of_time(time) != day {
            return Err(BackfillError::Inconsistent(format!(
                "block {block} is dated {}, but it lies in the block range of {day}",
                Day::of_time(time)
            )));
        }
        times.insert(*block, time);
    }
    Ok(times)
}

/// The logs as stored, with the times `block_times_on` found.
pub(crate) fn stored_logs(
    logs: Vec<Log>,
    times: &BTreeMap<u64, u64>,
) -> Result<Vec<StoredLog>, BackfillError> {
    logs.into_iter()
        .map(|log| {
            Ok(StoredLog {
                block_number: log.block_number,
                block_time: times[&log.block_number],
                log_index: u32::try_from(log.log_index).map_err(|_| {
                    BackfillError::Inconsistent(format!(
                        "log index {} in block {} is out of range",
                        log.log_index, log.block_number
                    ))
                })?,
                tx_hash: log.transaction_hash,
                topics: log.topics,
                data: log.data,
            })
        })
        .collect()
}

/// The newest stored snapshot on or before `from` that a replay can roll
/// forward to `from`: every day from its own day to the day before `from`
/// is stored complete.
pub fn usable_snapshot(
    root: &std::path::Path,
    chain: &str,
    pool: Address,
    from: Day,
) -> Result<Option<StoredSnapshot>, StoreError> {
    let Some(snapshot) = SnapshotStore::new(root).latest_on_or_before(chain, pool, from)? else {
        return Ok(None);
    };
    let events = EventStore::new(root);
    let mut day = snapshot.day;
    while day < from {
        if events.day_state(chain, pool, day) != DayState::Complete {
            return Ok(None);
        }
        day = day.next();
    }
    Ok(Some(snapshot))
}

/// One pool's day as the backfill left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolDay {
    pub pool: Address,
    /// Logs written; 0 for a day that was already stored.
    pub rows: usize,
    pub state: DayState,
    /// Whether this run fetched and wrote the day; false when it was
    /// already stored complete.
    pub fetched: bool,
}

/// Brings one pool's stored history up to the head, starting at `from`.
/// `on_day` is told about every day as it is written or skipped.
pub fn backfill_pool(
    source: &impl ChainSource,
    root: &std::path::Path,
    chain: &str,
    pool: PoolRef,
    from: Day,
    mut on_day: impl FnMut(Day, usize, DayState),
) -> Result<BackfillReport, BackfillError> {
    let mut outcomes = backfill_pools(source, root, chain, &[pool], from, |day, pools| {
        for pool in pools {
            on_day(day, pool.rows, pool.state);
        }
    })?;
    outcomes.pop().expect("one outcome for the one pool")
}

/// Brings several pools' stored histories up to the head, starting at
/// `from`, day by day: every day's logs of all the pools that need it come
/// from one request, so the node reads each block once rather than once
/// per pool. `on_day` is told, once per day, about every pool that has
/// the day, written or skipped.
///
/// The outcome of each pool is in the order asked. A pool that cannot
/// start (its state unreadable, say) or whose day cannot be stored fails
/// alone and the others go on; a failure of the node itself, or answers
/// that do not fit together, stops the run, and a re-run carries on from
/// what was stored.
pub fn backfill_pools(
    source: &impl ChainSource,
    root: &std::path::Path,
    chain: &str,
    pools: &[PoolRef],
    from: Day,
    mut on_day: impl FnMut(Day, &[PoolDay]),
) -> Result<Vec<Result<BackfillReport, BackfillError>>, BackfillError> {
    let events = EventStore::new(root);
    let (head, head_time) = source.head()?;
    let empty = BackfillReport {
        head_block: head,
        head_time,
        ..Default::default()
    };
    let last_day = Day::of_time(head_time);
    if from > last_day || pools.is_empty() {
        return Ok(pools.iter().map(|_| Ok(empty.clone())).collect());
    }

    let start = first_block_at_or_after(source, from.start(), 0, head)?;
    let mut outcomes: Vec<Result<BackfillReport, BackfillError>> = pools
        .iter()
        .map(|pool| prepare(source, root, chain, *pool, from, start, empty.clone()))
        .collect();

    // Each pool's first day; pools with nothing to store are left out.
    let firsts: Vec<Option<Day>> = outcomes
        .iter()
        .map(|outcome| outcome.as_ref().ok().and_then(|report| report.from))
        .collect();
    let Some(first) = firsts.iter().flatten().min().copied() else {
        return Ok(outcomes);
    };
    let mut start = if first == from {
        start
    } else {
        first_block_at_or_after(source, first.start(), start, head)?
    };

    let mut day = first;
    while day <= last_day {
        // The next day's first block closes this day. Past the head it does
        // not exist yet, and the day is still in progress.
        let next_start = first_block_at_or_after(source, day.end(), start, head)?;
        let complete = next_start <= head;
        let state = if complete {
            DayState::Complete
        } else {
            DayState::Partial
        };

        let mut seen = Vec::new();
        let mut wanted = Vec::new();
        for (index, pool) in pools.iter().enumerate() {
            let Ok(report) = &mut outcomes[index] else {
                continue;
            };
            if firsts[index].is_none_or(|first| day < first) {
                continue;
            }
            if events.day_state(chain, pool.address, day) == DayState::Complete {
                report.skipped += 1;
                seen.push(PoolDay {
                    pool: pool.address,
                    rows: 0,
                    state: DayState::Complete,
                    fetched: false,
                });
            } else {
                wanted.push(index);
            }
        }

        if !wanted.is_empty() {
            let addresses: Vec<Address> = wanted.iter().map(|&i| pools[i].address).collect();
            let logs = if start < next_start {
                source.logs_many(&addresses, start, next_start - 1)?
            } else {
                Vec::new()
            };
            let times = block_times_on(source, &logs, day)?;
            let mut by_pool: BTreeMap<Address, Vec<Log>> = BTreeMap::new();
            for log in logs {
                if !addresses.contains(&log.address) {
                    return Err(BackfillError::Inconsistent(format!(
                        "a log of {:#x} in block {} came back for pools it is not one of",
                        log.address, log.block_number
                    )));
                }
                by_pool.entry(log.address).or_default().push(log);
            }
            for index in wanted {
                let address = pools[index].address;
                let logs = by_pool.remove(&address).unwrap_or_default();
                let written = stored_logs(logs, &times).and_then(|logs| {
                    events.write_day(chain, address, day, &logs, complete)?;
                    Ok(logs.len())
                });
                match (written, &mut outcomes[index]) {
                    (Ok(rows), Ok(report)) => {
                        report.written.push((day, rows, complete));
                        seen.push(PoolDay {
                            pool: address,
                            rows,
                            state,
                            fetched: true,
                        });
                    }
                    (Err(error), outcome) => *outcome = Err(error),
                    (Ok(_), Err(_)) => unreachable!("a failed pool is not fetched"),
                }
            }
        }
        on_day(day, &seen);
        start = next_start;
        day = day.next();
    }
    Ok(outcomes)
}

/// Where one pool's history starts and the snapshot it replays from,
/// written now when none stored is usable. `start` is the first block of
/// `from`. The report's `from` is `None` when the pool has no whole day
/// yet.
fn prepare(
    source: &impl ChainSource,
    root: &std::path::Path,
    chain: &str,
    pool: PoolRef,
    from: Day,
    start: u64,
    mut report: BackfillReport,
) -> Result<BackfillReport, BackfillError> {
    let snapshots = SnapshotStore::new(root);
    let (head, last_day) = (report.head_block, Day::of_time(report.head_time));
    let mut from = from;
    let mut start = start;
    let mut base = usable_snapshot(root, chain, pool.address, from)?;
    if base.is_none() {
        let Some(before) = start.checked_sub(1) else {
            return Err(BackfillError::BeforeChain { day: from });
        };
        if !source.has_code(pool.address, before)? {
            // The pool did not exist before `from`. A replay cannot start
            // before its Initialize, so its history starts on the first
            // whole day after the block that created it.
            let created =
                first_block_with_code(source, pool.address, start, head)?.ok_or_else(|| {
                    BackfillError::Inconsistent(format!(
                        "{:#x} has no code even at block {head}; is it a pool?",
                        pool.address
                    ))
                })?;
            let created_day = Day::of_time(source.block_time(created)?);
            report.created = Some(created_day);
            from = created_day.next();
            if from > last_day {
                return Ok(report);
            }
            start = first_block_at_or_after(source, from.start(), created, head)?;
            base = usable_snapshot(root, chain, pool.address, from)?;
        }
    }
    report.from = Some(from);
    report.snapshot = Some(match base {
        Some(snapshot) => (snapshot.day, snapshot.block),
        None => {
            let block = start - 1;
            let state = source
                .snapshot(pool.address, block)
                .map_err(|source| BackfillError::Snapshot { block, source })?;
            snapshots.write(
                chain,
                &StoredSnapshot {
                    pool: pool.address,
                    token0: pool.token0,
                    token1: pool.token1,
                    block,
                    day: from,
                    state,
                },
            )?;
            report.snapshot_block = Some(block);
            (from, block)
        }
    });
    Ok(report)
}

/// What replaying a stored history showed against the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The day of the snapshot the replay started from.
    pub since: Day,
    /// State-changing events replayed.
    pub events: usize,
    /// The block the replica was compared with the chain at.
    pub block: u64,
    /// Empty when the replica equals the pool on the chain.
    pub differences: Vec<String>,
}

/// Replays what a backfill stored, from the snapshot it used through the
/// day of its own head, and compares the replica with the pool on the
/// chain at the block that replay reached: the backfill's head, or the
/// last stored event when an earlier run stored past that head. The replay
/// and the comparison thus cover the same blocks. `None` when the backfill
/// stored nothing to replay.
pub fn verify_backfill(
    source: &impl ChainSource,
    root: &std::path::Path,
    chain: &str,
    pool: Address,
    report: &BackfillReport,
) -> Result<Option<Verified>, BackfillError> {
    let Some((since, _)) = report.snapshot else {
        return Ok(None);
    };
    let snapshot = SnapshotStore::new(root).read(chain, pool, since)?;
    let through = Day::of_time(report.head_time);
    let events = read_events(root, chain, pool, since, through)?;
    let mut replica = Pool::from_snapshot(snapshot.state);
    for event in &events {
        replica
            .apply_event(&event.event, Replay::Strict, &mut ())
            .map_err(|error| StoreError::Replay {
                pool,
                block: event.block,
                log_index: event.log_index,
                reason: error.to_string(),
            })?;
    }
    let block = events
        .last()
        .map_or(report.head_block, |last| last.block.max(report.head_block));
    let on_chain = Pool::from_snapshot(
        source
            .snapshot(pool, block)
            .map_err(|source| BackfillError::Snapshot { block, source })?,
    );
    Ok(Some(Verified {
        since,
        events: events.len(),
        block,
        differences: replica.differences(&on_chain),
    }))
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use alloy_primitives::B256;
    use lpbot_venues::uniswap_v3::events::decode_log;
    use std::cell::{Cell, RefCell};

    /// A scripted chain: block times, logs and a head that can move.
    pub struct FakeChain {
        /// Time of block `i`.
        pub times: Vec<u64>,
        pub logs: Vec<Log>,
        pub head: Cell<u64>,
        /// The pool's state; before every log when `evolving`.
        pub snapshot: PoolSnapshot,
        /// When set, the state after a block is `snapshot` with every log
        /// up to that block applied, as on a real chain.
        pub evolving: bool,
        /// The block the pool was created in; before it there is no code
        /// and no state to read.
        pub created: Option<u64>,
        /// Addresses with no code at any block: not pools at all.
        pub no_code: Vec<Address>,
        pub time_reads: Cell<usize>,
        pub log_reads: RefCell<Vec<(u64, u64)>>,
        /// The pools each log read asked about.
        pub log_pools: RefCell<Vec<Vec<Address>>>,
        pub snapshot_reads: RefCell<Vec<u64>>,
    }

    impl FakeChain {
        /// Blocks every 12 seconds from `genesis_time`, every 50th slot missed.
        pub fn new(genesis_time: u64, blocks: u64, snapshot: PoolSnapshot) -> Self {
            let mut times = Vec::new();
            let mut time = genesis_time;
            for i in 0..blocks {
                times.push(time);
                time += if i % 50 == 49 { 24 } else { 12 };
            }
            Self {
                times,
                logs: Vec::new(),
                head: Cell::new(blocks - 1),
                snapshot,
                evolving: false,
                created: None,
                no_code: Vec::new(),
                time_reads: Cell::new(0),
                log_reads: RefCell::new(Vec::new()),
                log_pools: RefCell::new(Vec::new()),
                snapshot_reads: RefCell::new(Vec::new()),
            }
        }

        pub fn add_log(&mut self, pool: Address, block: u64, index: u64, data: Vec<u8>) {
            self.add_raw_log(pool, block, index, vec![B256::repeat_byte(0x11)], data);
        }

        /// A log with topics of its own.
        pub fn add_raw_log(
            &mut self,
            address: Address,
            block: u64,
            index: u64,
            topics: Vec<B256>,
            data: Vec<u8>,
        ) {
            self.logs.push(Log {
                address,
                topics,
                data,
                block_number: block,
                log_index: index,
                transaction_hash: B256::repeat_byte(block as u8),
            });
        }
    }

    impl ChainSource for FakeChain {
        fn head(&self) -> Result<(u64, u64), ChainError> {
            Ok((self.head.get(), self.times[self.head.get() as usize]))
        }

        fn block_time(&self, block: u64) -> Result<u64, ChainError> {
            assert!(block <= self.head.get(), "read past the head");
            self.time_reads.set(self.time_reads.get() + 1);
            Ok(self.times[block as usize])
        }

        fn block_times(&self, blocks: &[u64]) -> Result<Vec<u64>, ChainError> {
            Ok(blocks.iter().map(|b| self.times[*b as usize]).collect())
        }

        fn logs(&self, pool: Address, from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
            self.logs_many(&[pool], from, to)
        }

        /// One read for all the pools, as a node answers it.
        fn logs_many(&self, pools: &[Address], from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
            assert!(to <= self.head.get(), "read past the head");
            self.log_reads.borrow_mut().push((from, to));
            self.log_pools.borrow_mut().push(pools.to_vec());
            Ok(self
                .logs
                .iter()
                .filter(|l| pools.contains(&l.address) && (from..=to).contains(&l.block_number))
                .cloned()
                .collect())
        }

        fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
            self.snapshot_reads.borrow_mut().push(block);
            if !self.has_code(pool, block)? {
                // What `eth_call` to an address without code comes to.
                return Err(ChainError::ShortReturn {
                    what: "fee",
                    expected: 32,
                    got: 0,
                });
            }
            if !self.evolving {
                return Ok(self.snapshot.clone());
            }
            let mut replica = Pool::from_snapshot(self.snapshot.clone());
            for log in self.logs.iter().filter(|l| l.block_number <= block) {
                if let Some(event) = decode_log(&log.topics, &log.data).expect("a pool log") {
                    replica
                        .apply_event(&event, Replay::Strict, &mut ())
                        .expect("the scripted chain replays");
                }
            }
            Ok(replica.snapshot())
        }

        fn has_code(&self, address: Address, block: u64) -> Result<bool, ChainError> {
            if self.no_code.contains(&address) {
                return Ok(false);
            }
            Ok(!matches!(self.created, Some(created) if block < created))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakeChain;
    use super::*;
    use crate::snapshot::tests::sample;
    use crate::store::testing::TempDir;
    use std::fs;

    fn pool() -> PoolRef {
        let s = sample("2026-11-01", 0);
        PoolRef {
            address: s.pool,
            token0: s.token0,
            token1: s.token1,
        }
    }

    fn day(text: &str) -> Day {
        text.parse().unwrap()
    }

    /// A chain that starts at noon on 30 October 2026 and runs about three
    /// and a half days, with a log in every 100th block.
    fn chain() -> FakeChain {
        let genesis = day("2026-10-30").start() + 43_200;
        let mut chain = FakeChain::new(genesis, 25_000, sample("2026-11-01", 0).state);
        for block in (0..25_000).step_by(100) {
            chain.add_log(pool().address, block, 3, block.to_be_bytes().to_vec());
            chain.add_log(pool().address, block, 9, vec![]);
        }
        chain
    }

    fn all_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        let mut files = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push((path.clone(), fs::read(&path).unwrap()));
                }
            }
        }
        files.sort();
        files
    }

    #[test]
    fn the_first_block_of_a_time_is_found_at_every_position() {
        let chain = chain();
        let head = chain.head.get();
        for block in [
            0u64, 1, 49, 50, 51, 1_023, 1_024, 1_025, 7_000, 24_998, 24_999,
        ] {
            let time = chain.times[block as usize];
            assert_eq!(
                first_block_at_or_after(&chain, time, 0, head).unwrap(),
                block
            );
            // One second later the answer is the next block.
            assert_eq!(
                first_block_at_or_after(&chain, time + 1, 0, head).unwrap(),
                block + 1
            );
            // Starting the search at the answer itself works too.
            assert_eq!(
                first_block_at_or_after(&chain, time, block, head).unwrap(),
                block
            );
        }
        assert_eq!(first_block_at_or_after(&chain, 0, 0, head).unwrap(), 0);
        assert_eq!(
            first_block_at_or_after(&chain, u64::MAX, 0, head).unwrap(),
            head + 1
        );
        assert_eq!(first_block_at_or_after(&chain, 0, 10, 5).unwrap(), 6);
    }

    #[test]
    fn finding_a_block_a_day_ahead_takes_few_reads() {
        let chain = chain();
        let from = 3_000;
        let time = chain.times[from as usize] + 86_400;
        chain.time_reads.set(0);
        first_block_at_or_after(&chain, time, from, chain.head.get()).unwrap();
        assert!(chain.time_reads.get() <= 20, "{}", chain.time_reads.get());
    }

    #[test]
    fn every_log_lands_in_the_file_of_its_day() {
        let dir = TempDir::new("backfill");
        let chain = chain();
        let from = day("2026-10-31");
        let mut seen = Vec::new();
        let report = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            from,
            |d, rows, state| seen.push((d, rows, state)),
        )
        .unwrap();

        // 31 October, 1 November and 2 November are whole; 3 November is open.
        assert_eq!(report.written.len(), 4);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.head_block, 24_999);
        assert_eq!(
            report.written.iter().map(|w| w.2).collect::<Vec<_>>(),
            vec![true, true, true, false]
        );
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[3].2, DayState::Partial);

        let store = EventStore::new(dir.path());
        let mut stored = 0;
        for (d, rows, _) in &report.written {
            let logs = store
                .read_day("ethereum", pool().address, *d)
                .unwrap()
                .unwrap();
            assert_eq!(logs.len(), *rows);
            assert!(logs.iter().all(|l| Day::of_time(l.block_time) == *d));
            assert!(logs
                .iter()
                .all(|l| l.block_time == chain.times[l.block_number as usize]));
            assert!(
                logs.windows(2)
                    .all(|w| (w[0].block_number, w[0].log_index)
                        < (w[1].block_number, w[1].log_index))
            );
            stored += logs.len();
        }
        // Every log from the first block of 31 October on is stored, once.
        let first = first_block_at_or_after(&chain, from.start(), 0, 24_999).unwrap();
        let expected = chain
            .logs
            .iter()
            .filter(|l| l.block_number >= first)
            .count();
        assert_eq!(stored, expected);
        assert_eq!(report.rows_written(), expected);

        // The snapshot is the state right before the first day.
        assert_eq!(report.snapshot_block, Some(first - 1));
        let snapshot = SnapshotStore::new(dir.path())
            .read("ethereum", pool().address, from)
            .unwrap();
        assert_eq!((snapshot.block, snapshot.day), (first - 1, from));
        assert_eq!(snapshot.token0, pool().token0);
    }

    #[test]
    fn a_second_run_fetches_only_the_open_day_and_changes_nothing() {
        let dir = TempDir::new("backfill");
        let chain = chain();
        let from = day("2026-10-31");
        backfill_pool(&chain, dir.path(), "ethereum", pool(), from, |_, _, _| {}).unwrap();
        let before = all_files(dir.path());
        chain.log_reads.borrow_mut().clear();
        chain.snapshot_reads.borrow_mut().clear();

        let report =
            backfill_pool(&chain, dir.path(), "ethereum", pool(), from, |_, _, _| {}).unwrap();
        assert_eq!(report.skipped, 3);
        assert_eq!(report.written.len(), 1);
        assert_eq!(report.snapshot_block, None);
        assert_eq!(chain.log_reads.borrow().len(), 1);
        assert!(chain.snapshot_reads.borrow().is_empty());
        assert_eq!(all_files(dir.path()), before);
    }

    #[test]
    fn an_open_day_is_completed_once_the_head_moves_past_it() {
        let dir = TempDir::new("backfill");
        let chain = chain();
        let from = day("2026-11-01");
        // First run with the head in the middle of 1 November.
        let midday = first_block_at_or_after(&chain, from.start() + 40_000, 0, 24_999).unwrap();
        chain.head.set(midday);
        let report =
            backfill_pool(&chain, dir.path(), "ethereum", pool(), from, |_, _, _| {}).unwrap();
        assert_eq!(report.written.len(), 1);
        assert!(!report.written[0].2);
        let store = EventStore::new(dir.path());
        assert_eq!(
            store.day_state("ethereum", pool().address, from),
            DayState::Partial
        );
        let partial_rows = report.written[0].1;

        // The chain moves on two days.
        chain.head.set(24_999);
        let report =
            backfill_pool(&chain, dir.path(), "ethereum", pool(), from, |_, _, _| {}).unwrap();
        assert_eq!(report.written.len(), 3);
        assert!(report.written[0].2 && report.written[0].1 > partial_rows);
        assert_eq!(
            store.day_state("ethereum", pool().address, from),
            DayState::Complete
        );
        // The range reads back whole and in order.
        let logs = store
            .read_days("ethereum", pool().address, from, day("2026-11-03"))
            .unwrap();
        assert_eq!(logs.len(), report.rows_written());
    }

    /// A pool that really ran for three days, with swaps, mints and burns,
    /// served as a chain whose head is an hour into the fourth day. Its
    /// blocks are twelve seconds apart, block 100 starting the first day.
    fn ran() -> (FakeChain, PoolRef, crate::synth::SynthHistory) {
        use crate::synth::{generate, stored_log, tests as synth, SynthConfig};

        let config = SynthConfig {
            first_block: 100,
            ..synth::small()
        };
        let pool = synth::pool();
        let history = generate(&config, pool).unwrap();
        let genesis = config.start.start() - 100 * 12;
        let mut chain = FakeChain::new(
            genesis,
            100 + 3 * 7_200 + 300,
            history.snapshot.state.clone(),
        );
        chain.times = (0..chain.times.len() as u64)
            .map(|i| genesis + i * 12)
            .collect();
        chain.logs = history
            .events
            .iter()
            .map(|event| {
                let stored = stored_log(event);
                Log {
                    address: pool.address,
                    topics: stored.topics,
                    data: stored.data,
                    block_number: stored.block_number,
                    log_index: u64::from(stored.log_index),
                    transaction_hash: stored.tx_hash,
                }
            })
            .collect();
        (chain, pool, history)
    }

    #[test]
    fn a_backfilled_history_replays_to_the_pool_on_the_chain() {
        use crate::lake::replay_from_snapshot;

        let (chain, pool, history) = ran();
        let config = crate::synth::SynthConfig {
            first_block: 100,
            ..crate::synth::tests::small()
        };
        let dir = TempDir::new("backfill");
        let report = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool,
            config.start,
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(report.snapshot_block, Some(99));
        assert_eq!(*chain.snapshot_reads.borrow(), vec![99]);
        assert_eq!(report.rows_written(), history.events.len());
        assert_eq!(
            report.written.iter().map(|w| w.2).collect::<Vec<_>>(),
            vec![true, true, true, false]
        );

        let snapshot = SnapshotStore::new(dir.path())
            .read("ethereum", pool.address, config.start)
            .unwrap();
        let (replica, applied) = replay_from_snapshot(
            dir.path(),
            "ethereum",
            snapshot,
            config.start.next().next().next(),
        )
        .unwrap();
        assert_eq!(applied, history.events.len());
        assert!(replica.differences(&history.end).is_empty());
    }

    #[test]
    fn the_check_after_a_backfill_replays_and_compares_up_to_the_same_block_across_midnight() {
        use crate::lake::replay_from_snapshot;

        let (mut chain, pool, history) = ran();
        chain.evolving = true;
        let start = history.snapshot.day;
        // The run reads the head a minute before the second day ends...
        chain.head.set(100 + 2 * 7_200 - 5);
        let (first_head, first_time) = chain.head().unwrap();
        assert_eq!(Day::of_time(first_time), start.next());
        // ...and the chain is two hours into the third day, which has
        // swaps, when the pool's turn comes.
        chain.head.set(100 + 2 * 7_200 + 600);

        let dir = TempDir::new("backfill");
        let report =
            backfill_pool(&chain, dir.path(), "ethereum", pool, start, |_, _, _| {}).unwrap();
        assert_eq!(report.head_block, 100 + 2 * 7_200 + 600);
        // The old check replayed up to the day of the head read first and
        // compared at the pool's own head: it fails on a sound history.
        let snapshot = SnapshotStore::new(dir.path())
            .read("ethereum", pool.address, start)
            .unwrap();
        let (stale, _) =
            replay_from_snapshot(dir.path(), "ethereum", snapshot, Day::of_time(first_time))
                .unwrap();
        let on_chain =
            Pool::from_snapshot(chain.snapshot(pool.address, report.head_block).unwrap());
        assert!(!stale.differences(&on_chain).is_empty());
        // The check replays through the day of the pool's own head and
        // compares there.
        let verified = verify_backfill(&chain, dir.path(), "ethereum", pool.address, &report)
            .unwrap()
            .unwrap();
        assert_eq!((verified.since, verified.block), (start, report.head_block));
        assert!(verified.events > 0);
        assert!(
            verified.differences.is_empty(),
            "{:?}",
            verified.differences
        );

        // A run pinned to the head it read first stores every pool up to
        // that head and checks there, however far the chain has moved.
        let dir = TempDir::new("backfill");
        let pinned = AtHead {
            source: &chain,
            block: first_head,
            time: first_time,
        };
        let report =
            backfill_pool(&pinned, dir.path(), "ethereum", pool, start, |_, _, _| {}).unwrap();
        assert_eq!(
            (report.head_block, report.head_time),
            (first_head, first_time)
        );
        assert_eq!(
            report.written.iter().map(|w| w.2).collect::<Vec<_>>(),
            vec![true, false]
        );
        let verified = verify_backfill(&pinned, dir.path(), "ethereum", pool.address, &report)
            .unwrap()
            .unwrap();
        assert_eq!(verified.block, first_head);
        assert!(
            verified.differences.is_empty(),
            "{:?}",
            verified.differences
        );

        // A history that lacks an event fails the check.
        let day_file = dir.path().join(format!(
            "events/chain=ethereum/pool={:#x}/day={start}/events.parquet",
            pool.address
        ));
        let mut logs = crate::store::read_logs(&day_file).unwrap();
        logs.remove(logs.len() / 2);
        crate::store::write_logs(&day_file, &logs).unwrap();
        let outcome = verify_backfill(&pinned, dir.path(), "ethereum", pool.address, &report);
        assert!(
            outcome.is_err() || !outcome.unwrap().unwrap().differences.is_empty(),
            "a gap must not pass"
        );
    }

    #[test]
    fn nothing_stored_means_nothing_to_check() {
        let chain = chain();
        let dir = TempDir::new("backfill");
        let report = BackfillReport::default();
        assert_eq!(
            verify_backfill(&chain, dir.path(), "ethereum", pool().address, &report).unwrap(),
            None
        );
    }

    #[test]
    fn a_daily_rerun_reuses_the_snapshot_it_already_has() {
        let dir = TempDir::new("backfill");
        let chain = chain();
        let first = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-10-31"),
            |_, _, _| {},
        )
        .unwrap();
        let block = first.snapshot_block.unwrap();
        assert_eq!(first.snapshot, Some((day("2026-10-31"), block)));
        assert!(
            first.notes()[0].contains("taken at block"),
            "{:?}",
            first.notes()
        );

        // The next day the start moves on a day, as `--days 90` does.
        chain.snapshot_reads.borrow_mut().clear();
        let next = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-11-01"),
            |_, _, _| {},
        )
        .unwrap();
        assert!(chain.snapshot_reads.borrow().is_empty());
        assert_eq!(next.snapshot_block, None);
        assert_eq!(next.snapshot, Some((day("2026-10-31"), block)));
        assert_eq!(next.from, Some(day("2026-11-01")));
        let note = &next.notes()[0];
        assert!(
            note.contains("2026-10-31") && note.contains("no new snapshot"),
            "{note}"
        );
        assert_eq!(
            SnapshotStore::new(dir.path())
                .days("ethereum", pool().address)
                .unwrap(),
            vec![day("2026-10-31")]
        );

        // With a day missing between the snapshot and the start, the old
        // snapshot cannot reach the start: a new one is taken.
        fs::remove_dir_all(dir.path().join(format!(
            "events/chain=ethereum/pool={:#x}/day=2026-11-01",
            pool().address
        )))
        .unwrap();
        let after_gap = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-11-02"),
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(chain.snapshot_reads.borrow().len(), 1);
        assert_eq!(
            after_gap.snapshot.map(|(day, _)| day),
            Some(day("2026-11-02"))
        );
    }

    #[test]
    fn a_pool_created_after_the_start_is_stored_from_its_first_whole_day() {
        let dir = TempDir::new("backfill");
        let mut chain = chain();
        let head = chain.head.get();
        let created =
            first_block_at_or_after(&chain, day("2026-11-01").start() + 36_000, 0, head).unwrap();
        chain.created = Some(created);

        let report = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-10-31"),
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(report.created, Some(day("2026-11-01")));
        assert_eq!(report.from, Some(day("2026-11-02")));
        let second = first_block_at_or_after(&chain, day("2026-11-02").start(), 0, head).unwrap();
        assert_eq!(report.snapshot_block, Some(second - 1));
        assert_eq!(*chain.snapshot_reads.borrow(), vec![second - 1]);
        assert_eq!(
            report
                .written
                .iter()
                .map(|(d, _, complete)| (*d, *complete))
                .collect::<Vec<_>>(),
            vec![(day("2026-11-02"), true), (day("2026-11-03"), false)]
        );
        let notes = report.notes();
        assert!(
            notes[0].contains("created on 2026-11-01") && notes[0].contains("2026-11-02"),
            "{notes:?}"
        );

        // A second run with the same start finds the creation again and
        // reuses the snapshot.
        chain.snapshot_reads.borrow_mut().clear();
        let again = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-10-31"),
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(again.created, Some(day("2026-11-01")));
        assert!(chain.snapshot_reads.borrow().is_empty());
        assert_eq!(again.skipped, 1);

        // A pool created on the day of the head has no whole day yet.
        let dir = TempDir::new("backfill");
        chain.created = Some(head - 10);
        let report = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-10-31"),
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(report.created, Some(day("2026-11-03")));
        assert_eq!((report.snapshot, report.written.len()), (None, 0));
        assert!(
            report.notes()[0].contains("2026-11-04"),
            "{:?}",
            report.notes()
        );

        // An address with no code at all is not a pool.
        chain.created = Some(head + 1);
        let error = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-10-31"),
            |_, _, _| {},
        )
        .unwrap_err();
        assert!(error.to_string().contains("no code"), "{error}");
    }

    #[test]
    fn the_creation_block_is_found_by_halving() {
        let mut chain = chain();
        let address = pool().address;
        assert_eq!(
            first_block_with_code(&chain, address, 5, 900).unwrap(),
            Some(5)
        );
        for created in [6, 7, 450, 899, 900] {
            chain.created = Some(created);
            assert_eq!(
                first_block_with_code(&chain, address, 5, 900).unwrap(),
                Some(created)
            );
        }
        chain.created = Some(901);
        assert_eq!(
            first_block_with_code(&chain, address, 5, 900).unwrap(),
            None
        );
        assert_eq!(first_block_with_code(&chain, address, 9, 8).unwrap(), None);
    }

    #[test]
    fn starting_after_the_head_or_before_the_chain_is_handled() {
        let dir = TempDir::new("backfill");
        let chain = chain();
        let report = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2027-01-01"),
            |_, _, _| {},
        )
        .unwrap();
        assert_eq!(report.written, vec![]);
        assert!(all_files(dir.path()).is_empty());

        let error = backfill_pool(
            &chain,
            dir.path(),
            "ethereum",
            pool(),
            day("2026-10-01"),
            |_, _, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(error, BackfillError::BeforeChain { .. }),
            "{error}"
        );
        assert!(error.to_string().contains("pick a later start"));
    }

    #[test]
    fn a_log_dated_outside_its_day_stops_the_backfill() {
        let dir = TempDir::new("backfill");
        let mut chain = chain();
        // The node claims a log for a block of the previous day.
        struct Lying(FakeChain);
        impl ChainSource for Lying {
            fn head(&self) -> Result<(u64, u64), ChainError> {
                self.0.head()
            }
            fn block_time(&self, block: u64) -> Result<u64, ChainError> {
                self.0.block_time(block)
            }
            fn block_times(&self, blocks: &[u64]) -> Result<Vec<u64>, ChainError> {
                Ok(blocks.iter().map(|_| 5).collect())
            }
            fn logs(&self, pool: Address, from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
                self.0.logs(pool, from, to)
            }
            fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
                self.0.snapshot(pool, block)
            }
        }
        chain.add_log(pool().address, 24_990, 0, vec![]);
        let error = backfill_pool(
            &Lying(chain),
            dir.path(),
            "ethereum",
            pool(),
            day("2026-11-03"),
            |_, _, _| {},
        )
        .unwrap_err();
        assert!(matches!(error, BackfillError::Inconsistent(_)), "{error}");
    }

    /// The chain of `chain()` with two more pools, each with a log in
    /// every 100th block at its own offset.
    fn three_pools() -> (FakeChain, Vec<PoolRef>) {
        let mut chain = chain();
        let mut pools = vec![pool()];
        for (n, byte) in [0xb1u8, 0xc2].into_iter().enumerate() {
            let other = PoolRef {
                address: Address::repeat_byte(byte),
                ..pool()
            };
            for block in (0..25_000u64).step_by(100) {
                let block = block + 10 * (n as u64 + 1);
                chain.add_log(other.address, block, 1, block.to_be_bytes().to_vec());
            }
            pools.push(other);
        }
        (chain, pools)
    }

    /// Every file under `root`, by its path below `root`.
    fn files_below(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
        all_files(root)
            .into_iter()
            .map(|(path, bytes)| (path.strip_prefix(root).unwrap().to_path_buf(), bytes))
            .collect()
    }

    #[test]
    fn several_pools_store_what_one_at_a_time_does_with_one_log_read_a_day() {
        let (chain, pools) = three_pools();
        let from = day("2026-10-31");

        let alone = TempDir::new("backfill");
        for pool in &pools {
            backfill_pool(&chain, alone.path(), "ethereum", *pool, from, |_, _, _| {}).unwrap();
        }
        // Four days, read once for each of the three pools.
        assert_eq!(chain.log_reads.borrow().len(), 3 * 4);

        chain.log_reads.borrow_mut().clear();
        chain.log_pools.borrow_mut().clear();
        let together = TempDir::new("backfill");
        let mut days = Vec::new();
        let outcomes = backfill_pools(
            &chain,
            together.path(),
            "ethereum",
            &pools,
            from,
            |d, seen| days.push((d, seen.to_vec())),
        )
        .unwrap();
        // Four days, read once for all three pools.
        assert_eq!(chain.log_reads.borrow().len(), 4);
        assert!(chain
            .log_pools
            .borrow()
            .iter()
            .all(|asked| asked.len() == 3));
        assert_eq!(days.len(), 4);
        assert!(days
            .iter()
            .all(|(_, seen)| seen.len() == 3 && seen.iter().all(|p| p.fetched && p.rows > 0)));
        assert!(days[3].1.iter().all(|p| p.state == DayState::Partial));
        for outcome in &outcomes {
            assert_eq!(outcome.as_ref().unwrap().written.len(), 4);
        }
        // The same files, byte for byte.
        assert_eq!(files_below(alone.path()), files_below(together.path()));
    }

    #[test]
    fn a_pool_stored_before_is_left_out_of_the_reads_of_its_stored_days() {
        let (chain, pools) = three_pools();
        let from = day("2026-10-31");
        let dir = TempDir::new("backfill");
        backfill_pool(&chain, dir.path(), "ethereum", pools[0], from, |_, _, _| {}).unwrap();
        chain.log_pools.borrow_mut().clear();

        let mut days = Vec::new();
        let outcomes = backfill_pools(&chain, dir.path(), "ethereum", &pools, from, |d, seen| {
            days.push((d, seen.to_vec()))
        })
        .unwrap();
        let asked = chain.log_pools.borrow();
        assert_eq!(asked.len(), 4);
        // The first pool's three whole days are stored: only the others
        // are asked for them. The open day is asked for again for all.
        for asked in &asked[..3] {
            assert_eq!(*asked, vec![pools[1].address, pools[2].address]);
        }
        assert_eq!(asked[3].len(), 3);
        assert_eq!(outcomes[0].as_ref().unwrap().skipped, 3);
        let first = days[0]
            .1
            .iter()
            .find(|p| p.pool == pools[0].address)
            .unwrap();
        assert!(!first.fetched && first.rows == 0);
    }

    #[test]
    fn a_pool_that_cannot_start_fails_alone() {
        let (mut chain, pools) = three_pools();
        chain.no_code = vec![pools[1].address];
        let dir = TempDir::new("backfill");
        let outcomes = backfill_pools(
            &chain,
            dir.path(),
            "ethereum",
            &pools,
            day("2026-10-31"),
            |_, seen| assert!(seen.iter().all(|p| p.pool != pools[1].address)),
        )
        .unwrap();
        let error = outcomes[1].as_ref().unwrap_err();
        assert!(error.to_string().contains("no code"), "{error}");
        assert_eq!(outcomes[0].as_ref().unwrap().written.len(), 4);
        assert_eq!(outcomes[2].as_ref().unwrap().written.len(), 4);
        assert!(chain
            .log_pools
            .borrow()
            .iter()
            .all(|asked| !asked.contains(&pools[1].address)));
    }

    #[test]
    fn a_log_of_a_pool_not_asked_for_stops_the_run() {
        // A node that answers with every log in the range.
        struct Stray(FakeChain);
        impl ChainSource for Stray {
            fn head(&self) -> Result<(u64, u64), ChainError> {
                self.0.head()
            }
            fn block_time(&self, block: u64) -> Result<u64, ChainError> {
                self.0.block_time(block)
            }
            fn block_times(&self, blocks: &[u64]) -> Result<Vec<u64>, ChainError> {
                self.0.block_times(blocks)
            }
            fn logs(&self, pool: Address, from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
                self.0.logs(pool, from, to)
            }
            fn logs_many(&self, _: &[Address], from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
                let mut logs: Vec<Log> = self
                    .0
                    .logs
                    .iter()
                    .filter(|l| (from..=to).contains(&l.block_number))
                    .cloned()
                    .collect();
                logs.sort_by_key(|l| (l.block_number, l.log_index));
                Ok(logs)
            }
            fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
                self.0.snapshot(pool, block)
            }
        }
        let (chain, pools) = three_pools();
        let dir = TempDir::new("backfill");
        let error = backfill_pools(
            &Stray(chain),
            dir.path(),
            "ethereum",
            &pools[..2],
            day("2026-10-31"),
            |_, _| {},
        )
        .unwrap_err();
        assert!(matches!(error, BackfillError::Inconsistent(_)), "{error}");
        assert!(error.to_string().contains("not one of"), "{error}");
    }

    #[test]
    fn no_pools_means_nothing_read_or_written() {
        let chain = chain();
        let dir = TempDir::new("backfill");
        let outcomes = backfill_pools(
            &chain,
            dir.path(),
            "ethereum",
            &[],
            day("2026-10-31"),
            |_, _| panic!("no day to report"),
        )
        .unwrap();
        assert!(outcomes.is_empty());
        assert!(chain.log_reads.borrow().is_empty());
        assert!(all_files(dir.path()).is_empty());
    }
}
