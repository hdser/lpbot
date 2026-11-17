//! `lpbot rank`: the universe's pools ranked by implied volatility, from the
//! data folder or straight from the node.

use std::collections::BTreeMap;

use std::path::Path;

use alloy_primitives::{Address, U256};
use anyhow::{anyhow, Result};
use lpbot_chain::uniswap_v3::{discover_pools, fetch_snapshot, fetch_swaps};
use lpbot_core::ranking::{rank, usd_prices, Floors, PoolStats, PriceEdge, RankedPool};
use lpbot_core::Config;

use lpbot_data::backfill::{first_block_at_or_after, ChainSource, NodeSource};
use lpbot_data::day::Day;
use lpbot_data::lake::{read_events, stored_pools, TimedEvent};
use lpbot_data::snapshot::{SnapshotStore, StoredSnapshot};
use lpbot_data::store::EventStore;
use lpbot_data::zone_map::DAY;
use lpbot_venues::uniswap_v3::events::PoolEvent;

use crate::chain_cmd::{Chain, Tokens};

/// Ethereum produces a block every 12 seconds. Only an estimate: the
/// ranking finds its window from block times.
pub const BLOCKS_PER_DAY: u64 = 7_200;
const HEAD_MARGIN: u64 = 5;

/// What is read from the chain for one pool.
#[derive(Debug, Clone)]
pub struct PoolReading {
    pub id: String,
    pub symbol0: String,
    pub symbol1: String,
    pub decimals0: u8,
    pub decimals1: u8,
    pub fee_pips: u32,
    pub sqrt_price_x96: U256,
    /// Liquidity in range at the current price.
    pub liquidity: u128,
    /// Token1 traded over the last day, in the token's smallest unit.
    pub volume1_raw_24h: U256,
    /// Token1 traded over the whole window, in the token's smallest unit.
    pub volume1_raw_window: U256,
    /// Days of history the window volume was read from: the window's
    /// length, or less when less of it is stored. Zero when none is.
    pub window_days: f64,
}

fn to_f64(value: U256) -> f64 {
    // Exact enough for reporting; never used to decide token amounts.
    value.to_string().parse().unwrap_or(f64::INFINITY)
}

impl PoolReading {
    /// How many token1 one token0 is worth, in whole tokens.
    pub fn price0_in_1(&self) -> f64 {
        let sqrt = to_f64(self.sqrt_price_x96) / 2f64.powi(96);
        sqrt * sqrt * 10f64.powi(i32::from(self.decimals0) - i32::from(self.decimals1))
    }

    /// In-range liquidity times the square root of price, in whole token1.
    pub fn l_sqrt_p_token1(&self) -> f64 {
        self.liquidity as f64 * to_f64(self.sqrt_price_x96)
            / 2f64.powi(96)
            / 10f64.powi(i32::from(self.decimals1))
    }

    fn volume1(&self, raw: U256) -> f64 {
        to_f64(raw) / 10f64.powi(i32::from(self.decimals1))
    }
}

/// Dollar prices of the tokens and dollar-denominated stats per pool. The
/// daily volume of each pool is averaged over the days it was read from.
pub fn build_stats(
    readings: &[PoolReading],
    stables: &[String],
) -> (BTreeMap<String, f64>, Vec<PoolStats>) {
    let edges: Vec<PriceEdge> = readings
        .iter()
        .map(|r| PriceEdge {
            base: r.symbol0.clone(),
            quote: r.symbol1.clone(),
            price: r.price0_in_1(),
            depth_quote: r.l_sqrt_p_token1(),
        })
        .collect();
    let prices = usd_prices(stables, &edges);

    let stats = readings
        .iter()
        .map(|r| {
            // A token without a price values the pool at zero, which excludes it.
            let usd1 = prices.get(&r.symbol1).copied().unwrap_or(0.0);
            let volume_per_day_long = if r.window_days > 0.0 {
                r.volume1(r.volume1_raw_window) * usd1 / r.window_days
            } else {
                0.0
            };
            PoolStats {
                id: r.id.clone(),
                fee: f64::from(r.fee_pips) / 1_000_000.0,
                volume_24h: r.volume1(r.volume1_raw_24h) * usd1,
                volume_per_day_long,
                l_sqrt_p: r.l_sqrt_p_token1() * usd1,
            }
        })
        .collect();
    (prices, stats)
}

