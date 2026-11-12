//! `lpbot zones`: the zone map of one pool, from the data folder or the node.

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Result};
use lpbot_chain::clock::BlockClock;
use lpbot_chain::uniswap_v3::{discover_pools, fetch_snapshot, fetch_swaps};
use lpbot_core::{Config, Tick};
use lpbot_data::lake::{price_path, read_events};
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::EventStore;
use lpbot_data::zone_map::{build_zone_map, PathPoint, ZoneRow, DAY};
use lpbot_venues::uniswap_v3::pool::SwapEvent;
use std::path::Path;

use crate::chain_cmd::{Chain, Tokens};
use crate::rank_cmd::BLOCKS_PER_DAY;

const HEAD_MARGIN: u64 = 5;
/// Blocks between the headers read for block times: about two hours.
const CLOCK_STEP: u64 = 600;

/// Turns a pool's swaps into its price path. Swaps the clock cannot place
/// in time are an error, never dropped.
pub fn swap_path(swaps: &[(u64, SwapEvent)], clock: &BlockClock) -> Result<Vec<PathPoint>> {
    swaps
        .iter()
        .map(|(block, swap)| {
            Ok(PathPoint {
                time: clock
                    .time_of(*block)
                    .ok_or_else(|| anyhow!("no block time for block {block}"))?,
                tick: swap.tick,
                volume1: swap
                    .amount1
                    .unsigned_abs()
                    .to_string()
                    .parse()
                    .unwrap_or(f64::INFINITY),
                liquidity: swap.liquidity as f64,
            })
        })
        .collect()
}

/// How ticks are shown as prices.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceView {
    pub decimals0: u8,
    pub decimals1: u8,
    /// Show token0 per token1 instead of token1 per token0.
    pub invert: bool,
    /// For example "USDC per WETH".
    pub label: String,
}

impl PriceView {
    /// Quotes in the dollar stable when exactly one side is one, so an
    /// ETH pool reads in dollars per ETH whatever the token order.
    pub fn new(
        symbol0: &str,
        decimals0: u8,
        symbol1: &str,
        decimals1: u8,
        stables: &[String],
    ) -> Self {
        let is_stable = |s: &str| stables.iter().any(|x| x == s);
        let invert = is_stable(symbol0) && !is_stable(symbol1);
        let label = if invert {
            format!("{symbol0} per {symbol1}")
        } else {
            format!("{symbol1} per {symbol0}")
        };
        Self {
            decimals0,
            decimals1,
            invert,
            label,
        }
    }

    pub fn price(&self, tick: Tick) -> f64 {
        let raw = 1.0001f64.powi(tick);
        let price = raw * 10f64.powi(i32::from(self.decimals0) - i32::from(self.decimals1));
        if self.invert {
            1.0 / price
        } else {
            price
        }
    }

    /// A tick range as a price range, low price first.
    pub fn range(&self, lower: Tick, upper: Tick) -> String {
        let (a, b) = (self.price(lower), self.price(upper));
        format!("{} - {}", number(a.min(b)), number(a.max(b)))
    }
}

/// A price with about five significant digits.
fn number(value: f64) -> String {
    if value >= 10_000.0 {
        format!("{value:.0}")
    } else if value >= 100.0 {
        format!("{value:.1}")
    } else if value >= 1.0 {
        format!("{value:.3}")
    } else {
        format!("{value:.6}")
    }
}

fn duration(seconds: Option<u64>) -> String {
    match seconds {
        None => "-".to_string(),
        Some(s) if s < 90 * 60 => format!("{:.0}m", s as f64 / 60.0),
        Some(s) if s < 2 * DAY => format!("{:.1}h", s as f64 / 3_600.0),
        Some(s) => format!("{:.1}d", s as f64 / DAY as f64),
    }
}

fn percent(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("{:.digits$}%", v * 100.0))
}

