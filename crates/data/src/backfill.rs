//! Backfill: a pool's history from a node into the event store.
//!
//! For every UTC day from a start day to the head it finds the day's block
//! range, reads every log the pool emitted in it and writes one file. A day
//! that is already stored complete is skipped, so a re-run only fetches what
//! is new. The day in progress is stored as partial and replaced by the
//! next run. Before the first day it stores a snapshot of the pool, the
//! state every replay starts from.

use std::collections::BTreeMap;

use alloy_primitives::Address;
use lpbot_chain::eth::Log;
use lpbot_chain::uniswap_v3::{fetch_snapshot, ChainError};
use lpbot_chain::RpcClient;
use lpbot_venues::uniswap_v3::pool::PoolSnapshot;

use crate::day::Day;
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
    /// The pool's full state after a block.
    fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError>;
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

    fn snapshot(&self, pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
        fetch_snapshot(self.client, pool, block)
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
    pub head_block: u64,
}

impl BackfillReport {
    pub fn rows_written(&self) -> usize {
        self.written.iter().map(|(_, rows, _)| rows).sum()
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

fn stored_logs(
    source: &impl ChainSource,
    logs: Vec<Log>,
    day: Day,
) -> Result<Vec<StoredLog>, BackfillError> {
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
    let events = EventStore::new(root);
    let snapshots = SnapshotStore::new(root);
    let (head, head_time) = source.head()?;
    let mut report = BackfillReport {
        head_block: head,
        ..Default::default()
    };
    let last_day = Day::of_time(head_time);
    if from > last_day {
        return Ok(report);
    }

    let mut start = first_block_at_or_after(source, from.start(), 0, head)?;
    if !snapshots.has(chain, pool.address, from) {
        let Some(block) = start.checked_sub(1) else {
            return Err(BackfillError::BeforeChain { day: from });
        };
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
    }

    let mut day = from;
    while day <= last_day {
        // The next day's first block closes this day. Past the head it does
        // not exist yet, and the day is still in progress.
        let next_start = first_block_at_or_after(source, day.end(), start, head)?;
        let complete = next_start <= head;

        if events.day_state(chain, pool.address, day) == DayState::Complete {
            report.skipped += 1;
            on_day(day, 0, DayState::Complete);
        } else {
            let logs = if start < next_start {
                source.logs(pool.address, start, next_start - 1)?
            } else {
                Vec::new()
            };
            let logs = stored_logs(source, logs, day)?;
            events.write_day(chain, pool.address, day, &logs, complete)?;
            report.written.push((day, logs.len(), complete));
            on_day(
                day,
                logs.len(),
                if complete {
                    DayState::Complete
                } else {
                    DayState::Partial
                },
            );
        }
        start = next_start;
        day = day.next();
    }
    Ok(report)
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use alloy_primitives::B256;
    use std::cell::{Cell, RefCell};

    /// A scripted chain: block times, logs and a head that can move.
    pub struct FakeChain {
        /// Time of block `i`.
        pub times: Vec<u64>,
        pub logs: Vec<Log>,
        pub head: Cell<u64>,
        pub snapshot: PoolSnapshot,
        pub time_reads: Cell<usize>,
        pub log_reads: RefCell<Vec<(u64, u64)>>,
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
                time_reads: Cell::new(0),
                log_reads: RefCell::new(Vec::new()),
                snapshot_reads: RefCell::new(Vec::new()),
            }
        }

        pub fn add_log(&mut self, pool: Address, block: u64, index: u64, data: Vec<u8>) {
            self.logs.push(Log {
                address: pool,
                topics: vec![B256::repeat_byte(0x11)],
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
            assert!(to <= self.head.get(), "read past the head");
            self.log_reads.borrow_mut().push((from, to));
            Ok(self
                .logs
                .iter()
                .filter(|l| l.address == pool && (from..=to).contains(&l.block_number))
                .cloned()
                .collect())
        }

        fn snapshot(&self, _pool: Address, block: u64) -> Result<PoolSnapshot, ChainError> {
            self.snapshot_reads.borrow_mut().push(block);
            Ok(self.snapshot.clone())
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

    #[test]
    fn a_backfilled_history_replays_to_the_pool_on_the_chain() {
        use crate::lake::replay_from_snapshot;
        use crate::synth::{generate, stored_log, tests as synth, SynthConfig};

        // A pool that really ran: three days of swaps, mints and burns.
        let config = SynthConfig {
            first_block: 100,
            ..synth::small()
        };
        let pool = synth::pool();
        let history = generate(&config, pool).unwrap();

        // Serve it as a chain whose head is an hour into the fourth day.
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
}
