//! Reading the stored history back as decoded pool events.

use std::fs;
use std::path::Path;

use alloy_primitives::Address;
use lpbot_venues::uniswap_v3::events::{decode_log, PoolEvent};
use lpbot_venues::uniswap_v3::pool::{Pool, Replay, SwapEvent};

use crate::day::Day;
use crate::snapshot::StoredSnapshot;
use crate::store::{EventStore, StoreError, StoreResult};
use crate::zone_map::PathPoint;

/// A pool event with where and when it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimedEvent {
    pub block: u64,
    /// Unix time of the block, in seconds.
    pub time: u64,
    pub log_index: u32,
    pub event: PoolEvent,
}

/// The state-changing events of a pool from `from` to `to`, both days
/// included, in chain order. A day that is not stored is an error.
pub fn read_events(
    root: &Path,
    chain: &str,
    pool: Address,
    from: Day,
    to: Day,
) -> StoreResult<Vec<TimedEvent>> {
    let mut events = Vec::new();
    for log in EventStore::new(root).read_days(chain, pool, from, to)? {
        match decode_log(&log.topics, &log.data) {
            Ok(Some(event)) => events.push(TimedEvent {
                block: log.block_number,
                time: log.block_time,
                log_index: log.log_index,
                event,
            }),
            Ok(None) => {}
            Err(e) => {
                return Err(StoreError::BadFile {
                    path: root.to_path_buf(),
                    reason: format!(
                        "pool {pool:#x}, block {}, log {}: {e}",
                        log.block_number, log.log_index
                    ),
                })
            }
        }
    }
    Ok(events)
}

/// Rebuilds a pool from a stored snapshot and the stored events after it,
/// up to the end of day `to`. Every event must replay exactly. Returns the
/// pool and the number of events applied.
pub fn replay_from_snapshot(
    root: &Path,
    chain: &str,
    snapshot: StoredSnapshot,
    to: Day,
) -> StoreResult<(Pool, usize)> {
    let address = snapshot.pool;
    let events = read_events(root, chain, address, snapshot.day, to)?;
    let mut pool = Pool::from_snapshot(snapshot.state);
    for e in &events {
        pool.apply_event(&e.event, Replay::Strict, &mut ())
            .map_err(|error| StoreError::Replay {
                pool: address,
                block: e.block,
                log_index: e.log_index,
                reason: error.to_string(),
            })?;
    }
    Ok((pool, events.len()))
}

/// The swaps among some events, each with its block time.
pub fn swaps(events: &[TimedEvent]) -> impl Iterator<Item = (u64, &SwapEvent)> {
    events.iter().filter_map(|e| match &e.event {
        PoolEvent::Swap(swap) => Some((e.time, swap)),
        _ => None,
    })
}

/// The price path the zone map is built from.
pub fn price_path(events: &[TimedEvent]) -> Vec<PathPoint> {
    swaps(events)
        .map(|(time, swap)| PathPoint {
            time,
            tick: swap.tick,
            volume1: swap
                .amount1
                .unsigned_abs()
                .to_string()
                .parse()
                .unwrap_or(f64::INFINITY),
            liquidity: swap.liquidity as f64,
        })
        .collect()
}

/// Pools that have events stored for a chain.
pub fn stored_pools(root: &Path, chain: &str) -> StoreResult<Vec<Address>> {
    let dir = root.join("events").join(format!("chain={chain}"));
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let io = |source| StoreError::Io {
        path: dir.clone(),
        source,
    };
    let mut pools = Vec::new();
    for entry in fs::read_dir(&dir).map_err(io)? {
        let name = entry.map_err(io)?.file_name();
        if let Some(pool) = name
            .to_str()
            .and_then(|n| n.strip_prefix("pool="))
            .and_then(|a| a.parse::<Address>().ok())
        {
            pools.push(pool);
        }
    }
    pools.sort_unstable();
    Ok(pools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testing::TempDir;
    use crate::store::StoredLog;
    use alloy_primitives::{B256, I256, U256};
    use lpbot_venues::uniswap_v3::events::topics;

    fn pool() -> Address {
        "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
            .parse()
            .unwrap()
    }

    fn word_i(value: i64) -> [u8; 32] {
        I256::try_from(value).unwrap().to_be_bytes()
    }

    fn swap_log(block: u64, time: u64, amount1: i64, tick: i32) -> StoredLog {
        let mut data = Vec::new();
        data.extend(word_i(-5));
        data.extend(word_i(amount1));
        data.extend(U256::from(1u64 << 40).to_be_bytes::<32>());
        data.extend(U256::from(777u64).to_be_bytes::<32>());
        data.extend(word_i(i64::from(tick)));
        StoredLog {
            block_number: block,
            block_time: time,
            log_index: 1,
            tx_hash: B256::ZERO,
            topics: vec![topics::swap(), B256::ZERO, B256::ZERO],
            data,
        }
    }

    #[test]
    fn stored_logs_come_back_as_events_and_a_price_path() {
        let dir = TempDir::new("lake");
        let store = EventStore::new(dir.path());
        let day: Day = "2026-11-01".parse().unwrap();
        let unknown = StoredLog {
            topics: vec![B256::repeat_byte(0x77)],
            ..swap_log(10, day.start() + 5, 0, 0)
        };
        store
            .write_day(
                "ethereum",
                pool(),
                day,
                &[
                    swap_log(10, day.start() + 5, 300, -12),
                    unknown,
                    swap_log(11, day.start() + 17, -450, 8),
                ],
                true,
            )
            .unwrap();

        let events = read_events(dir.path(), "ethereum", pool(), day, day).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!((events[0].block, events[0].time), (10, day.start() + 5));
        assert_eq!(
            price_path(&events),
            vec![
                PathPoint {
                    time: day.start() + 5,
                    tick: -12,
                    volume1: 300.0,
                    liquidity: 777.0
                },
                PathPoint {
                    time: day.start() + 17,
                    tick: 8,
                    volume1: 450.0,
                    liquidity: 777.0
                },
            ]
        );
        assert_eq!(swaps(&events).count(), 2);
        assert_eq!(stored_pools(dir.path(), "ethereum").unwrap(), vec![pool()]);
        assert!(stored_pools(dir.path(), "base").unwrap().is_empty());

        // A missing day is an error, and so is a log that cannot be decoded.
        assert!(read_events(dir.path(), "ethereum", pool(), day, day.next()).is_err());
        let mut broken = swap_log(12, day.end() + 1, 1, 1);
        broken.data.truncate(40);
        store
            .write_day("ethereum", pool(), day.next(), &[broken], true)
            .unwrap();
        let error = read_events(dir.path(), "ethereum", pool(), day, day.next()).unwrap_err();
        assert!(error.to_string().contains("block 12"), "{error}");
    }
}
