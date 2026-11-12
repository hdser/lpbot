//! A simulated pool history, for trying the whole pipeline without a node.
//!
//! It runs a real pool replica through swaps, mints and burns along a
//! price that swings around a centre, and records the events the pool
//! would have emitted. The result is stored exactly like a backfill from
//! the chain, so everything downstream reads it the same way. The price is
//! made up; no conclusion about a strategy should be drawn from it.

use std::path::Path;

use alloy_primitives::{keccak256, Address, I256};
use lpbot_venues::uniswap_v3::events::{encode_log, PoolEvent};
use lpbot_venues::uniswap_v3::pool::{Pool, PoolError, SwapEvent};
use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

use crate::backfill::PoolRef;
use crate::day::{Day, DAY_SECONDS};
use crate::lake::TimedEvent;
use crate::snapshot::{SnapshotStore, StoredSnapshot};
use crate::store::{EventStore, StoreResult, StoredLog};

/// Seconds between blocks of the simulated chain.
const BLOCK_SECONDS: u64 = 12;

/// How the simulated history is shaped.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthConfig {
    /// The same seed always gives the same history.
    pub seed: u64,
    pub start: Day,
    pub days: u32,
    pub fee_pips: u32,
    pub tick_spacing: i32,
    /// Tick the price swings around.
    pub center_tick: i32,
    pub swaps_per_day: u32,
    /// Number of the first block of the first day.
    pub first_block: u64,
}

impl Default for SynthConfig {
    fn default() -> Self {
        Self {
            seed: 1,
            start: Day(20_454), // 2026-01-01
            days: 30,
            fee_pips: 500,
            tick_spacing: 10,
            // 2,000 dollars per ETH in a USDC/WETH pool.
            center_tick: 200_310,
            swaps_per_day: 480,
            first_block: 20_000_000,
        }
    }
}

