//! `lpbot pool-stats`: what each stored pool looked like over its history,
//! as the two pool tables. The series has the price, the active liquidity,
//! the value locked, volume and fees, hour by hour; the distribution has
//! the liquidity in fixed price buckets around the price, day by day.

use std::path::Path;

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::Config;
use lpbot_data::day::Day;
use lpbot_data::lake::{read_events, stored_pools};
use lpbot_data::pool_stats::{pool_stats, BucketPoint, SeriesPoint, StatsSettings};
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::{DayState, EventStore};
use lpbot_sim::tables::{write_pool, PoolInfo, PoolLiquidityRow, PoolSeriesRow};

use crate::chain_cmd::chain_addresses;

/// Which of a pool's tokens is the dollar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DollarSide {
    Zero,
    One,
}

/// How a pool's raw figures become whole tokens, prices and dollars.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub symbol0: String,
    pub symbol1: String,
    pub decimals0: u8,
    pub decimals1: u8,
    /// `None` for a pool without a dollar stable. When both tokens are
    /// stables, token1 counts as the dollar.
    pub dollar: Option<DollarSide>,
}

impl Quote {
    fn whole0(&self, raw: f64) -> f64 {
        raw / 10f64.powi(i32::from(self.decimals0))
    }

    fn whole1(&self, raw: f64) -> f64 {
        raw / 10f64.powi(i32::from(self.decimals1))
    }

    /// Whole token1 per whole token0 at a raw price.
    fn one_per_zero(&self, raw_price: f64) -> f64 {
        raw_price * 10f64.powi(i32::from(self.decimals0) - i32::from(self.decimals1))
    }

    /// The price the tables show: the dollar price of the other token, or
    /// token1 per token0 without a dollar.
    pub fn price(&self, raw_price: f64) -> f64 {
        let p = self.one_per_zero(raw_price);
        match self.dollar {
            Some(DollarSide::Zero) => 1.0 / p,
            Some(DollarSide::One) | None => p,
        }
    }

    pub fn price_unit(&self) -> String {
        match self.dollar {
            Some(DollarSide::Zero) => format!("{} per {}", self.symbol0, self.symbol1),
            Some(DollarSide::One) | None => format!("{} per {}", self.symbol1, self.symbol0),
        }
    }

    /// Dollars per whole token0 and per whole token1 at a raw price.
    fn dollars_per(&self, raw_price: f64) -> Option<(f64, f64)> {
        let p = self.one_per_zero(raw_price);
        match self.dollar? {
            DollarSide::One => Some((p, 1.0)),
            DollarSide::Zero => Some((1.0, 1.0 / p)),
        }
    }

    /// Whole amounts in dollars at a raw price, split as (volatile, dollar).
    fn split_usd(&self, whole0: f64, whole1: f64, raw_price: f64) -> Option<(f64, f64)> {
        let (usd0, usd1) = self.dollars_per(raw_price)?;
        Some(match self.dollar? {
            DollarSide::One => (whole0 * usd0, whole1 * usd1),
            DollarSide::Zero => (whole1 * usd1, whole0 * usd0),
        })
    }
}

/// A pool's quote from the config's tokens and dollar stables.
pub fn quote_for(config: &Config, chain: &str, token0: Address, token1: Address) -> Result<Quote> {
    let (_, tokens) = chain_addresses(config, chain)?;
    let find = |address: Address| {
        tokens
            .iter()
            .find(|(_, (a, _))| *a == address)
            .map(|(symbol, (_, decimals))| (symbol.clone(), *decimals))
            .ok_or_else(|| anyhow!("{address:#x} is not a token of the universe"))
    };
    let (symbol0, decimals0) = find(token0)?;
    let (symbol1, decimals1) = find(token1)?;
    let stables = &config.universe.usd_stables;
    let dollar = if stables.contains(&symbol1) {
        Some(DollarSide::One)
    } else if stables.contains(&symbol0) {
        Some(DollarSide::Zero)
    } else {
        None
    };
    Ok(Quote {
        symbol0,
        symbol1,
        decimals0,
        decimals1,
        dollar,
    })
}

/// The series in whole tokens and dollars.
pub fn series_rows(quote: &Quote, points: &[SeriesPoint]) -> Vec<PoolSeriesRow> {
    points
        .iter()
        .map(|p| {
            let (locked0, locked1) = (quote.whole0(p.locked0), quote.whole1(p.locked1));
            let (volume0, volume1) = (quote.whole0(p.volume0), quote.whole1(p.volume1));
            let dollars = quote.dollars_per(p.raw_price);
            PoolSeriesRow {
                time: p.time,
                block: p.block,
                tick: p.tick,
                price: quote.price(p.raw_price),
                liquidity: p.liquidity,
                locked0,
                locked1,
                tvl_usd: dollars.map(|(u0, u1)| locked0 * u0 + locked1 * u1),
                volume0,
                volume1,
                // A swap's dollar volume is what moved on its dollar side.
                volume_usd: quote.dollar.map(|side| match side {
                    DollarSide::Zero => volume0,
                    DollarSide::One => volume1,
                }),
                fees_usd: dollars
                    .map(|(u0, u1)| quote.whole0(p.fees0) * u0 + quote.whole1(p.fees1) * u1),
                swaps: p.swaps,
            }
        })
        .collect()
}