/// Why a pool with no history in the window is not ranked.
pub const NO_HISTORY: &str = "no history stored in the ranking window";

/// Ranks the readings. A pool with no stored history in the window is not
/// ranked, whatever the floors: it is listed last with `NO_HISTORY`.
pub fn rank_readings(
    readings: &[PoolReading],
    stables: &[String],
    floors: &Floors,
) -> (BTreeMap<String, f64>, Vec<RankedPool>) {
    let (prices, stats) = build_stats(readings, stables);
    let (read, unread): (Vec<_>, Vec<_>) = readings
        .iter()
        .zip(stats)
        .partition(|(reading, _)| reading.window_days > 0.0);
    let mut ranked = rank(read.into_iter().map(|(_, stats)| stats).collect(), floors);
    ranked.extend(unread.into_iter().map(|(_, stats)| RankedPool {
        stats,
        iv_24h: None,
        iv_long: None,
        excluded: Some(NO_HISTORY.to_string()),
    }));
    (prices, ranked)
}

fn money(value: f64) -> String {
    if value >= 1e9 {
        format!("{:.2}B", value / 1e9)
    } else if value >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if value >= 1e3 {
        format!("{:.0}K", value / 1e3)
    } else {
        format!("{value:.0}")
    }
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{:.1}%", v * 100.0))
}

/// The ranking as lines of text, one pool per line.
pub fn render(ranked: &[RankedPool], window_days: u64) -> Vec<String> {
    let mut lines = vec![format!(
        "{:<3} {:<16} {:>8} {:>8} {:>10} {:>10} {:>10}  note",
        "#",
        "pool",
        format!("IV {window_days}d"),
        "IV 24h",
        format!("vol/d {window_days}d"),
        "vol 24h",
        "depth 1%"
    )];
    let mut position = 0;
    for pool in ranked {
        let place = if pool.excluded.is_none() {
            position += 1;
            position.to_string()
        } else {
            "-".to_string()
        };
        lines.push(format!(
            "{:<3} {:<16} {:>8} {:>8} {:>10} {:>10} {:>10}  {}",
            place,
            pool.stats.id,
            percent(pool.iv_long),
            percent(pool.iv_24h),
            money(pool.stats.volume_per_day_long),
            money(pool.stats.volume_24h),
            money(pool.stats.depth_1pct()),
            pool.excluded.as_deref().unwrap_or("")
        ));
    }
    lines
}

pub(crate) fn floors(config: &Config) -> Result<Floors> {
    let floors = config
        .universe
        .floors
        .ok_or_else(|| anyhow!("the config has no universe.floors"))?;
    if config.universe.usd_stables.is_empty() {
        return Err(anyhow!("the config has no universe.usd_stables"));
    }
    Ok(Floors {
        volume_per_day: floors.volume_usd_per_day,
        depth_1pct: floors.in_range_liquidity_usd,
    })
}

pub(crate) fn token_of(tokens: &Tokens, address: Address) -> Result<(String, u8)> {
    tokens
        .iter()
        .find(|(_, (a, _))| *a == address)
        .map(|(symbol, (_, decimals))| (symbol.clone(), *decimals))
        .ok_or_else(|| anyhow!("pool token {address:#x} is not in the universe"))
}

fn print(readings: &[PoolReading], config: &Config, days: u64) -> Result<()> {
    let floors = floors(config)?;
    let (prices, ranked) = rank_readings(readings, &config.universe.usd_stables, &floors);
    println!();
    for (symbol, price) in &prices {
        println!("{symbol}: {price:.2} USD");
    }
    println!();
    for line in render(&ranked, days) {
        println!("{line}");
    }
    println!();
    println!("Volume is valued at current prices. Depth 1% is the value within 1% of the price.");
    Ok(())
}

/// Where a window of whole days ending at the head lies, in blocks, found
/// from block times rather than an assumed block rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowBlocks {
    /// The first block after the window's start.
    pub first: u64,
    /// The first block after the window's first day.
    pub second_day: u64,
    /// The first block of the window's last day.
    pub last_day: u64,
}