/// A simulated history: where the pool started, what happened, where it ended.
#[derive(Debug, Clone)]
pub struct SynthHistory {
    pub snapshot: StoredSnapshot,
    pub events: Vec<TimedEvent>,
    /// The pool after the last event.
    pub end: Pool,
    pub last_day: Day,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // SplitMix64.
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A whole number from `low` to `high`, both included.
    fn between(&mut self, low: i64, high: i64) -> i64 {
        low + (self.next() % (high - low + 1) as u64) as i64
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// Runs the simulated pool and records its events.
pub fn generate(config: &SynthConfig, pool: PoolRef) -> Result<SynthHistory, PoolError> {
    let spacing = config.tick_spacing;
    let round = |tick: i32| tick.div_euclid(spacing) * spacing;
    let center = round(config.center_tick);
    let mut rng = Rng(config.seed);

    // Liquidity in place before the history begins: a wide base and a
    // tighter layer around the centre.
    let mut replica = Pool::new(config.fee_pips, spacing);
    replica.initialize(get_sqrt_ratio_at_tick(center)?)?;
    replica.mint(
        center - 2_000 * spacing,
        center + 2_000 * spacing,
        5_000_000_000_000_000_000,
    )?;
    replica.mint(
        center - 60 * spacing,
        center + 60 * spacing,
        10_000_000_000_000_000_000,
    )?;
    let snapshot = StoredSnapshot {
        pool: pool.address,
        token0: pool.token0,
        token1: pool.token1,
        block: config.first_block - 1,
        day: config.start,
        state: replica.snapshot(),
    };

    let start_time = config.start.start();
    let total_seconds = u64::from(config.days) * DAY_SECONDS;
    let step = (DAY_SECONDS / u64::from(config.swaps_per_day.max(1))).max(BLOCK_SECONDS);
    let huge = I256::try_from(10u128.pow(30)).expect("fits");

    let mut events = Vec::new();
    let mut open_positions: Vec<(i32, i32, u128)> = Vec::new();
    let mut last_block = 0u64;
    let mut index_in_block = 0u32;
    let mut record = |elapsed: u64, event: PoolEvent| {
        let slot = elapsed / BLOCK_SECONDS;
        let block = config.first_block + slot;
        index_in_block = if block == last_block {
            index_in_block + 1
        } else {
            0
        };
        last_block = block;
        events.push(TimedEvent {
            block,
            time: start_time + slot * BLOCK_SECONDS,
            log_index: index_in_block,
            event,
        });
    };

    let mut elapsed = step / 2;
    while elapsed < total_seconds {
        // A fast swing of 1.5% every six hours on top of a slow one of 4%
        // every five days, with some noise.
        let t = elapsed as f64;
        let fast = 150.0 * (std::f64::consts::TAU * t / 21_600.0).sin();
        let slow = 400.0 * (std::f64::consts::TAU * t / 432_000.0).sin();
        let target = center + (fast + slow) as i32 + rng.between(-25, 25) as i32;

        // Direction comes from prices, not ticks: after a swap down onto an
        // initialized tick the pool's tick is one below its price.
        let limit = get_sqrt_ratio_at_tick(target)?;
        if limit != replica.sqrt_price_x96() {
            let zero_for_one = limit < replica.sqrt_price_x96();
            let amounts = replica.swap(zero_for_one, huge, limit, &mut ())?;
            if !(amounts.amount0.is_zero() && amounts.amount1.is_zero()) {
                record(
                    elapsed,
                    PoolEvent::Swap(SwapEvent {
                        amount0: amounts.amount0,
                        amount1: amounts.amount1,
                        sqrt_price_x96: replica.sqrt_price_x96(),
                        liquidity: replica.liquidity(),
                        tick: replica.tick(),
                    }),
                );
            }
        }

        // Other providers come and go.
        if rng.chance(3) {
            let lower = round(replica.tick() + rng.between(-300, 300) as i32);
            let upper = lower + spacing * rng.between(1, 20) as i32;
            let amount = rng.between(1, 50) as u128 * 100_000_000_000_000_000;
            let (amount0, amount1) = replica.mint(lower, upper, amount)?;
            open_positions.push((lower, upper, amount));
            record(
                elapsed,
                PoolEvent::Mint {
                    tick_lower: lower,
                    tick_upper: upper,
                    amount,
                    amount0,
                    amount1,
                },
            );
        }
        if rng.chance(3) && !open_positions.is_empty() {
            let (lower, upper, amount) = open_positions.remove(0);
            let (amount0, amount1) = replica.burn(lower, upper, amount)?;
            record(
                elapsed,
                PoolEvent::Burn {
                    tick_lower: lower,
                    tick_upper: upper,
                    amount,
                    amount0,
                    amount1,
                },
            );
        }
        elapsed += step;
    }

    Ok(SynthHistory {
        snapshot,
        events,
        end: replica,
        last_day: Day(config.start.0 + config.days.max(1) - 1),
    })
}

/// The stored form of an event, as a backfill would have written it.
pub fn stored_log(event: &TimedEvent) -> StoredLog {
    let (topics, data) = encode_log(&event.event);
    let mut seed = event.block.to_be_bytes().to_vec();
    seed.extend(event.log_index.to_be_bytes());
    StoredLog {
        block_number: event.block,
        block_time: event.time,
        log_index: event.log_index,
        tx_hash: keccak256(seed),
        topics,
        data,
    }
}

/// Stores a simulated history: its snapshot and one complete file per day.
pub fn write_history(root: &Path, chain: &str, history: &SynthHistory) -> StoreResult<()> {
    SnapshotStore::new(root).write(chain, &history.snapshot)?;
    let store = EventStore::new(root);
    let pool: Address = history.snapshot.pool;
    let mut day = history.snapshot.day;
    let mut rest = history.events.as_slice();
    while day <= history.last_day {
        let count = rest.partition_point(|e| e.time < day.end());
        let logs: Vec<StoredLog> = rest[..count].iter().map(stored_log).collect();
        store.write_day(chain, pool, day, &logs, true)?;
        rest = &rest[count..];
        day = day.next();
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::lake::{price_path, read_events, replay_from_snapshot};
    use crate::store::testing::TempDir;
    use crate::zone_map::build_zone_map;

    pub fn pool() -> PoolRef {
        PoolRef {
            address: "0x5157000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            token0: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
                .parse()
                .unwrap(),
            token1: "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
                .parse()
                .unwrap(),
        }
    }

    pub fn small() -> SynthConfig {
        SynthConfig {
            days: 3,
            swaps_per_day: 240,
            ..Default::default()
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_history_and_another_seed_does_not() {
        let a = generate(&small(), pool()).unwrap();
        let b = generate(&small(), pool()).unwrap();
        assert_eq!(a.events, b.events);
        assert_eq!(a.end, b.end);
        let other = generate(&SynthConfig { seed: 2, ..small() }, pool()).unwrap();
        assert_ne!(a.events, other.events);
    }

    #[test]
    fn the_history_is_ordered_dated_and_made_of_all_event_kinds() {
        let config = small();
        let history = generate(&config, pool()).unwrap();
        let (mut swaps, mut mints, mut burns) = (0, 0, 0);
        for e in &history.events {
            match e.event {
                PoolEvent::Swap(_) => swaps += 1,
                PoolEvent::Mint { .. } => mints += 1,
                PoolEvent::Burn { .. } => burns += 1,
                _ => panic!("unexpected event"),
            }
            assert!(e.time >= config.start.start());
            assert!(e.time < Day(config.start.0 + config.days).start());
            assert_eq!(
                e.time,
                config.start.start() + (e.block - config.first_block) * 12
            );
        }
        assert!(
            swaps > 600 && mints > 5 && burns > 3,
            "{swaps} {mints} {burns}"
        );
        assert!(history
            .events
            .windows(2)
            .all(|w| { (w[0].block, w[0].log_index) < (w[1].block, w[1].log_index) }));
        assert_eq!(history.snapshot.block, config.first_block - 1);
        assert_eq!(history.last_day, Day(config.start.0 + 2));
        // The price stays around the centre.
        let ticks: Vec<i32> = price_path(&history.events).iter().map(|p| p.tick).collect();
        assert!(ticks.iter().all(|t| (t - 200_310).abs() < 700));
        assert!(ticks.iter().max().unwrap() - ticks.iter().min().unwrap() > 300);
    }

    #[test]
    fn a_stored_history_replays_to_the_pool_that_produced_it() {
        let dir = TempDir::new("synth");
        let history = generate(&small(), pool()).unwrap();
        write_history(dir.path(), "ethereum", &history).unwrap();

        let read = read_events(
            dir.path(),
            "ethereum",
            pool().address,
            history.snapshot.day,
            history.last_day,
        )
        .unwrap();
        assert_eq!(read, history.events);

        let snapshot = SnapshotStore::new(dir.path())
            .latest_on_or_before("ethereum", pool().address, history.last_day)
            .unwrap()
            .unwrap();
        let (replica, applied) =
            replay_from_snapshot(dir.path(), "ethereum", snapshot, history.last_day).unwrap();
        assert_eq!(applied, history.events.len());
        assert!(replica.differences(&history.end).is_empty());
    }

    #[test]
    fn the_zone_map_of_the_history_finds_ranges_that_are_crossed() {
        let history = generate(&small(), pool()).unwrap();
        let path = price_path(&history.events);
        let now = history.last_day.end();
        let rows = build_zone_map("synthetic", 0.0005, 10, &path, &[10, 20, 40], &[3], now);
        assert!(!rows.is_empty());
        // The fast swing crosses the middle about eight times a day.
        assert!(rows[0].passes_per_day > 4.0, "{:?}", rows[0]);
        assert!(rows[0].implied_vol.is_some() || rows[0].time_in_range == 0.0);
    }
}