/// The zone map as text: for each lookback, its `top` busiest ranges.
pub fn render(rows: &[ZoneRow], lookbacks: &[u32], top: usize, view: &PriceView) -> Vec<String> {
    let mut lines = Vec::new();
    for &days in lookbacks {
        let of_lookback: Vec<&ZoneRow> = rows.iter().filter(|r| r.lookback_days == days).collect();
        let Some(first) = of_lookback.first() else {
            lines.push(format!("{days}-day lookback: no data"));
            lines.push(String::new());
            continue;
        };
        let covered = if first.observed_days + 0.01 < f64::from(days) {
            format!(" (data covers {:.1} days)", first.observed_days)
        } else {
            String::new()
        };
        lines.push(format!(
            "{days}-day lookback{covered}: {} of {} ranges",
            top.min(of_lookback.len()),
            of_lookback.len()
        ));
        lines.push(format!(
            "{:<19} {:<21} {:>8} {:>9} {:>8} {:>8} {:>8} {:>8} {:>9} {:>7}",
            "ticks",
            view.label,
            "passes/d",
            "up/down",
            "fees/d",
            "in range",
            "back<1d",
            "back p50",
            "pass back",
            "IV"
        ));
        for row in of_lookback.iter().take(top) {
            lines.push(format!(
                "{:<19} {:<21} {:>8.2} {:>9} {:>8} {:>8} {:>8} {:>8} {:>9} {:>7}",
                format!("{}..{}", row.tick_lower, row.tick_upper),
                view.range(row.tick_lower, row.tick_upper),
                row.passes_per_day,
                format!("{}/{}", row.passes_up, row.passes_down),
                percent(Some(row.pass_yield_per_day), 3),
                percent(Some(row.time_in_range), 1),
                percent(row.return_odds_1d, 0),
                duration(row.return_p50_seconds),
                duration(row.pass_back_p50_seconds),
                percent(row.implied_vol, 0),
            ));
        }
        lines.push(String::new());
    }
    lines
}

pub struct ZonesArgs<'a> {
    pub pool: Address,
    pub lookbacks: &'a [u32],
    /// Range widths as multiples of the pool's tick spacing.
    pub width_steps: &'a [u32],
    pub top: usize,
    pub log_chunk: u64,
}

/// A pool's price path with what is needed to show its zone map.
pub struct ZoneInput {
    pub name: String,
    pub view: PriceView,
    /// Fee tier as a fraction.
    pub fee: f64,
    pub tick_spacing: Tick,
    pub tick_now: Tick,
    pub path: Vec<PathPoint>,
    /// Unix time the lookbacks end at.
    pub now: u64,
}

fn check(args: &ZonesArgs) -> Result<u32> {
    let Some(&longest) = args.lookbacks.iter().max() else {
        bail!("--days needs at least one lookback");
    };
    if longest == 0 || args.width_steps.is_empty() || args.width_steps.contains(&0) {
        bail!("--days and --widths take positive numbers");
    }
    Ok(longest)
}

fn token_of(tokens: &Tokens, address: Address) -> Result<(String, u8)> {
    tokens
        .iter()
        .find(|(_, (a, _))| *a == address)
        .map(|(symbol, (_, decimals))| (symbol.clone(), *decimals))
        .ok_or_else(|| anyhow!("pool token {address:#x} is not in the universe"))
}

/// Reads a pool's swaps over the longest lookback from the node.
pub fn load_from_node(chain: &Chain, config: &Config, args: &ZonesArgs) -> Result<ZoneInput> {
    let longest = check(args)?;
    let head = chain.client.block_number()?.saturating_sub(HEAD_MARGIN);
    let from = head.saturating_sub(u64::from(longest) * BLOCKS_PER_DAY);

    let addresses: Vec<_> = chain.tokens.values().map(|(a, _)| *a).collect();
    let found = discover_pools(&chain.client, chain.factory, &addresses, head)?;
    let pool = found
        .iter()
        .find(|p| p.address == args.pool)
        .ok_or_else(|| {
            anyhow!(
                "{:#x} is not a pool of the universe; `lpbot pools` lists them",
                args.pool
            )
        })?;
    let (symbol0, decimals0) = token_of(&chain.tokens, pool.token0)?;
    let (symbol1, decimals1) = token_of(&chain.tokens, pool.token1)?;
    let name = format!("{symbol0}/{symbol1} {}", pool.fee_pips as f64 / 10_000.0);

    let snapshot = fetch_snapshot(&chain.client, pool.address, head)?;
    println!(
        "{name} on {}: reading swaps of blocks {from} to {head} ({longest} days)",
        chain.name
    );
    let swaps = fetch_swaps(
        &chain.client,
        pool.address,
        from + 1,
        head,
        args.log_chunk,
        |_, _| {},
    )?;
    let clock = chain.client.block_clock(from, head, CLOCK_STEP)?;
    let now = clock
        .time_of(head)
        .ok_or_else(|| anyhow!("no block time for block {head}"))?;
    Ok(ZoneInput {
        name,
        view: PriceView::new(
            &symbol0,
            decimals0,
            &symbol1,
            decimals1,
            &config.universe.usd_stables,
        ),
        fee: f64::from(snapshot.fee_pips) / 1_000_000.0,
        tick_spacing: snapshot.tick_spacing,
        tick_now: snapshot.tick,
        path: swap_path(&swaps, &clock)?,
        now,
    })
}