/// The blocks of the `days` days that end at `head`, whose time is
/// `head_time`.
pub fn window_blocks(
    source: &impl ChainSource,
    head: u64,
    head_time: u64,
    days: u64,
) -> Result<WindowBlocks> {
    // Blocks strictly after a moment: the window holds (start, head_time].
    let after =
        |moment: u64, lo: u64| first_block_at_or_after(source, moment.saturating_add(1), lo, head);
    let start = head_time.saturating_sub(days * DAY);
    let first = after(start, 0)?;
    Ok(WindowBlocks {
        first,
        second_day: after(start + DAY, first)?,
        last_day: after(head_time.saturating_sub(DAY), first)?,
    })
}

/// Reads every universe pool from the node and prints the ranking.
pub fn run(chain: &Chain, config: &Config, days: u64, log_chunk: u64) -> Result<()> {
    if days == 0 {
        return Err(anyhow!("--days must be at least 1"));
    }
    floors(config)?;

    let head = chain.client.block_number()?.saturating_sub(HEAD_MARGIN);
    let (head_time, _) = chain.client.block_header(head)?;
    let source = NodeSource {
        client: &chain.client,
        head_margin: HEAD_MARGIN,
        log_chunk,
    };
    let blocks = window_blocks(&source, head, head_time, days)?;
    let from = blocks.first;

    let addresses: Vec<_> = chain.tokens.values().map(|(a, _)| *a).collect();
    let found = discover_pools(&chain.client, chain.factory, &addresses, head)?;
    println!(
        "reading {} pools on {} over blocks {from} to {head} ({days} days)",
        found.len(),
        chain.name
    );

    let mut readings = Vec::new();
    let mut swaps_in_first_day = 0usize;
    let mut swaps_in_last_day = 0usize;
    for pool in &found {
        let (symbol0, decimals0) = token_of(&chain.tokens, pool.token0)?;
        let (symbol1, decimals1) = token_of(&chain.tokens, pool.token1)?;
        let id = format!("{symbol0}/{symbol1} {}", pool.fee_pips as f64 / 10_000.0);
        let snapshot = fetch_snapshot(&chain.client, pool.address, head)?;
        let swaps = fetch_swaps(
            &chain.client,
            pool.address,
            from,
            head,
            log_chunk,
            |_, _| {},
        )?;

        let mut window = U256::ZERO;
        let mut last_day = U256::ZERO;
        for (block, swap) in &swaps {
            let amount = swap.amount1.unsigned_abs();
            window += amount;
            if *block >= blocks.last_day {
                last_day += amount;
                swaps_in_last_day += 1;
            }
            if *block < blocks.second_day {
                swaps_in_first_day += 1;
            }
        }
        println!("  {id}: {} swaps", swaps.len());

        readings.push(PoolReading {
            id,
            symbol0,
            symbol1,
            decimals0,
            decimals1,
            fee_pips: snapshot.fee_pips,
            sqrt_price_x96: snapshot.sqrt_price_x96,
            liquidity: snapshot.liquidity,
            volume1_raw_24h: last_day,
            volume1_raw_window: window,
            window_days: days as f64,
        });
    }

    print(&readings, config, days)?;
    if days > 1 && swaps_in_first_day == 0 && swaps_in_last_day > 0 {
        println!();
        println!(
            "warning: no swaps were found in the first day of the window. A pruned node \
             does not keep old logs; the {days}-day figures are then too low. Use an \
             archive node or a shorter --days."
        );
    }
    Ok(())
}

/// One stored pool as a ranking input, as it stood at `now`: price and
/// in-range liquidity from its last swap at or before `now` (or the
/// snapshot, when that is newer), volume from the swaps of the `days` days
/// ending at `now`. `covered_days` is how much of that window is stored;
/// the daily volume is averaged over it. Nothing after `now` is looked at.
pub fn reading_from_events(
    id: String,
    tokens: ((String, u8), (String, u8)),
    snapshot: &StoredSnapshot,
    events: &[TimedEvent],
    (now, days): (u64, u64),
    covered_days: f64,
) -> PoolReading {
    let ((symbol0, decimals0), (symbol1, decimals1)) = tokens;
    let state = &snapshot.state;
    let mut reading = PoolReading {
        id,
        symbol0,
        symbol1,
        decimals0,
        decimals1,
        fee_pips: state.fee_pips,
        sqrt_price_x96: state.sqrt_price_x96,
        liquidity: state.liquidity,
        volume1_raw_24h: U256::ZERO,
        volume1_raw_window: U256::ZERO,
        window_days: covered_days,
    };
    for event in events {
        let PoolEvent::Swap(swap) = &event.event else {
            continue;
        };
        if event.time > now {
            continue;
        }
        // A swap older than the snapshot does not tell the price after it.
        if event.block > snapshot.block {
            reading.sqrt_price_x96 = swap.sqrt_price_x96;
            reading.liquidity = swap.liquidity;
        }
        if event.time + days * DAY <= now {
            continue;
        }
        let amount = swap.amount1.unsigned_abs();
        reading.volume1_raw_window += amount;
        if event.time + DAY > now {
            reading.volume1_raw_24h += amount;
        }
    }
    reading
}

