//! `lpbot pool-stats`: what each stored pool looked like over its history,
//! as the two pool tables. The series has the price, the active liquidity,
//! the value locked, volume and fees, hour by hour; the distribution has
//! the liquidity in fixed price buckets around the price, day by day.

use std::collections::HashMap;
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

/// Dollars per whole token for one of a pool's tokens, hour by hour, taken
/// from a pool that pairs that token with a dollar stable. It prices a pool
/// that has no dollar stable of its own, such as WBTC/WETH.
#[derive(Debug, Clone, PartialEq)]
pub struct Reference {
    /// Which of this pool's tokens the prices are for.
    pub side: DollarSide,
    /// (time, dollars per whole token), by time.
    pub usd: Vec<(u64, f64)>,
    /// The pool the prices come from, for the output.
    pub from: String,
}

impl Reference {
    /// The latest price at or before `time`; before the first, the first.
    pub fn at(&self, time: u64) -> Option<f64> {
        let after = self.usd.partition_point(|(t, _)| *t <= time);
        self.usd.get(after.saturating_sub(1)).map(|(_, usd)| *usd)
    }
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
    /// For a pool without a dollar stable: one token's dollar price from
    /// another pool. `None` leaves such a pool without dollar figures.
    pub reference: Option<Reference>,
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

    /// Dollars per whole token0 and per whole token1 at a raw price and a
    /// time: from the pool's dollar stable, or from the reference price of
    /// one of its tokens.
    fn dollars_per(&self, raw_price: f64, time: u64) -> Option<(f64, f64)> {
        let p = self.one_per_zero(raw_price);
        match self.dollar {
            Some(DollarSide::One) => Some((p, 1.0)),
            Some(DollarSide::Zero) => Some((1.0, 1.0 / p)),
            None => {
                let reference = self.reference.as_ref()?;
                let usd = reference.at(time)?;
                Some(match reference.side {
                    DollarSide::Zero => (usd, usd / p),
                    DollarSide::One => (usd * p, usd),
                })
            }
        }
    }

    /// The token the price is quoted in: the dollar stable, or token1.
    fn quote_side(&self) -> DollarSide {
        self.dollar.unwrap_or(DollarSide::One)
    }

    /// Whole amounts in dollars at a raw price and time, split as (base
    /// token, quote token): the token priced and the one it is priced in.
    fn split_usd(&self, whole0: f64, whole1: f64, raw_price: f64, time: u64) -> Option<(f64, f64)> {
        let (usd0, usd1) = self.dollars_per(raw_price, time)?;
        Some(match self.quote_side() {
            DollarSide::One => (whole0 * usd0, whole1 * usd1),
            DollarSide::Zero => (whole1 * usd1, whole0 * usd0),
        })
    }

    /// The token whose dollar price this pool gives, when it pairs a
    /// volatile token with a dollar stable.
    fn volatile_symbol(&self) -> Option<&str> {
        match self.dollar? {
            DollarSide::Zero => Some(&self.symbol1),
            DollarSide::One => Some(&self.symbol0),
        }
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
        reference: None,
    })
}

/// A volatile token's dollar prices from the preferred pool sampled so far.
struct PriceSource {
    preference: (usize, usize),
    name: String,
    usd: Vec<(u64, f64)>,
}

/// How much a pool is preferred as the source of a token's dollar price:
/// the busiest fee tiers first, then the config's order of stables. Lower
/// is better.
fn preference(quote: &Quote, fee_pips: u32, stables: &[String]) -> (usize, usize) {
    let fee = [500, 3_000, 100, 10_000]
        .iter()
        .position(|f| *f == fee_pips)
        .unwrap_or(4);
    let stable = match quote.dollar {
        Some(DollarSide::Zero) => &quote.symbol0,
        Some(DollarSide::One) => &quote.symbol1,
        None => return (usize::MAX, usize::MAX),
    };
    let order = stables
        .iter()
        .position(|s| s == stable)
        .unwrap_or(stables.len());
    (fee, order)
}