/// Reads a pool's swaps over the longest lookback from the data folder.
pub fn load_local(
    root: &Path,
    chain_name: &str,
    tokens: &Tokens,
    config: &Config,
    args: &ZonesArgs,
) -> Result<ZoneInput> {
    let longest = check(args)?;
    let days = EventStore::new(root).days(chain_name, args.pool)?;
    let (Some((first, _)), Some((last, _))) = (days.first(), days.last()) else {
        bail!(
            "no events stored for pool {:#x} under {}; run `lpbot backfill --pool {:#x}` first",
            args.pool,
            root.display(),
            args.pool
        );
    };
    let from = (*first).max(last.minus(longest));
    let snapshot = SnapshotStore::new(root)
        .latest_on_or_before(chain_name, args.pool, *last)?
        .ok_or_else(|| anyhow!("no snapshot stored for pool {:#x}", args.pool))?;
    let (symbol0, decimals0) = token_of(tokens, snapshot.token0)?;
    let (symbol1, decimals1) = token_of(tokens, snapshot.token1)?;
    let name = format!(
        "{symbol0}/{symbol1} {}",
        snapshot.state.fee_pips as f64 / 10_000.0
    );
    println!(
        "{name}: stored events from {from} to {last}, under {}",
        root.display()
    );
    let path = price_path(&read_events(root, chain_name, args.pool, from, *last)?);
    let (tick_now, now) = path
        .last()
        .map_or((snapshot.state.tick, last.start()), |p| (p.tick, p.time));
    Ok(ZoneInput {
        name,
        view: PriceView::new(
            &symbol0,
            decimals0,
            &symbol1,
            decimals1,
            &config.universe.usd_stables,
        ),
        fee: f64::from(snapshot.state.fee_pips) / 1_000_000.0,
        tick_spacing: snapshot.state.tick_spacing,
        tick_now,
        path,
        now,
    })
}

