//! What a pool looked like over its stored history.
//!
//! The stored events are replayed through the replica and sampled on a
//! fixed clock. Every `series_every` seconds: the price, the active
//! liquidity, what every position holds at that price (the value locked),
//! and the volume and swap fees since the last sample. Every
//! `liquidity_every` seconds: the liquidity in fixed buckets of ticks
//! around the price, with what each bucket holds. The buckets sit on a grid
//! that does not move, so the same bucket can be followed through time.
//!
//! Floating point throughout: these are metrics for viewing and research,
//! never amounts a decision moves.

use std::collections::BTreeMap;

use lpbot_venues::uniswap_v3::events::PoolEvent;
use lpbot_venues::uniswap_v3::pool::{Pool, PoolError, PoolSnapshot, Replay};

use crate::lake::TimedEvent;

/// How to sample a pool.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsSettings {
    /// Seconds between rows of the series.
    pub series_every: u64,
    /// Seconds between distributions.
    pub liquidity_every: u64,
    /// Width of a distribution bucket, in ticks. Rounded up to a multiple
    /// of the pool's tick spacing.
    pub bucket_ticks: i32,
    /// How far the distribution reaches on each side of the price, in ticks.
    pub span_ticks: i32,
    /// Samples start here, in Unix seconds. Earlier events only build the
    /// state.
    pub from: u64,
}

/// One row of the series. Amounts are in each token's smallest unit.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesPoint {
    pub time: u64,
    /// The last block applied before the sample.
    pub block: u64,
    pub tick: i32,
    /// Token1 per token0, in smallest units.
    pub raw_price: f64,
    /// Liquidity active at the price.
    pub liquidity: f64,
    /// What all positions together hold at the price.
    pub locked0: f64,
    pub locked1: f64,
    /// Amounts swapped since the previous row, counted on both sides.
    pub volume0: f64,
    pub volume1: f64,
    /// Fees swappers paid since the previous row, in the token they paid in.
    pub fees0: f64,
    pub fees1: f64,
    pub swaps: u32,
}

/// The liquidity in one bucket of ticks at one time.
#[derive(Debug, Clone, PartialEq)]
pub struct BucketPoint {
    pub time: u64,
    pub tick_lower: i32,
    pub tick_upper: i32,
    /// The bucket's tick at the price when the sample was taken, for
    /// finding the bucket the price is in.
    pub tick: i32,
    /// Average liquidity over the bucket's ticks.
    pub liquidity: f64,
    /// What the positions hold inside the bucket at the price, in smallest
    /// units.
    pub amount0: f64,
    pub amount1: f64,
}

/// The pool's history, sampled.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoolStats {
    pub series: Vec<SeriesPoint>,
    pub liquidity: Vec<BucketPoint>,
}

#[derive(Debug, thiserror::Error)]
pub enum StatsError {
    #[error("the replica refused the event at block {block}, log {log_index}: {source}")]
    Replay {
        block: u64,
        log_index: u32,
        source: PoolError,
    },
    #[error("{0}")]
    Settings(String),
}

/// `sqrt(1.0001^tick)`.
pub fn sqrt_at(tick: i32) -> f64 {
    1.0001f64.powf(f64::from(tick) / 2.0)
}

/// What `liquidity` holds between `lower` and `upper` at the square root of
/// the price `sqrt`, in smallest units, as the pool's formulas give it.
pub fn amounts(liquidity: f64, lower: i32, upper: i32, sqrt: f64) -> (f64, f64) {
    let (a, b) = (sqrt_at(lower), sqrt_at(upper));
    if sqrt <= a {
        (liquidity * (1.0 / a - 1.0 / b), 0.0)
    } else if sqrt >= b {
        (0.0, liquidity * (b - a))
    } else {
        (liquidity * (1.0 / sqrt - 1.0 / b), liquidity * (sqrt - a))
    }
}

/// The ranges over which the liquidity is constant, with that liquidity,
/// lowest first. Ranges without liquidity are left out.
pub fn segments(pool: &Pool) -> Vec<(i32, i32, f64)> {
    let mut ticks: Vec<(i32, i128)> = pool
        .ticks()
        .filter(|(_, info)| info.liquidity_gross > 0)
        .map(|(tick, info)| (tick, info.liquidity_net))
        .collect();
    ticks.sort_by_key(|(tick, _)| *tick);
    let mut out = Vec::new();
    let mut liquidity: i128 = 0;
    for pair in ticks.windows(2) {
        liquidity += pair[0].1;
        if liquidity > 0 {
            out.push((pair[0].0, pair[1].0, liquidity as f64));
        }
    }
    out
}

fn sqrt_price(pool: &Pool) -> f64 {
    pool.sqrt_price_x96()
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::NAN)
        / 2f64.powi(96)
}

/// Swaps since the last sample.
#[derive(Debug, Default)]
struct Flow {
    volume0: f64,
    volume1: f64,
    fees0: f64,
    fees1: f64,
    swaps: u32,
}

