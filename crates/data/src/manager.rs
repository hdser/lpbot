//! The position manager's logs, stored by day beside the pool history.
//!
//! Following position NFTs needs the manager's own events next to the
//! pools': `IncreaseLiquidity`, `DecreaseLiquidity`, `Collect` and
//! `Transfer`. They are stored in the same format as a pool's, under their
//! own folder so nothing that lists the pools sees them:
//!
//! ```text
//! <lake>/manager/events/chain=<chain>/pool=<manager address>/day=<YYYY-MM-DD>/events.parquet
//! ```
//!
//! The manager acts for every Uniswap v3 pool, not only the followed ones;
//! the book picks out its pools' NFTs by pairing. A day is 3,000 to 8,000
//! logs. The local node serves a day in a few seconds and several days at
//! once about as fast as one, so days are fetched on several connections.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use alloy_primitives::{Address, B256};

use crate::backfill::{
    block_times_on, first_block_at_or_after, stored_logs, BackfillError, ChainSource,
};
use crate::day::Day;
use crate::positions::{manager_topics, ChainLog, MANAGER};
use crate::store::{DayState, EventStore, StoreResult, StoredLog};

/// Where the manager's logs live under a lake.
pub fn manager_root(lake: &Path) -> PathBuf {
    lake.join("manager")
}

/// What storing the manager's days did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagerDays {
    /// Days fetched and written, with their row count.
    pub written: Vec<(Day, usize)>,
    /// Days already stored complete.
    pub skipped: usize,
}

/// One day's blocks: from `first` up to, not including, `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayRange {
    pub day: Day,
    pub first: u64,
    pub end: u64,
}

/// The block ranges of the days from `from` to `to`, both included, that
/// are not stored complete and are over at the source's head, and how many
/// are stored already.
pub fn missing_ranges(
    source: &impl ChainSource,
    lake: &Path,
    chain: &str,
    from: Day,
    to: Day,
) -> Result<(Vec<DayRange>, usize), BackfillError> {
    let store = EventStore::new(manager_root(lake));
    let missing: Vec<Day> = days(from, to)
        .filter(|day| store.day_state(chain, MANAGER, *day) != DayState::Complete)
        .collect();
    let skipped = days(from, to).count() - missing.len();
    let mut ranges = Vec::new();
    let (Some(first), Some(last)) = (missing.first().copied(), missing.last().copied()) else {
        return Ok((ranges, skipped));
    };
    let (head, _) = source.head()?;
    let mut start = first_block_at_or_after(source, first.start(), 0, head)?;
    let mut day = first;
    while day <= last {
        let next_start = first_block_at_or_after(source, day.end(), start, head)?;
        if next_start > head {
            // Not over yet at the head.
            break;
        }
        if missing.contains(&day) {
            ranges.push(DayRange {
                day,
                first: start,
                end: next_start,
            });
        }
        start = next_start;
        day = day.next();
    }
    Ok((ranges, skipped))
}

/// One day of the manager's logs the book follows, with their block times.
pub fn fetch_day(
    source: &impl ChainSource,
    range: &DayRange,
) -> Result<Vec<StoredLog>, BackfillError> {
    if range.first >= range.end {
        return Ok(Vec::new());
    }
    let wanted: Vec<B256> = manager_topics::all();
    let logs: Vec<_> = source
        .logs(MANAGER, range.first, range.end - 1)?
        .into_iter()
        .filter(|log| log.topics.first().is_some_and(|t| wanted.contains(t)))
        .collect();
    let times = block_times_on(source, &logs, range.day)?;
    stored_logs(logs, &times)
}

/// Stores the manager's logs for every day from `from` to `to`, both
/// included, that is not stored complete yet, fetching `threads` days at
/// once. A day is complete once the next day's first block is at or
/// behind the source's head; a later day is not stored. Safe to run
/// again: it fetches only what is missing. `on_day` hears of each day
/// written, in no particular order.
pub fn store_manager_days(
    source: &(impl ChainSource + Sync),
    lake: &Path,
    chain: &str,
    from: Day,
    to: Day,
    threads: usize,
    on_day: impl Fn(Day, usize) + Sync,
) -> Result<ManagerDays, BackfillError> {
    let (ranges, skipped) = missing_ranges(source, lake, chain, from, to)?;
    let store = EventStore::new(manager_root(lake));
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let written: Mutex<Vec<(Day, usize)>> = Mutex::new(Vec::new());
    let errors: Mutex<Vec<BackfillError>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1).min(ranges.len().max(1)) {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some(range) = ranges.get(index) else {
                    return;
                };
                if failed.load(Ordering::Relaxed) {
                    return;
                }
                let result = fetch_day(source, range).and_then(|logs| {
                    store.write_day(chain, MANAGER, range.day, &logs, true)?;
                    Ok(logs.len())
                });
                match result {
                    Ok(rows) => {
                        on_day(range.day, rows);
                        written
                            .lock()
                            .expect("not poisoned")
                            .push((range.day, rows));
                    }
                    Err(error) => {
                        failed.store(true, Ordering::Relaxed);
                        errors.lock().expect("not poisoned").push(error);
                        return;
                    }
                }
            });
        }
    });
    if let Some(error) = errors
        .into_inner()
        .expect("not poisoned")
        .into_iter()
        .next()
    {
        return Err(error);
    }
    let mut written = written.into_inner().expect("not poisoned");
    written.sort();
    Ok(ManagerDays { written, skipped })
}

fn days(from: Day, to: Day) -> impl Iterator<Item = Day> {
    let mut day = from;
    std::iter::from_fn(move || {
        if day > to {
            return None;
        }
        let current = day;
        day = day.next();
        Some(current)
    })
}