/// Builds the zone map of a loaded pool and prints it.
pub fn print(input: &ZoneInput, args: &ZonesArgs) -> Result<()> {
    let longest = check(args)?;
    println!(
        "{} swaps; price now {} {} (tick {})",
        input.path.len(),
        number(input.view.price(input.tick_now)),
        input.view.label,
        input.tick_now
    );
    println!();

    let widths: Vec<Tick> = args
        .width_steps
        .iter()
        .map(|&steps| steps as Tick * input.tick_spacing)
        .collect();
    let rows = build_zone_map(
        &input.name,
        input.fee,
        input.tick_spacing,
        &input.path,
        &widths,
        args.lookbacks,
        input.now,
    );
    for line in render(&rows, args.lookbacks, args.top, &input.view) {
        println!("{line}");
    }
    println!(
        "passes/d: full crossings of the range per day. fees/d: passes times the fee tier, \
         the fee a position there earned per day on its value. back<1d: chance price was \
         back inside within a day of leaving. back p50: median time outside. pass back: \
         median wait between a full pass and the pass back. IV: implied volatility of the zone."
    );

    if let (Some(first), true) = (input.path.first(), longest > 1) {
        if first.time > input.now.saturating_sub(u64::from(longest) * DAY) + DAY {
            println!();
            println!(
                "note: the data starts more than a day into the {longest}-day window. From a \
                 node, that means old logs are pruned; from the data folder, that the backfill \
                 is shorter than the lookback."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{I256, U256};

    fn swap(tick: Tick, amount1: i64) -> SwapEvent {
        SwapEvent {
            amount0: I256::ZERO,
            amount1: I256::try_from(amount1).unwrap(),
            sqrt_price_x96: U256::ZERO,
            liquidity: 5_000,
            tick,
        }
    }

    #[test]
    fn swaps_become_path_points_with_block_times_and_absolute_volume() {
        let clock = BlockClock::new(vec![(100, 1_000), (200, 2_200)]);
        let path = swap_path(&[(100, swap(7, -300)), (150, swap(9, 450))], &clock).unwrap();
        assert_eq!(
            path,
            vec![
                PathPoint {
                    time: 1_000,
                    tick: 7,
                    volume1: 300.0,
                    liquidity: 5_000.0
                },
                PathPoint {
                    time: 1_600,
                    tick: 9,
                    volume1: 450.0,
                    liquidity: 5_000.0
                },
            ]
        );
        assert!(swap_path(&[(1, swap(0, 1))], &BlockClock::new(vec![])).is_err());
    }

    #[test]
    fn prices_read_in_the_stable_whatever_the_token_order() {
        let stables = vec!["USDC".to_string(), "USDT".to_string()];
        // Mainnet order: USDC is token0. Tick 200,311 is 2,000 dollars per ETH.
        let view = PriceView::new("USDC", 6, "WETH", 18, &stables);
        assert!(view.invert);
        assert_eq!(view.label, "USDC per WETH");
        assert!((view.price(200_311) - 2_000.0).abs() < 0.5);
        // A higher tick is a lower dollar price; the range still reads low to high.
        assert!(view.price(200_321) < view.price(200_311));
        assert_eq!(view.range(200_311, 200_321), "1998.0 - 2000.0");

        // The other order: the stable is token1, nothing to invert.
        let view = PriceView::new("WETH", 18, "USDT", 6, &stables);
        assert!(!view.invert);
        assert_eq!(view.label, "USDT per WETH");
        assert!((view.price(-200_311) - 2_000.0).abs() < 0.5);
        assert!(view.price(-200_301) > view.price(-200_311));

        // Neither or both stable: token1 per token0.
        assert!(!PriceView::new("WBTC", 8, "WETH", 18, &stables).invert);
        assert!(!PriceView::new("USDC", 6, "USDT", 6, &stables).invert);
    }

    #[test]
    fn durations_and_numbers_are_shortened_for_reading() {
        assert_eq!(duration(None), "-");
        assert_eq!(duration(Some(1_800)), "30m");
        assert_eq!(duration(Some(3 * 3_600)), "3.0h");
        assert_eq!(duration(Some(3 * DAY)), "3.0d");
        assert_eq!(number(64_250.4), "64250");
        assert_eq!(number(1_998.04), "1998.0");
        assert_eq!(number(31.4159), "31.416");
        assert_eq!(number(0.0005), "0.000500");
    }

    #[test]
    fn the_table_shows_the_top_ranges_of_each_lookback() {
        let hour = 3_600;
        let path: Vec<PathPoint> = (0..48u64)
            .map(|i| PathPoint {
                time: i * hour,
                tick: if i % 2 == 0 { 200_295 } else { 200_325 },
                volume1: 1e18,
                liquidity: 1e18,
            })
            .collect();
        let rows = build_zone_map(
            "USDC/WETH 0.05",
            0.0005,
            10,
            &path,
            &[10, 20],
            &[1, 7],
            47 * hour,
        );
        let view = PriceView::new("USDC", 6, "WETH", 18, &["USDC".to_string()]);
        let lines = render(&rows, &[1, 7, 30], 2, &view);

        assert!(lines[0].starts_with("1-day lookback: 2 of "));
        assert!(lines[1].contains("USDC per WETH") && lines[1].contains("passes/d"));
        assert!(
            lines[2].contains("24.00") && lines[2].contains("1.200%"),
            "{}",
            lines[2]
        );
        assert_eq!(lines[4], "");
        assert!(lines[5].starts_with("7-day lookback (data covers 2.0 days): 2 of "));
        assert_eq!(lines[10], "30-day lookback: no data");
    }
}