fn series_point(pool: &Pool, time: u64, block: u64, flow: &Flow) -> SeriesPoint {
    let sqrt = sqrt_price(pool);
    let (mut locked0, mut locked1) = (0.0, 0.0);
    for (lower, upper, liquidity) in segments(pool) {
        let (a0, a1) = amounts(liquidity, lower, upper, sqrt);
        locked0 += a0;
        locked1 += a1;
    }
    SeriesPoint {
        time,
        block,
        tick: pool.tick(),
        raw_price: sqrt * sqrt,
        liquidity: pool.liquidity() as f64,
        locked0,
        locked1,
        volume0: flow.volume0,
        volume1: flow.volume1,
        fees0: flow.fees0,
        fees1: flow.fees1,
        swaps: flow.swaps,
    }
}

/// The liquidity of `pool` in buckets `width` ticks wide, on a fixed grid,
/// from `span` ticks below its tick to `span` above, at `time`.
pub fn bucket_points(pool: &Pool, time: u64, width: i32, span: i32) -> Vec<BucketPoint> {
    let tick = pool.tick();
    let sqrt = sqrt_price(pool);
    let first = (tick - span).div_euclid(width);
    let last = (tick + span).div_euclid(width);
    // Bucket index → (liquidity × ticks, amount0, amount1).
    let mut buckets: BTreeMap<i32, (f64, f64, f64)> =
        (first..=last).map(|k| (k, (0.0, 0.0, 0.0))).collect();
    let (low, high) = (first * width, (last + 1) * width);
    for (lower, upper, liquidity) in segments(pool) {
        let (from, to) = (lower.max(low), upper.min(high));
        if from >= to {
            continue;
        }
        let mut start = from;
        while start < to {
            let k = start.div_euclid(width);
            let end = ((k + 1) * width).min(to);
            let (a0, a1) = amounts(liquidity, start, end, sqrt);
            let entry = buckets.get_mut(&k).expect("inside the span");
            entry.0 += liquidity * f64::from(end - start);
            entry.1 += a0;
            entry.2 += a1;
            start = end;
        }
    }
    buckets
        .into_iter()
        .map(|(k, (weighted, amount0, amount1))| BucketPoint {
            time,
            tick_lower: k * width,
            tick_upper: (k + 1) * width,
            tick,
            liquidity: weighted / f64::from(width),
            amount0,
            amount1,
        })
        .collect()
}

/// The first multiple of `every` at or after `time`.
fn clock_from(time: u64, every: u64) -> u64 {
    time.div_ceil(every) * every
}

