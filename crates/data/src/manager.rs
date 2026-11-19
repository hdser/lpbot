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
//! the book picks out its pools' NFTs by pairing. A day is about 3,000
//! logs, read in about three seconds from the local node.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// Stores the manager's logs for every day from `from` to `to`, both
/// included, that is not stored complete yet. A day is complete once the
/// next day's first block is at or behind the source's head; a later day
/// is not stored. Safe to run again: it fetches only what is missing.
pub fn store_manager_days(
    source: &impl ChainSource,
    lake: &Path,
    chain: &str,
    from: Day,
    to: Day,
    mut on_day: impl FnMut(Day, usize),
) -> Result<ManagerDays, BackfillError> {
    let store = EventStore::new(manager_root(lake));
    let mut report = ManagerDays::default();
    let missing: Vec<Day> = days(from, to)
        .filter(|day| store.day_state(chain, MANAGER, *day) != DayState::Complete)
        .collect();
    report.skipped = days(from, to).count() - missing.len();
    let Some(first) = missing.first().copied() else {
        return Ok(report);
    };
    let (head, _) = source.head()?;
    let wanted: Vec<B256> = manager_topics::all();
    let mut start = first_block_at_or_after(source, first.start(), 0, head)?;
    let mut day = first;
    while day <= to {
        let next_start = first_block_at_or_after(source, day.end(), start, head)?;
        if next_start > head {
            // Not over yet at the head.
            break;
        }
        if missing.contains(&day) {
            let logs: Vec<_> = if start < next_start {
                source
                    .logs(MANAGER, start, next_start - 1)?
                    .into_iter()
                    .filter(|log| log.topics.first().is_some_and(|t| wanted.contains(t)))
                    .collect()
            } else {
                Vec::new()
            };
            let times = block_times_on(source, &logs, day)?;
            let logs = stored_logs(logs, &times)?;
            store.write_day(chain, MANAGER, day, &logs, true)?;
            report.written.push((day, logs.len()));
            on_day(day, logs.len());
        }
        start = next_start;
        day = day.next();
    }
    Ok(report)
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
        let report =
            store_manager_days(&chain, dir.path(), "ethereum", first, third, |_, _| {}).unwrap();
        // The third day is not over at the head, so it is not stored.
        assert_eq!(report.written, vec![(first, 1), (second, 2)]);
        assert_eq!(
            first_missing_day(dir.path(), "ethereum", first, third),
            Some(third)
        );
        let again =
            store_manager_days(&chain, dir.path(), "ethereum", first, second, |_, _| {}).unwrap();
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