/// The pool to take a dollar price from for a pool without a dollar
/// stable: of the pools pairing its quote token (else its base token) with
/// a stable, the preferred one. `None` when no such pool is stored.
pub fn reference_pool<'a>(
    quote: &Quote,
    candidates: &'a [(Address, Quote, u32)],
    stables: &[String],
) -> Option<&'a (Address, Quote, u32)> {
    [&quote.symbol1, &quote.symbol0]
        .into_iter()
        .find_map(|symbol| {
            candidates
                .iter()
                .filter(|(_, q, _)| {
                    q.volatile_symbol() == Some(symbol.as_str())
                        && !(stables.contains(&q.symbol0) && stables.contains(&q.symbol1))
                })
                .min_by_key(|(_, q, fee)| preference(q, *fee, stables))
        })
}

/// The series in whole tokens and dollars.
pub fn series_rows(quote: &Quote, points: &[SeriesPoint]) -> Vec<PoolSeriesRow> {
    points
        .iter()
        .map(|p| {
            let (locked0, locked1) = (quote.whole0(p.locked0), quote.whole1(p.locked1));
            let (volume0, volume1) = (quote.whole0(p.volume0), quote.whole1(p.volume1));
            let dollars = quote.dollars_per(p.raw_price, p.time);
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
                // A swap's dollar volume is what moved on its quote side:
                // the dollar stable, or token1 at its reference price.
                volume_usd: dollars.map(|(u0, u1)| match quote.quote_side() {
                    DollarSide::Zero => volume0 * u0,
                    DollarSide::One => volume1 * u1,
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
            let split = quote.split_usd(amount0, amount1, 1.0001f64.powi(b.tick), b.time);
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
///
/// A pool without a dollar stable, such as WBTC/WETH, is priced in dollars
/// through the preferred pool that pairs one of its tokens with a stable
/// (USDC/WETH 0.05% for WETH). That pool is sampled first, and added to
/// the pools sampled when it was not asked for.
pub fn run(config: &Config, args: &PoolStatsArgs) -> Result<usize> {
    let stored = stored_pools(args.root, args.chain)?;
    let asked = if args.pools.is_empty() {
        stored.clone()
    } else {
        args.pools.to_vec()
    };
    if asked.is_empty() {
        bail!(
            "no pool history under {}; store some with `lpbot backfill` first",
            args.root.display()
        );
    }
    let snapshots = SnapshotStore::new(args.root);
    let events = EventStore::new(args.root);
    let stables = &config.universe.usd_stables;

    // A pool's quote and fee tier, from its first snapshot.
    let describe = |pool: Address| -> Result<(Address, Quote, u32)> {
        let day = snapshots
            .days(args.chain, pool)?
            .first()
            .copied()
            .ok_or_else(|| {
                anyhow!("pool {pool:#x} has no stored snapshot; run `lpbot backfill` first")
            })?;
        let snapshot = snapshots.read(args.chain, pool, day)?;
        let quote = quote_for(config, args.chain, snapshot.token0, snapshot.token1)?;
        Ok((pool, quote, snapshot.state.fee_pips))
    };
    let mut plan = asked
        .iter()
        .map(|pool| describe(*pool))
        .collect::<Result<Vec<_>>>()?;
    if plan.iter().any(|(_, quote, _)| quote.dollar.is_none()) {
        let candidates = stored
            .iter()
            .map(|pool| describe(*pool))
            .collect::<Result<Vec<_>>>()?;
        let mut sources: Vec<(Address, Quote, u32)> = Vec::new();
        for (_, quote, _) in plan.iter().filter(|(_, q, _)| q.dollar.is_none()) {
            if let Some(source) = reference_pool(quote, &candidates, stables) {
                let known =
                    |list: &[(Address, Quote, u32)]| list.iter().any(|(p, _, _)| *p == source.0);
                if !known(&plan) && !known(&sources) {
                    sources.push(source.clone());
                }
            }
        }
        plan.extend(sources);
    }
    // Pools with a dollar stable first: they give the others their prices.
    plan.sort_by_key(|(_, quote, _)| quote.dollar.is_none());

    // The dollar price of each volatile token.
    let mut prices: HashMap<String, PriceSource> = HashMap::new();
    let mut written = 0;
    for (pool, mut quote, fee_pips) in plan {
        if quote.dollar.is_none() {
            let reference = [
                (quote.symbol1.clone(), DollarSide::One),
                (quote.symbol0.clone(), DollarSide::Zero),
            ]
            .into_iter()
            .find_map(|(symbol, side)| {
                prices.get(&symbol).map(|source| Reference {
                    side,
                    usd: source.usd.clone(),
                    from: source.name.clone(),
                })
            });
            quote.reference = reference;
        }
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
        let name = format!(
            "{}/{} {}",
            quote.symbol0,
            quote.symbol1,
            f64::from(fee_pips) / 10_000.0
        );
        // Stablecoin pairs sit within a fraction of a percent of each other.
        let stable_pair = stables.contains(&quote.symbol0) && stables.contains(&quote.symbol1);
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
        let priced = match (&quote.dollar, &quote.reference) {
            (Some(_), _) => String::new(),
            (None, Some(reference)) => format!(", dollars through {}", reference.from),
            (None, None) => ", no dollar figures: no stored pool prices either token".to_string(),
        };
        println!(
            "  {name:<18} {} to {}: {} series rows, {} bucket rows{priced}",
            snapshot.day,
            last,
            series.len(),
            liquidity.len()
        );
        if let (Some(symbol), false) = (quote.volatile_symbol(), stable_pair) {
            let rank = preference(&quote, fee_pips, stables);
            if prices.get(symbol).is_none_or(|best| rank < best.preference) {
                let usd = series.iter().map(|row| (row.time, row.price)).collect();
                prices.insert(
                    symbol.to_string(),
                    PriceSource {
                        preference: rank,
                        name: name.clone(),
                        usd,
                    },
                );
            }
        }
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
            reference: None,
        }
    }

    fn weth_usdt() -> Quote {
        Quote {
            symbol0: "WETH".into(),
            symbol1: "USDT".into(),
            decimals0: 18,
            decimals1: 6,
            dollar: Some(DollarSide::One),
            reference: None,
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
        // USDC is the dollar side and the quote token, WETH the base one.
        assert!((row.dollar_usd.unwrap() - 1_000.0).abs() < 1e-6);
        assert!((row.volatile_usd.unwrap() - 2_000.0).abs() < 1.0);
        assert!((row.value_usd.unwrap() - 3_000.0).abs() < 1.0);
    }

    /// WBTC is token0 (8 decimals), WETH token1 (18): no dollar stable,
    /// WETH priced at `weth_usd` throughout from another pool.
    fn wbtc_weth(weth_usd: f64) -> Quote {
        Quote {
            symbol0: "WBTC".into(),
            symbol1: "WETH".into(),
            decimals0: 8,
            decimals1: 18,
            dollar: None,
            reference: Some(Reference {
                side: DollarSide::One,
                usd: vec![(0, weth_usd)],
                from: "USDC/WETH 0.05".into(),
            }),
        }
    }

    #[test]
    fn a_reference_price_is_the_latest_at_or_before_the_time() {
        let reference = Reference {
            side: DollarSide::One,
            usd: vec![(3_600, 2_000.0), (7_200, 2_100.0)],
            from: String::new(),
        };
        assert_eq!(reference.at(0), Some(2_000.0));
        assert_eq!(reference.at(3_600), Some(2_000.0));
        assert_eq!(reference.at(7_199), Some(2_000.0));
        assert_eq!(reference.at(7_200), Some(2_100.0));
        assert_eq!(reference.at(99_999), Some(2_100.0));
        let empty = Reference {
            usd: Vec::new(),
            ..reference
        };
        assert_eq!(empty.at(0), None);
    }

    #[test]
    fn a_pool_without_a_dollar_stable_is_priced_through_its_reference() {
        // 1 WBTC is 30 WETH and WETH is $2,000: 1 WBTC and 10 WETH locked
        // are $80,000; 3 WETH of volume is $6,000.
        let raw = 30.0 * 1e10;
        let point = SeriesPoint {
            raw_price: raw,
            locked0: 1e8,
            locked1: 10e18,
            volume0: 0.1e8,
            volume1: 3e18,
            fees0: 0.0,
            fees1: 0.009e18,
            ..row_point()
        };
        let quote = wbtc_weth(2_000.0);
        let row = &series_rows(&quote, &[point])[0];
        assert!((row.price - 30.0).abs() < 1e-9, "WETH per WBTC");
        assert_eq!(quote.price_unit(), "WETH per WBTC");
        assert!((row.tvl_usd.unwrap() - 80_000.0).abs() < 1e-6);
        assert!((row.volume_usd.unwrap() - 6_000.0).abs() < 1e-6);
        assert!((row.fees_usd.unwrap() - 18.0).abs() < 1e-9);

        // The liquidity: WBTC is the base token, WETH the quote token.
        let tick = (raw.ln() / 1.0001f64.ln()).floor() as i32;
        let bucket = BucketPoint {
            time: 0,
            tick_lower: tick - 50,
            tick_upper: tick + 50,
            tick,
            liquidity: 1.0,
            amount0: 0.5e8,
            amount1: 5e18,
        };
        let row = &liquidity_rows(&quote, &[bucket])[0];
        assert!(
            (row.volatile_usd.unwrap() - 30_000.0).abs() < 10.0,
            "{row:?}"
        );
        assert!((row.dollar_usd.unwrap() - 10_000.0).abs() < 1e-6);

        // Without a reference there are still no dollar figures.
        let plain = Quote {
            reference: None,
            ..quote
        };
        assert_eq!(series_rows(&plain, &[row_point()])[0].tvl_usd, None);
    }

    #[test]
    fn the_reference_comes_from_the_busiest_fee_tier_and_the_first_stable() {
        let stables = vec!["USDC".to_string(), "USDT".to_string()];
        let pool = |byte: u8| Address::repeat_byte(byte);
        let usdt_weth = Quote {
            symbol0: "WETH".into(),
            symbol1: "USDT".into(),
            ..weth_usdt()
        };
        let usdc_usdt = Quote {
            symbol0: "USDC".into(),
            symbol1: "USDT".into(),
            decimals0: 6,
            decimals1: 6,
            dollar: Some(DollarSide::One),
            reference: None,
        };
        let wbtc_usdc = Quote {
            symbol0: "WBTC".into(),
            symbol1: "USDC".into(),
            decimals0: 8,
            decimals1: 6,
            dollar: Some(DollarSide::One),
            reference: None,
        };
        let candidates = vec![
            (pool(1), usdc_weth(), 3_000),
            (pool(2), usdt_weth.clone(), 500),
            (pool(3), usdc_weth(), 500),
            (pool(4), usdc_usdt, 100),
            (pool(5), wbtc_usdc, 500),
        ];
        // WETH, the quote token, first: 0.05% beats 0.3%, USDC beats USDT.
        let chosen = reference_pool(&wbtc_weth(0.0), &candidates, &stables).unwrap();
        assert_eq!(chosen.0, pool(3));
        // Without a WETH pool, WBTC's own.
        let no_weth: Vec<_> = candidates
            .iter()
            .filter(|c| c.1.symbol1 != "WETH" && c.1.symbol0 != "WETH")
            .cloned()
            .collect();
        assert_eq!(
            reference_pool(&wbtc_weth(0.0), &no_weth, &stables)
                .unwrap()
                .0,
            pool(5)
        );
        assert!(reference_pool(&wbtc_weth(0.0), &[], &stables).is_none());
    }

    #[test]
    fn a_percentage_becomes_ticks() {
        assert_eq!(ticks_for(0.5), 50);
        assert_eq!(ticks_for(20.0), 1_823);
        assert_eq!(ticks_for(0.0001), 1);
    }
}
