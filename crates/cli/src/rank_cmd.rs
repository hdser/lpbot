//! `lpbot rank`: the universe's pools ranked by implied volatility.

use std::collections::BTreeMap;

use alloy_primitives::U256;
use anyhow::{anyhow, Result};
use lpbot_chain::uniswap_v3::{discover_pools, fetch_snapshot, fetch_swaps};
use lpbot_core::ranking::{rank, usd_prices, Floors, PoolStats, PriceEdge, RankedPool};
use lpbot_core::Config;

use crate::chain_cmd::Chain;

/// Ethereum produces a block every 12 seconds.
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

/// Dollar prices of the tokens and dollar-denominated stats per pool.
pub fn build_stats(
    readings: &[PoolReading],
    stables: &[String],
    window_days: f64,
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
            PoolStats {
                id: r.id.clone(),
                fee: f64::from(r.fee_pips) / 1_000_000.0,
                volume_24h: r.volume1(r.volume1_raw_24h) * usd1,
                volume_per_day_long: r.volume1(r.volume1_raw_window) * usd1 / window_days,
                l_sqrt_p: r.l_sqrt_p_token1() * usd1,
            }
        })
        .collect();
    (prices, stats)
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

/// Reads every universe pool from the node and prints the ranking.
pub fn run(chain: &Chain, config: &Config, days: u64, log_chunk: u64) -> Result<()> {
    if days == 0 {
        return Err(anyhow!("--days must be at least 1"));
    }
    let floors_config = config
        .universe
        .floors
        .ok_or_else(|| anyhow!("the config has no universe.floors"))?;
    let floors = Floors {
        volume_per_day: floors_config.volume_usd_per_day,
        depth_1pct: floors_config.in_range_liquidity_usd,
    };
    if config.universe.usd_stables.is_empty() {
        return Err(anyhow!("the config has no universe.usd_stables"));
    }

    let head = chain.client.block_number()?.saturating_sub(HEAD_MARGIN);
    let from = head.saturating_sub(days * BLOCKS_PER_DAY);
    let day_start = head.saturating_sub(BLOCKS_PER_DAY);

    let addresses: Vec<_> = chain.tokens.values().map(|(a, _)| *a).collect();
    let found = discover_pools(&chain.client, chain.factory, &addresses, head)?;
    println!(
        "reading {} pools on {} over blocks {from} to {head} ({days} days)",
        found.len(),
        chain.name
    );

    let token = |address| {
        chain
            .tokens
            .iter()
            .find(|(_, (a, _))| *a == address)
            .map(|(symbol, (_, decimals))| (symbol.clone(), *decimals))
            .ok_or_else(|| anyhow!("pool token {address:#x} is not in the universe"))
    };

    let mut readings = Vec::new();
    let mut swaps_in_first_day = 0usize;
    let mut swaps_in_last_day = 0usize;
    for pool in &found {
        let (symbol0, decimals0) = token(pool.token0)?;
        let (symbol1, decimals1) = token(pool.token1)?;
        let id = format!("{symbol0}/{symbol1} {}", pool.fee_pips as f64 / 10_000.0);
        let snapshot = fetch_snapshot(&chain.client, pool.address, head)?;
        let swaps = fetch_swaps(
            &chain.client,
            pool.address,
            from + 1,
            head,
            log_chunk,
            |_, _| {},
        )?;

        let mut window = U256::ZERO;
        let mut last_day = U256::ZERO;
        for (block, swap) in &swaps {
            let amount = swap.amount1.unsigned_abs();
            window += amount;
            if *block > day_start {
                last_day += amount;
                swaps_in_last_day += 1;
            }
            if *block <= from + BLOCKS_PER_DAY {
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
        });
    }

    let (prices, stats) = build_stats(&readings, &config.universe.usd_stables, days as f64);
    println!();
    for (symbol, price) in &prices {
        println!("{symbol}: {price:.2} USD");
    }
    println!();
    for line in render(&rank(stats, &floors), days) {
        println!("{line}");
    }
    println!();
    println!("Volume is valued at current prices. Depth 1% is the value within 1% of the price.");

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

#[cfg(test)]
mod tests {
    use super::*;
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
        }
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
        let (prices, stats) = build_stats(&readings, &["USDC".to_string()], 7.0);

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
        let (prices, stats) = build_stats(&[pool], &["USDC".to_string()], 7.0);
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
        let (_, stats) = build_stats(&readings, &["USDC".to_string()], 7.0);
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
    fn money_is_shortened_for_reading() {
        assert_eq!(money(1_234_000_000.0), "1.23B");
        assert_eq!(money(5_000_000.0), "5.0M");
        assert_eq!(money(12_345.0), "12K");
        assert_eq!(money(12.0), "12");
    }
}