/// How many days of the `days` days ending at `now` a history stored from
/// the start of `first` to the end of `last` covers.
pub fn covered_days(first: Day, last: Day, now: u64, days: u64) -> f64 {
    let start = now.saturating_sub(days * DAY).max(first.start());
    let end = now.min(last.end());
    end.saturating_sub(start) as f64 / DAY as f64
}

/// Ranks the pools stored in the data folder, without touching a node.
pub fn run_local(
    root: &Path,
    chain_name: &str,
    tokens: &Tokens,
    config: &Config,
    days: u64,
) -> Result<()> {
    if days == 0 {
        return Err(anyhow!("--days must be at least 1"));
    }
    floors(config)?;
    let pools = stored_pools(root, chain_name)?;
    if pools.is_empty() {
        return Err(anyhow!(
            "no pools stored under {}; run `lpbot backfill` first, or pass --node to read the node",
            root.display()
        ));
    }

    let store = EventStore::new(root);
    let mut loaded = Vec::new();
    for pool in pools {
        let stored = store.days(chain_name, pool)?;
        let (Some((first, _)), Some((last, _))) = (stored.first(), stored.last()) else {
            continue;
        };
        let from = (*first).max(last.minus(days as u32));
        let snapshot = SnapshotStore::new(root)
            .latest_on_or_before(chain_name, pool, *last)?
            .ok_or_else(|| anyhow!("no snapshot stored for pool {pool:#x}"))?;
        let events = read_events(root, chain_name, pool, from, *last)?;
        loaded.push((snapshot, events, from, *last));
    }
    // Every pool is measured up to the same moment: the newest stored event.
    let now = loaded
        .iter()
        .filter_map(|(_, events, _, _)| events.last().map(|e| e.time))
        .max()
        .ok_or_else(|| anyhow!("the stored pools have no events"))?;

    println!(
        "ranking {} stored pool(s) under {} over the {days} days to {}",
        loaded.len(),
        root.display(),
        Day::of_time(now)
    );
    let mut readings = Vec::new();
    let mut warnings = Vec::new();
    for (snapshot, events, from, last) in &loaded {
        let token0 = token_of(tokens, snapshot.token0)?;
        let token1 = token_of(tokens, snapshot.token1)?;
        let id = format!(
            "{}/{} {}",
            token0.0,
            token1.0,
            snapshot.state.fee_pips as f64 / 10_000.0
        );
        println!("  {id}: {} events from {from} to {last}", events.len());
        let covered = covered_days(*from, *last, now, days);
        if let Some(warning) = short_history(&id, *from, *last, covered, days) {
            warnings.push(warning);
        }
        readings.push(reading_from_events(
            id,
            (token0, token1),
            snapshot,
            events,
            (now, days),
            covered,
        ));
    }
    print(&readings, config, days)?;
    for warning in warnings {
        println!();
        println!("{warning}");
    }
    Ok(())
}