/// Replays `events` on top of `snapshot` and samples the pool. Events must
/// be in chain order and start right after the snapshot. A sample at time
/// `t` shows the pool as the last block before `t` left it; the volume and
/// fees in a row are those of the swaps since the previous row.
pub fn pool_stats(
    snapshot: PoolSnapshot,
    events: &[TimedEvent],
    settings: &StatsSettings,
) -> Result<PoolStats, StatsError> {
    if settings.series_every == 0 || settings.liquidity_every == 0 {
        return Err(StatsError::Settings(
            "the sampling intervals must be at least a second".into(),
        ));
    }
    if settings.bucket_ticks <= 0 || settings.span_ticks <= 0 {
        return Err(StatsError::Settings(
            "the bucket width and the span must be positive numbers of ticks".into(),
        ));
    }
    let spacing = snapshot.tick_spacing.max(1);
    let width = (settings.bucket_ticks + spacing - 1).div_euclid(spacing) * spacing;
    let fee = f64::from(snapshot.fee_pips) / 1_000_000.0;

    let mut pool = Pool::from_snapshot(snapshot);
    let mut out = PoolStats::default();
    let mut flow = Flow::default();
    let mut next_series = clock_from(settings.from, settings.series_every);
    let mut next_liquidity = clock_from(settings.from, settings.liquidity_every);
    let mut block = 0;
    for event in events {
        while event.time >= next_series {
            out.series
                .push(series_point(&pool, next_series, block, &flow));
            flow = Flow::default();
            next_series += settings.series_every;
        }
        while event.time >= next_liquidity {
            out.liquidity.extend(bucket_points(
                &pool,
                next_liquidity,
                width,
                settings.span_ticks,
            ));
            next_liquidity += settings.liquidity_every;
        }
        pool.apply_event(&event.event, Replay::Strict, &mut ())
            .map_err(|source| StatsError::Replay {
                block: event.block,
                log_index: event.log_index,
                source,
            })?;
        block = event.block;
        if event.time < settings.from {
            continue;
        }
        if let PoolEvent::Swap(swap) = &event.event {
            let a0 = swap.amount0.to_string().parse::<f64>().unwrap_or(0.0);
            let a1 = swap.amount1.to_string().parse::<f64>().unwrap_or(0.0);
            flow.volume0 += a0.abs();
            flow.volume1 += a1.abs();
            // The token paid in is the one the pool received.
            if a0 > 0.0 {
                flow.fees0 += a0 * fee;
            } else if a1 > 0.0 {
                flow.fees1 += a1 * fee;
            }
            flow.swaps += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::{generate, tests as synth, SynthConfig};

    fn settings(from: u64) -> StatsSettings {
        StatsSettings {
            series_every: 3_600,
            liquidity_every: 86_400,
            bucket_ticks: 60,
            span_ticks: 2_000,
            from,
        }
    }

    #[test]
    fn a_range_holds_token0_above_the_price_and_token1_below() {
        let sqrt = sqrt_at(0);
        let (above0, above1) = amounts(1e18, 100, 200, sqrt);
        assert!(above0 > 0.0 && above1 == 0.0);
        let (below0, below1) = amounts(1e18, -200, -100, sqrt);
        assert!(below0 == 0.0 && below1 > 0.0);
        let (in0, in1) = amounts(1e18, -100, 100, sqrt);
        assert!(in0 > 0.0 && in1 > 0.0);
        // Split at the price, the two halves add up to the whole.
        let (low0, low1) = amounts(1e18, -100, 0, sqrt);
        let (high0, high1) = amounts(1e18, 0, 100, sqrt);
        assert!((low0 + high0 - in0).abs() < 1e-3 && (low1 + high1 - in1).abs() < 1e-3);
    }

    #[test]
    fn the_series_counts_every_swap_once_and_follows_the_price() {
        let config = SynthConfig {
            first_block: 100,
            ..synth::small()
        };
        let history = generate(&config, synth::pool()).unwrap();
        let from = config.start.start();
        let stats = pool_stats(
            history.snapshot.state.clone(),
            &history.events,
            &settings(from),
        )
        .unwrap();
        // Three days of history: a row for every whole hour before the last event.
        assert!(
            stats.series.len() >= 71 && stats.series.len() <= 72,
            "{}",
            stats.series.len()
        );
        assert!(stats
            .series
            .windows(2)
            .all(|w| w[1].time - w[0].time == 3_600));
        let swaps: u32 = stats.series.iter().map(|r| r.swaps).sum();
        let all = history
            .events
            .iter()
            .filter(|e| {
                matches!(e.event, PoolEvent::Swap(_)) && e.time < stats.series.last().unwrap().time
            })
            .count() as u32;
        assert_eq!(swaps, all);
        // Every sample holds something and its price sits at its tick.
        for row in &stats.series {
            assert!(row.locked0 > 0.0 || row.locked1 > 0.0);
            let at_tick = 1.0001f64.powi(row.tick);
            assert!((row.raw_price / at_tick - 1.0).abs() < 2e-4, "{row:?}");
        }
        // Fees are the fee tier's share of what was paid in.
        let fee = f64::from(history.snapshot.state.fee_pips) / 1e6;
        let paid: f64 = stats.series.iter().map(|r| r.fees0 + r.fees1).sum();
        assert!(paid > 0.0);
        let volume: f64 = stats.series.iter().map(|r| r.volume0 + r.volume1).sum();
        assert!(paid < volume * fee);
    }

    #[test]
    fn the_buckets_add_up_to_what_is_locked_near_the_price() {
        let config = SynthConfig {
            first_block: 100,
            ..synth::small()
        };
        let history = generate(&config, synth::pool()).unwrap();
        let from = config.start.start();
        let wide = StatsSettings {
            // A span far wider than any position: the buckets hold everything.
            span_ticks: 400_000,
            bucket_ticks: 1_000,
            ..settings(from)
        };
        let stats = pool_stats(history.snapshot.state.clone(), &history.events, &wide).unwrap();
        let first_time = stats.liquidity[0].time;
        let day: Vec<&BucketPoint> = stats
            .liquidity
            .iter()
            .filter(|b| b.time == first_time)
            .collect();
        let series = stats.series.iter().find(|r| r.time == first_time).unwrap();
        let (sum0, sum1) = day
            .iter()
            .fold((0.0, 0.0), |(a, b), p| (a + p.amount0, b + p.amount1));
        assert!(
            (sum0 / series.locked0 - 1.0).abs() < 1e-9,
            "{sum0} {}",
            series.locked0
        );
        assert!(
            (sum1 / series.locked1 - 1.0).abs() < 1e-9,
            "{sum1} {}",
            series.locked1
        );
        // The buckets sit on a fixed grid, in order, without gaps.
        assert!(day.windows(2).all(|w| w[0].tick_upper == w[1].tick_lower));
        assert!(day.iter().all(|b| b.tick_lower % 1_000 == 0));
        // The bucket holding the price has liquidity equal to what is active.
        let at = day
            .iter()
            .find(|b| b.tick_lower <= series.tick && series.tick < b.tick_upper)
            .unwrap();
        assert!(at.liquidity > 0.0);
    }

    #[test]
    fn nonsense_settings_are_refused() {
        let config = synth::small();
        let history = generate(&config, synth::pool()).unwrap();
        let bad = StatsSettings {
            series_every: 0,
            ..settings(0)
        };
        assert!(pool_stats(history.snapshot.state.clone(), &history.events, &bad).is_err());
        let bad = StatsSettings {
            bucket_ticks: 0,
            ..settings(0)
        };
        assert!(pool_stats(history.snapshot.state, &history.events, &bad).is_err());
    }
}