/// The first day from `from` to `to` that is not stored complete for the
/// manager, if any.
pub fn first_missing_day(lake: &Path, chain: &str, from: Day, to: Day) -> Option<Day> {
    let store = EventStore::new(manager_root(lake));
    days(from, to).find(|day| store.day_state(chain, MANAGER, *day) != DayState::Complete)
}

/// One day of the followed pools and the manager, merged into chain
/// order. A pool's missing day is an error; the manager's too.
pub fn read_merged_day(
    lake: &Path,
    chain: &str,
    pools: &[Address],
    day: Day,
    with_manager: bool,
) -> StoreResult<Vec<ChainLog>> {
    let pool_store = EventStore::new(lake);
    let mut sources: BTreeMap<Address, Vec<StoredLog>> = BTreeMap::new();
    for pool in pools {
        sources.insert(*pool, pool_store.read_days(chain, *pool, day, day)?);
    }
    if with_manager {
        let store = EventStore::new(manager_root(lake));
        sources.insert(MANAGER, store.read_days(chain, MANAGER, day, day)?);
    }
    let mut merged: Vec<ChainLog> = sources
        .into_iter()
        .flat_map(|(address, logs)| {
            logs.into_iter().map(move |log| ChainLog {
                address,
                block: log.block_number,
                time: log.block_time,
                log_index: log.log_index,
                tx_hash: log.tx_hash,
                topics: log.topics,
                data: log.data,
            })
        })
        .collect();
    merged.sort_by_key(|log| (log.block, log.log_index));
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backfill::testing::FakeChain;
    use crate::snapshot::tests::sample;
    use crate::store::testing::TempDir;
    use alloy_primitives::U256;

    /// A scripted chain several threads can read.
    struct Locked(Mutex<FakeChain>);

    impl ChainSource for Locked {
        fn head(&self) -> Result<(u64, u64), lpbot_chain::uniswap_v3::ChainError> {
            self.0.lock().unwrap().head()
        }
        fn block_time(&self, block: u64) -> Result<u64, lpbot_chain::uniswap_v3::ChainError> {
            self.0.lock().unwrap().block_time(block)
        }
        fn block_times(
            &self,
            blocks: &[u64],
        ) -> Result<Vec<u64>, lpbot_chain::uniswap_v3::ChainError> {
            self.0.lock().unwrap().block_times(blocks)
        }
        fn logs(
            &self,
            pool: Address,
            from: u64,
            to: u64,
        ) -> Result<Vec<lpbot_chain::eth::Log>, lpbot_chain::uniswap_v3::ChainError> {
            self.0.lock().unwrap().logs(pool, from, to)
        }
        fn snapshot(
            &self,
            pool: Address,
            block: u64,
        ) -> Result<lpbot_venues::uniswap_v3::pool::PoolSnapshot, lpbot_chain::uniswap_v3::ChainError>
        {
            self.0.lock().unwrap().snapshot(pool, block)
        }
    }

    #[test]
    fn the_managers_days_are_stored_once_and_read_back_merged_with_the_pools() {
        let genesis = Day::from_ymd(2026, 11, 1).unwrap().start();
        // Blocks every 12 seconds or so, for two days and part of a third.
        let mut chain = FakeChain::new(genesis, 15_000, sample("2026-11-01", 0).state);
        let token = |n: u64| B256::from(U256::from(n).to_be_bytes::<32>());
        let increase = |block: u64, index: u64| {
            (
                vec![manager_topics::increase_liquidity(), token(block)],
                block,
                index,
            )
        };
        let approval = alloy_primitives::keccak256("Approval(address,address,uint256)");
        for (topics, block, index) in [increase(10, 1), increase(7_300, 4), increase(7_300, 2)] {
            chain.add_raw_log(MANAGER, block, index, topics, vec![0u8; 96]);
        }
        chain.add_raw_log(
            MANAGER,
            11,
            0,
            vec![approval, B256::ZERO, B256::ZERO],
            vec![],
        );

        let dir = TempDir::new("manager");
        let (first, second, third) = (
            Day::from_ymd(2026, 11, 1).unwrap(),
            Day::from_ymd(2026, 11, 2).unwrap(),
            Day::from_ymd(2026, 11, 3).unwrap(),
        );
        let chain = Locked(Mutex::new(chain));
        let report =
            store_manager_days(&chain, dir.path(), "ethereum", first, third, 3, |_, _| {}).unwrap();
        // The third day is not over at the head, so it is not stored.
        assert_eq!(report.written, vec![(first, 1), (second, 2)]);
        assert_eq!(
            first_missing_day(dir.path(), "ethereum", first, third),
            Some(third)
        );
        let again = store_manager_days(&chain, dir.path(), "ethereum", first, second, 3, |_, _| {})
            .unwrap();
        assert!(again.written.is_empty());
        assert_eq!(again.skipped, 2);

        // Merged with a pool's day, in chain order.
        let pool = sample("2026-11-02", 0).pool;
        EventStore::new(dir.path())
            .write_day(
                "ethereum",
                pool,
                second,
                &[StoredLog {
                    block_number: 7_300,
                    block_time: genesis + 7_300 * 12,
                    log_index: 3,
                    tx_hash: B256::ZERO,
                    topics: vec![B256::repeat_byte(1)],
                    data: vec![],
                }],
                true,
            )
            .unwrap();
        let merged = read_merged_day(dir.path(), "ethereum", &[pool], second, true).unwrap();
        let order: Vec<(Address, u32)> = merged.iter().map(|l| (l.address, l.log_index)).collect();
        assert_eq!(order, vec![(MANAGER, 2), (pool, 3), (MANAGER, 4)]);
        assert!(read_merged_day(dir.path(), "ethereum", &[pool], third, false).is_err());
    }
}