/// The distribution in whole tokens and dollars, priced at each sample's
/// own price.
pub fn liquidity_rows(quote: &Quote, buckets: &[BucketPoint]) -> Vec<PoolLiquidityRow> {
    buckets
        .iter()
        .map(|b| {
            let at = |tick: i32| quote.price(1.0001f64.powi(tick));
            let (a, c) = (at(b.tick_lower), at(b.tick_upper));
            let (low, high) = (a.min(c), a.max(c));
            let (amount0, amount1) = (quote.whole0(b.amount0), quote.whole1(b.amount1));
            let split = quote.split_usd(amount0, amount1, 1.0001f64.powi(b.tick));
            PoolLiquidityRow {
                time: b.time,
                tick_lower: b.tick_lower,
                tick_upper: b.tick_upper,
                price_low: low,
                price_high: high,
                price_mid: (low + high) / 2.0,
                liquidity: b.liquidity,
                amount0,
                amount1,
                value_usd: split.map(|(v, d)| v + d),
                volatile_usd: split.map(|(v, _)| v),
                dollar_usd: split.map(|(_, d)| d),
                at_price: b.tick_lower <= b.tick && b.tick < b.tick_upper,
            }
        })
        .collect()
}

/// Ticks a move of `pct` percent spans.
pub fn ticks_for(pct: f64) -> i32 {
    ((1.0 + pct / 100.0).ln() / 1.0001f64.ln()).round().max(1.0) as i32
}

pub struct PoolStatsArgs<'a> {
    /// Pools to sample; empty means every stored pool.
    pub pools: &'a [Address],
    pub from: Option<Day>,
    pub every: u64,
    pub liquidity_every: u64,
    pub span_pct: f64,
    pub bucket_pct: f64,
    pub root: &'a Path,
    pub results: &'a Path,
    pub chain: &'a str,
}