/// A warning for a pool whose stored history covers less than the window,
/// or `None` when it covers all of it.
pub fn short_history(id: &str, first: Day, last: Day, covered: f64, days: u64) -> Option<String> {
    if covered + 1e-9 >= days as f64 {
        return None;
    }
    Some(if covered <= 0.0 {
        format!(
            "warning: {id} has no history in the {days} days ranked (what was read runs from \
             {first} to {last}), so it is not ranked. Run `lpbot backfill` to bring it up to date."
        )
    } else {
        format!(
            "warning: the history read for {id} runs from {first} to {last} and covers \
             {covered:.1} of the {days} days ranked; its daily volume is the average over those \
             {covered:.1} days. Run `lpbot backfill` to store the rest, or pass a shorter --days."
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_venues::uniswap_v3::pool::PoolSnapshot;
    use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

    fn usdc_weth(fee_pips: u32, liquidity: u128, volume_weth_per_day: u64) -> PoolReading {
        // One USDC (6 decimals) worth 0.0005 WETH (18 decimals) is a raw
        // price of 5e8, which is tick ln(5e8) / ln(1.0001) = 200,311.
        PoolReading {
            id: format!("USDC/WETH {}", fee_pips as f64 / 10_000.0),
            symbol0: "USDC".into(),
            symbol1: "WETH".into(),
            decimals0: 6,
            decimals1: 18,
            fee_pips,
            sqrt_price_x96: get_sqrt_ratio_at_tick(200_311).unwrap(),
            liquidity,
            volume1_raw_24h: U256::from(volume_weth_per_day)
                * U256::from(10u64).pow(U256::from(18u8)),
            volume1_raw_window: U256::from(volume_weth_per_day * 7)
                * U256::from(10u64).pow(U256::from(18u8)),
            window_days: 7.0,
        }
    }

    #[test]
    fn the_daily_volume_is_averaged_over_the_days_each_pool_was_read_from() {
        let liquidity = (2.5e23 / 5e8f64.sqrt()) as u128;
        let week = usdc_weth(500, liquidity, 50_000);
        // The same volume read from two days only is a busier pool.
        let mut two_days = week.clone();
        two_days.window_days = 2.0;
        let mut unread = week.clone();
        unread.window_days = 0.0;
        let (_, stats) = build_stats(&[week, two_days, unread], &["USDC".to_string()]);
        assert!((stats[0].volume_per_day_long / 100e6 - 1.0).abs() < 0.001);
        assert!((stats[1].volume_per_day_long / 350e6 - 1.0).abs() < 0.001);
        assert_eq!(stats[2].volume_per_day_long, 0.0);
    }

    #[test]
    fn a_pool_without_history_in_the_window_is_never_ranked() {
        let liquidity = (2.5e23 / 5e8f64.sqrt()) as u128;
        let mut unread = usdc_weth(500, liquidity, 50_000);
        unread.window_days = 0.0;
        unread.id = "unread".into();
        let read = usdc_weth(3000, liquidity, 50_000);
        // Floors of zero let anything through, but not a pool never read.
        let none = Floors {
            volume_per_day: 0.0,
            depth_1pct: 0.0,
        };
        let (_, ranked) = rank_readings(&[unread, read], &["USDC".to_string()], &none);
        assert_eq!(ranked[0].stats.id, "USDC/WETH 0.3");
        assert!(ranked[0].excluded.is_none());
        assert_eq!(ranked[1].stats.id, "unread");
        assert_eq!(ranked[1].excluded.as_deref(), Some(NO_HISTORY));
    }

    #[test]
    fn a_history_shorter_than_the_window_covers_part_of_it() {
        let day = |text: &str| text.parse::<Day>().unwrap();
        let now = day("2026-01-30").start() + 3_600;
        // Stored for the whole window and beyond.
        assert_eq!(
            covered_days(day("2026-01-01"), day("2026-01-30"), now, 7),
            7.0
        );
        // Stored from two days before `now`.
        let recent = covered_days(day("2026-01-28"), day("2026-01-30"), now, 7);
        assert!((recent - (2.0 + 1.0 / 24.0)).abs() < 1e-9, "{recent}");
        // Ending 25 hours before `now`, like a pool not brought up to date.
        let stale = covered_days(day("2026-01-01"), day("2026-01-28"), now, 7);
        assert!((stale - (6.0 - 1.0 / 24.0)).abs() < 1e-9, "{stale}");
        // Entirely before the window.
        assert_eq!(
            covered_days(day("2026-01-01"), day("2026-01-10"), now, 7),
            0.0
        );

        assert_eq!(
            short_history("p", day("2026-01-01"), day("2026-01-30"), 7.0, 7),
            None
        );
        let warning = short_history("p", day("2026-01-28"), day("2026-01-30"), 2.04, 7).unwrap();
        assert!(
            warning.contains("2.0 of the 7 days") && warning.contains("lpbot backfill"),
            "{warning}"
        );
        let warning = short_history("p", day("2026-01-01"), day("2026-01-10"), 0.0, 7).unwrap();
        assert!(warning.contains("not ranked"), "{warning}");
    }

    /// A chain whose blocks come every 12 seconds, except that every tenth
    /// slot is missed, so a day holds fewer than 7,200 blocks.
    struct Slots;

    impl ChainSource for Slots {
        fn head(&self) -> Result<(u64, u64), lpbot_chain::uniswap_v3::ChainError> {
            unreachable!()
        }
        fn block_time(&self, block: u64) -> Result<u64, lpbot_chain::uniswap_v3::ChainError> {
            Ok(1_700_000_000 + block * 12 + block / 9 * 12)
        }
        fn block_times(
            &self,
            blocks: &[u64],
        ) -> Result<Vec<u64>, lpbot_chain::uniswap_v3::ChainError> {
            blocks.iter().map(|b| self.block_time(*b)).collect()
        }
        fn logs(
            &self,
            _: Address,
            _: u64,
            _: u64,
        ) -> Result<Vec<lpbot_chain::eth::Log>, lpbot_chain::uniswap_v3::ChainError> {
            unreachable!()
        }
        fn snapshot(
            &self,
            _: Address,
            _: u64,
        ) -> Result<PoolSnapshot, lpbot_chain::uniswap_v3::ChainError> {
            unreachable!()
        }
    }

    #[test]
    fn the_node_window_is_found_by_block_time() {
        let head = 1_000_000;
        let time = |block| Slots.block_time(block).unwrap();
        let head_time = time(head);
        let blocks = window_blocks(&Slots, head, head_time, 7).unwrap();
        // Each boundary is the first block after its moment.
        for (block, moment) in [
            (blocks.first, head_time - 7 * DAY),
            (blocks.second_day, head_time - 6 * DAY),
            (blocks.last_day, head_time - DAY),
        ] {
            assert!(time(block) > moment && time(block - 1) <= moment, "{block}");
        }
        // Fewer blocks than 7,200 a day: counting blocks would reach back
        // further than seven days.
        assert!(head - blocks.first < 7 * BLOCKS_PER_DAY);
        assert!(blocks.first < blocks.second_day && blocks.second_day < blocks.last_day);
    }

    #[test]
    fn token_order_and_decimals_are_handled_when_reading_a_price() {
        let pool = usdc_weth(500, 1, 0);
        let weth_per_usdc = pool.price0_in_1();
        assert!(
            (1.0 / weth_per_usdc - 2_000.0).abs() < 0.5,
            "{}",
            1.0 / weth_per_usdc
        );
    }

    #[test]
    fn stats_are_in_dollars_and_reproduce_the_implied_volatility() {
        // Choose liquidity so that 5 million dollars sit within 1% of the price.
        // L * sqrt(P) must be 500 million dollars = 250,000 WETH = 2.5e23 wei,
        // and sqrt(P) is sqrt(5e8) = 22,360.68 in raw units.
        let liquidity = (2.5e23 / 5e8f64.sqrt()) as u128;
        // 100 million dollars a day is 50,000 WETH.
        let readings = vec![usdc_weth(500, liquidity, 50_000)];
        let (prices, stats) = build_stats(&readings, &["USDC".to_string()]);

        assert!((prices["WETH"] - 2_000.0).abs() < 0.5);
        let s = &stats[0];
        assert!(
            (s.depth_1pct() / 5e6 - 1.0).abs() < 0.001,
            "{}",
            s.depth_1pct()
        );
        assert!((s.volume_24h / 100e6 - 1.0).abs() < 0.001);
        assert!((s.volume_per_day_long / 100e6 - 1.0).abs() < 0.001);

        let ranked = rank(
            stats,
            &Floors {
                volume_per_day: 5e6,
                depth_1pct: 1e6,
            },
        );
        let iv = ranked[0].iv_long.unwrap();
        assert!((iv - 0.382).abs() < 0.002, "{iv}");
    }

    #[test]
    fn a_pool_whose_quote_token_has_no_price_is_excluded() {
        let mut pool = usdc_weth(500, 1_000_000_000_000_000_000, 50_000);
        pool.symbol0 = "AAA".into();
        pool.symbol1 = "BBB".into();
        let (prices, stats) = build_stats(&[pool], &["USDC".to_string()]);
        assert!(!prices.contains_key("BBB"));
        let ranked = rank(
            stats,
            &Floors {
                volume_per_day: 1.0,
                depth_1pct: 1.0,
            },
        );
        assert!(ranked[0].excluded.is_some());
    }

    #[test]
    fn the_table_numbers_ranked_pools_and_marks_excluded_ones() {
        let liquidity = (2.5e23 / 5e8f64.sqrt()) as u128;
        let readings = vec![
            usdc_weth(500, liquidity, 50_000),
            usdc_weth(3000, liquidity / 10_000, 10),
        ];
        let (_, stats) = build_stats(&readings, &["USDC".to_string()]);
        let lines = render(
            &rank(
                stats,
                &Floors {
                    volume_per_day: 5e6,
                    depth_1pct: 1e6,
                },
            ),
            7,
        );
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("IV 7d") && lines[0].contains("depth 1%"));
        assert!(lines[1].starts_with("1 ") && lines[1].contains("USDC/WETH 0.05"));
        assert!(
            lines[1].contains("38.") && lines[1].contains("100.0M") && lines[1].contains("5.0M")
        );
        assert!(lines[2].starts_with("- ") && lines[2].contains("below the floor"));
    }

    #[test]
    fn a_stored_pool_is_read_from_its_last_swap_and_the_swaps_of_the_window() {
        use alloy_primitives::I256;
        use lpbot_venues::uniswap_v3::pool::SwapEvent;

        let now = 100 * DAY;
        let snapshot = StoredSnapshot {
            pool: Address::ZERO,
            token0: Address::ZERO,
            token1: Address::ZERO,
            // Taken a little before the window.
            block: (now - 9 * DAY) / 12,
            day: Day::of_time(now - 9 * DAY),
            state: PoolSnapshot {
                fee_pips: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from(1u8),
                tick: 0,
                liquidity: 1,
                fee_growth_global0: U256::ZERO,
                fee_growth_global1: U256::ZERO,
                fee_protocol0: 0,
                fee_protocol1: 0,
                ticks: vec![],
            },
        };
        let swap = |time: u64, amount1: i64, liquidity: u128| TimedEvent {
            block: time / 12,
            time,
            log_index: 0,
            event: PoolEvent::Swap(SwapEvent {
                amount0: I256::ZERO,
                amount1: I256::try_from(amount1).unwrap(),
                sqrt_price_x96: U256::from(liquidity),
                liquidity,
                tick: 0,
            }),
        };
        let events = vec![
            swap(now - 8 * DAY, 1_000, 5),    // before the 7-day window
            swap(now - 7 * DAY + 1, -200, 6), // first second inside it
            swap(now - DAY, 30, 7),           // exactly a day old: not in the last 24 h
            swap(now - DAY + 1, -4, 8),       // inside the last 24 h
            swap(now, 1, 9),                  // the newest event
        ];
        let tokens = (("USDC".to_string(), 6), ("WETH".to_string(), 18));
        let read = |events: &[TimedEvent], now: u64| {
            reading_from_events("x".into(), tokens.clone(), &snapshot, events, (now, 7), 7.0)
        };
        let reading = read(&events, now);
        assert_eq!(reading.volume1_raw_window, U256::from(235u64));
        assert_eq!(reading.volume1_raw_24h, U256::from(5u64));
        assert_eq!(
            (reading.liquidity, reading.sqrt_price_x96),
            (9, U256::from(9u8))
        );
        assert_eq!((reading.decimals0, reading.decimals1), (6, 18));
        assert_eq!(reading.window_days, 7.0);

        // A swap after `now` tells nothing about the pool at `now`.
        let mut later = events.clone();
        later.push(swap(now + 1, 1_000_000, 10));
        assert_eq!(read(&later, now).volume1_raw_window, U256::from(235u64));
        assert_eq!(read(&later, now).liquidity, 9);
        // Measured a second before the newest swap, the one before it stands.
        assert_eq!(read(&events, now - 1).liquidity, 8);

        // Without swaps the snapshot's price and liquidity stand, and a
        // swap from before the snapshot does not replace them.
        assert_eq!(read(&[], now).liquidity, 1);
        let old = [swap(now - 10 * DAY, 3, 4)];
        let quiet = read(&old, now);
        assert_eq!((quiet.liquidity, quiet.volume1_raw_window), (1, U256::ZERO));
    }

    #[test]
    fn money_is_shortened_for_reading() {
        assert_eq!(money(1_234_000_000.0), "1.23B");
        assert_eq!(money(5_000_000.0), "5.0M");
        assert_eq!(money(12_345.0), "12K");
        assert_eq!(money(12.0), "12");
    }
}