/// Samples every pool asked for and writes its two tables. Returns the
/// pools written.
pub fn run(config: &Config, args: &PoolStatsArgs) -> Result<usize> {
    let pools = if args.pools.is_empty() {
        stored_pools(args.root, args.chain)?
    } else {
        args.pools.to_vec()
    };
    if pools.is_empty() {
        bail!(
            "no pool history under {}; store some with `lpbot backfill` first",
            args.root.display()
        );
    }
    let snapshots = SnapshotStore::new(args.root);
    let events = EventStore::new(args.root);
    let mut written = 0;
    for pool in pools {
        let days = snapshots.days(args.chain, pool)?;
        let day = match args.from {
            Some(from) => days.iter().rfind(|d| **d <= from).or_else(|| days.first()),
            None => days.first(),
        }
        .copied()
        .ok_or_else(|| {
            anyhow!("pool {pool:#x} has no stored snapshot; run `lpbot backfill` first")
        })?;
        let snapshot = snapshots.read(args.chain, pool, day)?;
        let Some(last) = events
            .days(args.chain, pool)?
            .into_iter()
            .filter(|(d, state)| *state == DayState::Complete && *d >= day)
            .map(|(d, _)| d)
            .max()
        else {
            println!("  {pool:#x}: no complete day stored yet, left out");
            continue;
        };
        let quote = quote_for(config, args.chain, snapshot.token0, snapshot.token1)?;
        let name = format!(
            "{}/{} {}",
            quote.symbol0,
            quote.symbol1,
            f64::from(snapshot.state.fee_pips) / 10_000.0
        );
        // Stablecoin pairs sit within a fraction of a percent of each other.
        let stable_pair = config.universe.usd_stables.contains(&quote.symbol0)
            && config.universe.usd_stables.contains(&quote.symbol1);
        let (span_pct, bucket_pct) = if stable_pair {
            (args.span_pct.min(0.5), args.bucket_pct.min(0.01))
        } else {
            (args.span_pct, args.bucket_pct)
        };
        let history = read_events(args.root, args.chain, pool, snapshot.day, last)?;
        let from = args
            .from
            .unwrap_or(snapshot.day)
            .start()
            .max(snapshot.day.start());
        let stats = pool_stats(
            snapshot.state.clone(),
            &history,
            &StatsSettings {
                series_every: args.every,
                liquidity_every: args.liquidity_every,
                bucket_ticks: ticks_for(bucket_pct),
                span_ticks: ticks_for(span_pct),
                from,
            },
        )
        .with_context(|| format!("pool {name} ({pool:#x})"))?;
        let info = PoolInfo {
            pool: format!("{pool:#x}"),
            name: name.clone(),
            price_unit: quote.price_unit(),
        };
        let series = series_rows(&quote, &stats.series);
        let liquidity = liquidity_rows(&quote, &stats.liquidity);
        write_pool(args.results, &info, &series, &liquidity)?;
        println!(
            "  {name:<18} {} to {}: {} series rows, {} bucket rows",
            snapshot.day,
            last,
            series.len(),
            liquidity.len()
        );
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usdc_weth() -> Quote {
        // USDC is token0 (6 decimals), WETH token1 (18): ticks are WETH per USDC.
        Quote {
            symbol0: "USDC".into(),
            symbol1: "WETH".into(),
            decimals0: 6,
            decimals1: 18,
            dollar: Some(DollarSide::Zero),
        }
    }

    fn weth_usdt() -> Quote {
        Quote {
            symbol0: "WETH".into(),
            symbol1: "USDT".into(),
            decimals0: 18,
            decimals1: 6,
            dollar: Some(DollarSide::One),
        }
    }

    /// The raw price (token1 per token0 in smallest units) at which ETH
    /// trades at `usd`, in each pool.
    fn raw_usdc_weth(usd: f64) -> f64 {
        1e12 / usd
    }

    fn raw_weth_usdt(usd: f64) -> f64 {
        usd / 1e12
    }

    #[test]
    fn prices_are_the_dollar_price_of_eth_whichever_token_is_first() {
        assert!((usdc_weth().price(raw_usdc_weth(2_250.0)) - 2_250.0).abs() < 1e-6);
        assert!((weth_usdt().price(raw_weth_usdt(2_250.0)) - 2_250.0).abs() < 1e-6);
        assert_eq!(usdc_weth().price_unit(), "USDC per WETH");
        assert_eq!(weth_usdt().price_unit(), "USDT per WETH");
    }

    #[test]
    fn value_locked_volume_and_fees_come_out_in_dollars() {
        // 1,000 USDC and 2 WETH at $2,000: $5,000 locked.
        let point = SeriesPoint {
            time: 3_600,
            block: 1,
            tick: 0,
            raw_price: raw_usdc_weth(2_000.0),
            liquidity: 1.0,
            locked0: 1_000e6,
            locked1: 2e18,
            volume0: 500e6,
            volume1: 0.25e18,
            fees0: 0.25e6,
            fees1: 0.0,
            swaps: 3,
        };
        let row = &series_rows(&usdc_weth(), &[point])[0];
        assert!((row.tvl_usd.unwrap() - 5_000.0).abs() < 1e-6);
        assert!((row.volume_usd.unwrap() - 500.0).abs() < 1e-9);
        assert!((row.fees_usd.unwrap() - 0.25).abs() < 1e-9);
        assert!((row.price - 2_000.0).abs() < 1e-6);
        // Without a dollar, there are no dollar figures.
        let plain = Quote {
            dollar: None,
            ..usdc_weth()
        };
        let row = &series_rows(&plain, &[row_point()])[0];
        assert_eq!(
            (row.tvl_usd, row.volume_usd, row.fees_usd),
            (None, None, None)
        );
    }

    fn row_point() -> SeriesPoint {
        SeriesPoint {
            time: 0,
            block: 0,
            tick: 0,
            raw_price: 1.0,
            liquidity: 0.0,
            locked0: 0.0,
            locked1: 0.0,
            volume0: 0.0,
            volume1: 0.0,
            fees0: 0.0,
            fees1: 0.0,
            swaps: 0,
        }
    }

    #[test]
    fn a_bucket_is_priced_low_to_high_and_split_by_the_token_held() {
        let quote = usdc_weth();
        // ETH at $2,000 is tick ln(1e12 / 2000) / ln(1.0001).
        let tick = (raw_usdc_weth(2_000.0).ln() / 1.0001f64.ln()).floor() as i32;
        let bucket = BucketPoint {
            time: 0,
            tick_lower: tick - 50,
            tick_upper: tick + 50,
            tick,
            liquidity: 1.0,
            amount0: 1_000e6,
            amount1: 1e18,
        };
        let row = &liquidity_rows(&quote, &[bucket])[0];
        assert!(
            row.price_low < 2_000.0 && 2_000.0 < row.price_high,
            "{row:?}"
        );
        assert!(row.at_price);
        // USDC is the dollar side, the WETH the volatile one.
        assert!((row.dollar_usd.unwrap() - 1_000.0).abs() < 1e-6);
        assert!((row.volatile_usd.unwrap() - 2_000.0).abs() < 1.0);
        assert!((row.value_usd.unwrap() - 3_000.0).abs() < 1.0);
    }

    #[test]
    fn a_percentage_becomes_ticks() {
        assert_eq!(ticks_for(0.5), 50);
        assert_eq!(ticks_for(20.0), 1_823);
        assert_eq!(ticks_for(0.0001), 1);
    }
}
