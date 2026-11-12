//! The zone map: how often price crosses each candidate range.
//!
//! Everything here is statistics on a pool's price path. It decides where
//! ranges go, never token amounts, so it works in floating point.
//!
//! A range is `[lower, upper)` in ticks, as in the pool: a position is in
//! range while `lower <= tick < upper`. All of it is in tick terms; which
//! token is "up" is the caller's business.

use lpbot_core::metrics::implied_vol_annual;
use lpbot_core::Tick;

/// Seconds in a day.
pub const DAY: u64 = 86_400;

/// The pool right after one swap. The price stays at `tick` until the next
/// point.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PathPoint {
    /// Unix time of the swap's block, in seconds.
    pub time: u64,
    /// Pool tick after the swap.
    pub tick: Tick,
    /// Token1 the swap traded, in the token's smallest unit.
    pub volume1: f64,
    /// Liquidity in range after the swap.
    pub liquidity: f64,
}

/// Which side of a range the price is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `tick < lower`.
    Below,
    /// `tick >= upper`.
    Above,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Inside,
    Outside(Side),
}

fn position(tick: Tick, lower: Tick, upper: Tick) -> Position {
    if tick < lower {
        Position::Outside(Side::Below)
    } else if tick >= upper {
        Position::Outside(Side::Above)
    } else {
        Position::Inside
    }
}

/// One stay outside a range, from the moment price left it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Excursion {
    pub side: Side,
    /// Seconds outside. When `returned` is false the window ended first,
    /// and this is only how long the stay has lasted so far.
    pub seconds: u64,
    pub returned: bool,
}

/// What one range saw over one window of the price path.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneStats {
    pub lower: Tick,
    pub upper: Tick,
    /// Seconds of the window the path covers. Shorter than the window when
    /// the data starts later.
    pub observed_seconds: u64,
    pub seconds_inside: u64,
    /// Times price came into the range.
    pub visits: u32,
    /// Full passes from below the range to above it.
    pub passes_up: u32,
    /// Full passes from above the range to below it.
    pub passes_down: u32,
    /// Seconds between each full pass and the next one, which by
    /// construction runs the other way. This is how long a position that
    /// price ran through had to wait to be carried back.
    pub pass_intervals: Vec<u64>,
    /// Seconds from the last full pass to the end of the window.
    pub since_last_pass: Option<u64>,
    /// Every stay outside the range that began inside the window.
    pub excursions: Vec<Excursion>,
    /// Token1 volume traded inside the range, smallest unit.
    pub volume1_inside: f64,
    /// In-range liquidity summed over the seconds spent inside.
    pub liquidity_seconds: f64,
}

fn quantile(values: &mut [u64], q: f64) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize;
    Some(values[index])
}

impl ZoneStats {
    pub fn passes(&self) -> u32 {
        self.passes_up + self.passes_down
    }

    pub fn observed_days(&self) -> f64 {
        self.observed_seconds as f64 / DAY as f64
    }

    /// Full passes per day, both directions.
    pub fn passes_per_day(&self) -> f64 {
        if self.observed_seconds == 0 {
            return 0.0;
        }
        f64::from(self.passes()) / self.observed_days()
    }

    /// Share of the observed time price spent inside the range.
    pub fn time_in_range(&self) -> f64 {
        if self.observed_seconds == 0 {
            return 0.0;
        }
        self.seconds_inside as f64 / self.observed_seconds as f64
    }

    /// Fee income per day as a fraction of the position: a full pass trades
    /// the whole position once, so it pays about the fee tier.
    pub fn pass_yield_per_day(&self, fee: f64) -> f64 {
        self.passes_per_day() * fee
    }

    /// Average in-range liquidity while price was inside.
    pub fn avg_liquidity(&self) -> Option<f64> {
        (self.seconds_inside > 0).then(|| self.liquidity_seconds / self.seconds_inside as f64)
    }

    /// Token1 volume per day spent inside the range.
    pub fn volume1_per_day_inside(&self) -> Option<f64> {
        (self.seconds_inside > 0)
            .then(|| self.volume1_inside / (self.seconds_inside as f64 / DAY as f64))
    }

    /// Implied volatility of the zone: the pool formula, with the volume
    /// traded while price was inside and the liquidity that was in range
    /// then. Noisy when price spent little time inside.
    pub fn implied_vol(&self, fee: f64) -> Option<f64> {
        let center = f64::from(self.lower + self.upper) / 2.0;
        let sqrt_price = 1.0001f64.powf(center / 2.0);
        implied_vol_annual(
            fee,
            self.volume1_per_day_inside()?,
            self.avg_liquidity()? * sqrt_price,
        )
    }

    /// Chance that price, having left the range, is back inside within
    /// `horizon` seconds. Stays the window cut off before the horizon are
    /// left out, because they could still go either way. `side` limits the
    /// count to exits on one side.
    pub fn return_odds(&self, horizon: u64, side: Option<Side>) -> Option<f64> {
        let mut counted = 0u32;
        let mut back = 0u32;
        for e in &self.excursions {
            if side.is_some_and(|s| s != e.side) {
                continue;
            }
            if e.returned {
                counted += 1;
                if e.seconds <= horizon {
                    back += 1;
                }
            } else if e.seconds >= horizon {
                counted += 1;
            }
        }
        (counted > 0).then(|| f64::from(back) / f64::from(counted))
    }

    /// Quantile of the time outside, over the stays that ended.
    pub fn return_time_quantile(&self, q: f64) -> Option<u64> {
        let mut times: Vec<u64> = self
            .excursions
            .iter()
            .filter(|e| e.returned)
            .map(|e| e.seconds)
            .collect();
        quantile(&mut times, q)
    }

    /// Quantile of the wait between a full pass and the pass back.
    pub fn pass_interval_quantile(&self, q: f64) -> Option<u64> {
        quantile(&mut self.pass_intervals.clone(), q)
    }
}

/// Measures one range over the window `[from, to]` of a path sorted by time.
///
/// The last point before `from` sets where price stood when the window
/// opened. Without one, the window opens at the first point.
pub fn zone_stats(path: &[PathPoint], lower: Tick, upper: Tick, from: u64, to: u64) -> ZoneStats {
    let mut stats = ZoneStats {
        lower,
        upper,
        observed_seconds: 0,
        seconds_inside: 0,
        visits: 0,
        passes_up: 0,
        passes_down: 0,
        pass_intervals: Vec::new(),
        since_last_pass: None,
        excursions: Vec::new(),
        volume1_inside: 0.0,
        liquidity_seconds: 0.0,
    };
    if to <= from || upper <= lower {
        return stats;
    }

    let first_inside = path.partition_point(|p| p.time <= from);
    let (mut current, mut now, rest) = if first_inside > 0 {
        (path[first_inside - 1], from, &path[first_inside..])
    } else {
        match path.first() {
            Some(first) if first.time <= to => (*first, first.time, &path[1..]),
            _ => return stats,
        }
    };
    let opened_at = now;

    let mut spot = position(current.tick, lower, upper);
    // The side price was last seen outside on. A pass is a change of it.
    let mut last_outside = match spot {
        Position::Outside(side) => Some(side),
        Position::Inside => None,
    };
    let mut last_pass_at: Option<u64> = None;
    // A stay outside that began inside the window, with its start.
    let mut outside_since: Option<(Side, u64)> = None;

    let spend = |stats: &mut ZoneStats, spot: Position, point: &PathPoint, seconds: u64| {
        if spot == Position::Inside {
            stats.seconds_inside += seconds;
            stats.liquidity_seconds += point.liquidity * seconds as f64;
        }
    };

    for next in rest {
        if next.time > to {
            break;
        }
        spend(&mut stats, spot, &current, next.time - now);
        now = next.time;

        // Volume of the move, shared by how much of it ran through the range.
        let (low, high) = (current.tick.min(next.tick), current.tick.max(next.tick));
        let share = if low == high {
            f64::from(u8::from(
                position(next.tick, lower, upper) == Position::Inside,
            ))
        } else {
            let overlap = (high.min(upper) - low.max(lower)).max(0);
            f64::from(overlap) / f64::from(high - low)
        };
        stats.volume1_inside += next.volume1 * share;

        let next_spot = position(next.tick, lower, upper);
        if next_spot != spot {
            // A stay outside ends when price is anywhere else.
            if let Some((side, since)) = outside_since.take() {
                stats.excursions.push(Excursion {
                    side,
                    seconds: now - since,
                    returned: true,
                });
            }
            match next_spot {
                Position::Inside => stats.visits += 1,
                Position::Outside(side) => {
                    outside_since = Some((side, now));
                    if last_outside.is_some_and(|previous| previous != side) {
                        match side {
                            Side::Above => stats.passes_up += 1,
                            Side::Below => stats.passes_down += 1,
                        }
                        if let Some(previous) = last_pass_at {
                            stats.pass_intervals.push(now - previous);
                        }
                        last_pass_at = Some(now);
                    }
                    last_outside = Some(side);
                }
            }
            spot = next_spot;
        }
        current = *next;
    }

    spend(&mut stats, spot, &current, to - now);
    if let Some((side, since)) = outside_since {
        stats.excursions.push(Excursion {
            side,
            seconds: to - since,
            returned: false,
        });
    }
    stats.since_last_pass = last_pass_at.map(|at| to - at);
    stats.observed_seconds = to - opened_at;
    stats
}

/// Candidate ranges that cover the ticks between `min_tick` and `max_tick`.
///
/// For each width the ranges tile the span edge to edge, aligned on
/// multiples of the width. Where half the width is itself a valid step, a
/// second tiling shifted by half is added, so a busy level that sits on a
/// seam of the first tiling is in the middle of a range of the second.
/// Widths that are not a positive multiple of the tick spacing are skipped.
pub fn candidate_ranges(
    min_tick: Tick,
    max_tick: Tick,
    tick_spacing: Tick,
    widths: &[Tick],
) -> Vec<(Tick, Tick)> {
    let mut ranges = Vec::new();
    if tick_spacing <= 0 || max_tick < min_tick {
        return ranges;
    }
    for &width in widths {
        if width <= 0 || width % tick_spacing != 0 {
            continue;
        }
        let half = width / 2;
        let step = if width % 2 == 0 && half % tick_spacing == 0 {
            half
        } else {
            width
        };
        // Start one range early so the shifted tiling also covers the bottom.
        let mut lower = min_tick.div_euclid(width) * width - (width - step);
        while lower <= max_tick {
            if lower + width > min_tick {
                ranges.push((lower, lower + width));
            }
            lower += step;
        }
    }
    ranges
}

/// One row of the `zone_map` table.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneRow {
    pub pool: String,
    pub lookback_days: u32,
    pub tick_lower: Tick,
    pub tick_upper: Tick,
    /// Days of the lookback the data covers.
    pub observed_days: f64,
    pub passes_up: u32,
    pub passes_down: u32,
    pub passes_per_day: f64,
    /// Passes per day times the fee tier.
    pub pass_yield_per_day: f64,
    pub time_in_range: f64,
    pub visits: u32,
    /// Chance of being back inside within a day of leaving.
    pub return_odds_1d: Option<f64>,
    pub return_p50_seconds: Option<u64>,
    pub return_p90_seconds: Option<u64>,
    /// Median wait between a full pass and the pass back.
    pub pass_back_p50_seconds: Option<u64>,
    pub pass_back_p90_seconds: Option<u64>,
    pub implied_vol: Option<f64>,
}

impl ZoneRow {
    pub fn from_stats(pool: &str, lookback_days: u32, fee: f64, stats: &ZoneStats) -> Self {
        Self {
            pool: pool.to_string(),
            lookback_days,
            tick_lower: stats.lower,
            tick_upper: stats.upper,
            observed_days: stats.observed_days(),
            passes_up: stats.passes_up,
            passes_down: stats.passes_down,
            passes_per_day: stats.passes_per_day(),
            pass_yield_per_day: stats.pass_yield_per_day(fee),
            time_in_range: stats.time_in_range(),
            visits: stats.visits,
            return_odds_1d: stats.return_odds(DAY, None),
            return_p50_seconds: stats.return_time_quantile(0.5),
            return_p90_seconds: stats.return_time_quantile(0.9),
            pass_back_p50_seconds: stats.pass_interval_quantile(0.5),
            pass_back_p90_seconds: stats.pass_interval_quantile(0.9),
            implied_vol: stats.implied_vol(fee),
        }
    }
}

/// Builds the zone map of one pool: every candidate range on every
/// lookback, ending at `now`. Ranges price never touched are left out.
/// Rows come ordered by lookback, then by passes per day, busiest first.
pub fn build_zone_map(
    pool: &str,
    fee: f64,
    tick_spacing: Tick,
    path: &[PathPoint],
    widths: &[Tick],
    lookbacks_days: &[u32],
    now: u64,
) -> Vec<ZoneRow> {
    let mut rows = Vec::new();
    for &days in lookbacks_days {
        let from = now.saturating_sub(u64::from(days) * DAY);
        let start = path.partition_point(|p| p.time <= from).saturating_sub(1);
        let end = path.partition_point(|p| p.time <= now);
        let window = &path[start.min(end)..end];
        let (Some(min_tick), Some(max_tick)) = (
            window.iter().map(|p| p.tick).min(),
            window.iter().map(|p| p.tick).max(),
        ) else {
            continue;
        };

        let mut of_lookback: Vec<ZoneRow> =
            candidate_ranges(min_tick, max_tick, tick_spacing, widths)
                .into_iter()
                .map(|(lower, upper)| zone_stats(window, lower, upper, from, now))
                .filter(|stats| stats.seconds_inside > 0 || stats.passes() > 0)
                .map(|stats| ZoneRow::from_stats(pool, days, fee, &stats))
                .collect();
        of_lookback.sort_by(|a, b| {
            b.passes_per_day
                .total_cmp(&a.passes_per_day)
                .then(b.time_in_range.total_cmp(&a.time_in_range))
                .then(a.tick_lower.cmp(&b.tick_lower))
                .then(a.tick_upper.cmp(&b.tick_upper))
        });
        rows.extend(of_lookback);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;

    fn point(time: u64, tick: Tick) -> PathPoint {
        PathPoint {
            time,
            tick,
            volume1: 0.0,
            liquidity: 0.0,
        }
    }

    /// A path that visits `ticks` one hour apart, starting at time zero.
    fn hourly(ticks: &[Tick]) -> Vec<PathPoint> {
        ticks
            .iter()
            .enumerate()
            .map(|(i, &tick)| point(i as u64 * HOUR, tick))
            .collect()
    }

    #[test]
    fn a_zigzag_through_the_range_counts_every_full_pass() {
        // Range [100, 200). Below, above, below, above, below: four passes.
        let path = hourly(&[50, 250, 50, 250, 50]);
        let stats = zone_stats(&path, 100, 200, 0, 4 * HOUR);
        assert_eq!((stats.passes_up, stats.passes_down), (2, 2));
        assert_eq!(stats.pass_intervals, vec![HOUR, HOUR, HOUR]);
        assert_eq!(stats.since_last_pass, Some(0));
        assert_eq!(stats.seconds_inside, 0);
        assert_eq!(stats.visits, 0);
        assert_eq!(stats.passes_per_day(), 24.0);
    }

    #[test]
    fn dipping_in_and_back_out_the_same_side_is_not_a_pass() {
        let path = hourly(&[50, 150, 50, 150, 199, 50]);
        let stats = zone_stats(&path, 100, 200, 0, 5 * HOUR);
        assert_eq!(stats.passes(), 0);
        assert_eq!(stats.visits, 2);
        assert_eq!(stats.seconds_inside, 3 * HOUR);
        assert_eq!(stats.time_in_range(), 0.6);
    }

    #[test]
    fn a_pass_may_linger_inside_on_the_way() {
        let path = hourly(&[50, 120, 180, 130, 260, 150, 90]);
        let stats = zone_stats(&path, 100, 200, 0, 6 * HOUR);
        assert_eq!((stats.passes_up, stats.passes_down), (1, 1));
        assert_eq!(stats.pass_intervals, vec![2 * HOUR]);
        assert_eq!(stats.visits, 2);
        assert_eq!(stats.seconds_inside, 4 * HOUR);
    }

    #[test]
    fn the_range_bounds_follow_the_pool_convention() {
        // In range while lower <= tick < upper.
        let path = hourly(&[99, 100, 199, 200, 199, 100, 99]);
        let stats = zone_stats(&path, 100, 200, 0, 6 * HOUR);
        assert_eq!((stats.passes_up, stats.passes_down), (1, 1));
        assert_eq!(stats.seconds_inside, 4 * HOUR);
    }

    #[test]
    fn starting_inside_needs_a_first_exit_before_passes_count() {
        let path = hourly(&[150, 250, 50, 250]);
        let stats = zone_stats(&path, 100, 200, 0, 3 * HOUR);
        // The first exit upward is not a pass; the two crossings after it are.
        assert_eq!((stats.passes_up, stats.passes_down), (1, 1));
    }

    #[test]
    fn stays_outside_are_timed_and_the_last_one_is_marked_open() {
        // Inside, out above for 2 h, inside, out below for 1 h, inside,
        // then out above until the window ends 3 h later.
        let path = hourly(&[150, 250, 250, 150, 50, 150, 250]);
        let stats = zone_stats(&path, 100, 200, 0, 9 * HOUR);
        assert_eq!(
            stats.excursions,
            vec![
                Excursion {
                    side: Side::Above,
                    seconds: 2 * HOUR,
                    returned: true
                },
                Excursion {
                    side: Side::Below,
                    seconds: HOUR,
                    returned: true
                },
                Excursion {
                    side: Side::Above,
                    seconds: 3 * HOUR,
                    returned: false
                },
            ]
        );
        assert_eq!(stats.return_time_quantile(0.5), Some(2 * HOUR));
        assert_eq!(stats.return_time_quantile(0.0), Some(HOUR));
    }

    #[test]
    fn return_odds_leave_out_stays_cut_off_before_the_horizon() {
        let path = hourly(&[150, 250, 250, 150, 50, 150, 250]);
        let stats = zone_stats(&path, 100, 200, 0, 9 * HOUR);
        // Within 1 h: one of three came back in time; the open stay has
        // already lasted longer than the horizon, so it counts as a miss.
        assert_eq!(stats.return_odds(HOUR, None), Some(1.0 / 3.0));
        // Within 2 h: two of three.
        assert_eq!(stats.return_odds(2 * HOUR, None), Some(2.0 / 3.0));
        // Within 5 h: the open stay could still return, so it is left out.
        assert_eq!(stats.return_odds(5 * HOUR, None), Some(1.0));
        assert_eq!(stats.return_odds(HOUR, Some(Side::Below)), Some(1.0));
        assert_eq!(stats.return_odds(HOUR, Some(Side::Above)), Some(0.0));
    }

    #[test]
    fn a_stay_outside_that_began_before_the_window_is_not_timed() {
        let path = hourly(&[250, 250, 150, 250]);
        let stats = zone_stats(&path, 100, 200, 0, 3 * HOUR);
        assert_eq!(stats.excursions.len(), 1);
        assert!(!stats.excursions[0].returned);
        assert_eq!(stats.visits, 1);
    }

    #[test]
    fn the_point_before_the_window_sets_where_price_starts() {
        let path = hourly(&[50, 250, 50, 250, 50]);
        // Window from 1.5 h to 4 h: price starts above, then down, up, down.
        let stats = zone_stats(&path, 100, 200, HOUR + HOUR / 2, 4 * HOUR);
        assert_eq!((stats.passes_up, stats.passes_down), (1, 2));
        assert_eq!(stats.observed_seconds, 2 * HOUR + HOUR / 2);
    }

    #[test]
    fn data_that_starts_late_shortens_the_observed_time() {
        let mut path = hourly(&[50, 250, 50]);
        for p in &mut path {
            p.time += 10 * DAY;
        }
        let stats = zone_stats(&path, 100, 200, 0, 10 * DAY + 2 * HOUR);
        assert_eq!(stats.observed_seconds, 2 * HOUR);
        assert_eq!(stats.passes(), 2);
        assert_eq!(stats.passes_per_day(), 24.0);
    }

    #[test]
    fn empty_and_degenerate_inputs_give_empty_stats() {
        assert_eq!(zone_stats(&[], 100, 200, 0, DAY).observed_seconds, 0);
        let path = hourly(&[50, 250]);
        assert_eq!(zone_stats(&path, 100, 200, DAY, DAY).passes(), 0);
        assert_eq!(zone_stats(&path, 200, 100, 0, DAY).passes(), 0);
        // A path that only starts after the window.
        assert_eq!(
            zone_stats(&path[1..], 100, 200, 0, HOUR / 2).observed_seconds,
            0
        );
        assert_eq!(zone_stats(&[], 100, 200, 0, DAY).passes_per_day(), 0.0);
    }

    #[test]
    fn volume_is_shared_by_how_much_of_a_move_ran_through_the_range() {
        let mut path = hourly(&[0, 400, 150, 150, 500]);
        path[1].volume1 = 1_000.0; // 0 -> 400: a quarter is inside [100, 200)
        path[2].volume1 = 500.0; // 400 -> 150: 50 of 250 ticks
        path[3].volume1 = 70.0; // stays at 150: all inside
        path[4].volume1 = 350.0; // 150 -> 500: 50 of 350 ticks
        let stats = zone_stats(&path, 100, 200, 0, 4 * HOUR);
        assert!((stats.volume1_inside - (250.0 + 100.0 + 70.0 + 50.0)).abs() < 1e-9);
    }

    #[test]
    fn zone_implied_vol_matches_the_pool_formula() {
        // Price sits at tick 0 (price 1) inside [-10, 10) for two days,
        // trading 1,000 a day against liquidity of 100,000.
        let path: Vec<PathPoint> = (0..=48)
            .map(|i| PathPoint {
                time: i * HOUR,
                tick: 0,
                volume1: if i == 0 { 0.0 } else { 1_000.0 / 24.0 },
                liquidity: 100_000.0,
            })
            .collect();
        let stats = zone_stats(&path, -10, 10, 0, 2 * DAY);
        assert_eq!(stats.avg_liquidity(), Some(100_000.0));
        assert!((stats.volume1_per_day_inside().unwrap() - 1_000.0).abs() < 1e-6);
        // 2 * sqrt(0.003 * 1000 / 100000) * sqrt(365) = 0.2093
        let iv = stats.implied_vol(0.003).unwrap();
        assert!((iv - 0.2093).abs() < 0.0005, "{iv}");
        assert_eq!(stats.pass_yield_per_day(0.003), 0.0);
    }

    #[test]
    fn a_zone_price_never_rested_in_has_no_implied_vol() {
        let path = hourly(&[50, 250, 50]);
        let stats = zone_stats(&path, 100, 200, 0, 2 * HOUR);
        assert_eq!(stats.implied_vol(0.003), None);
        assert_eq!(stats.avg_liquidity(), None);
    }

    #[test]
    fn candidate_ranges_tile_the_span_with_a_half_shifted_copy() {
        // Spacing 10, width 20: half the width is a valid step.
        assert_eq!(
            candidate_ranges(5, 45, 10, &[20]),
            vec![(-10, 10), (0, 20), (10, 30), (20, 40), (30, 50), (40, 60)]
        );
        // Width equal to the spacing cannot be shifted by half.
        assert_eq!(
            candidate_ranges(5, 25, 10, &[10]),
            vec![(0, 10), (10, 20), (20, 30)]
        );
    }

    #[test]
    fn candidate_ranges_handle_negative_ticks_and_bad_widths() {
        assert_eq!(
            candidate_ranges(-25, -5, 10, &[10]),
            vec![(-30, -20), (-20, -10), (-10, 0)]
        );
        assert!(candidate_ranges(0, 100, 10, &[15, 0, -10]).is_empty());
        assert!(candidate_ranges(0, 100, 0, &[10]).is_empty());
        assert!(candidate_ranges(100, 0, 10, &[10]).is_empty());
        // Every range touches the span, and the span is covered.
        let ranges = candidate_ranges(-205_337, -204_112, 60, &[60, 120, 240]);
        assert!(ranges
            .iter()
            .all(|&(lo, hi)| hi > -205_337 && lo <= -204_112));
        for tick in [-205_337, -204_800, -204_112] {
            for width in [60, 120, 240] {
                assert!(ranges
                    .iter()
                    .any(|&(lo, hi)| hi - lo == width && lo <= tick && tick < hi));
            }
        }
    }

    #[test]
    fn the_zone_map_finds_the_range_that_is_crossed_most() {
        // Price swings between 95 and 125 every hour for three days, with
        // one excursion far away in the middle.
        let mut ticks = Vec::new();
        for i in 0..72 {
            ticks.push(if i % 2 == 0 { 95 } else { 125 });
        }
        ticks[36] = 405;
        let path = hourly(&ticks);
        let now = 71 * HOUR;
        let rows = build_zone_map("p", 0.0005, 10, &path, &[10, 20], &[1, 7], now);

        let one_day: Vec<&ZoneRow> = rows.iter().filter(|r| r.lookback_days == 1).collect();
        let seven_day: Vec<&ZoneRow> = rows.iter().filter(|r| r.lookback_days == 7).collect();
        assert!(!one_day.is_empty() && !seven_day.is_empty());
        // Rows are grouped by lookback in the order asked for.
        assert_eq!(rows[0].lookback_days, 1);

        // On the last day every range strictly between 95 and 125 is
        // passed 24 times a day.
        let best = one_day[0];
        assert!(best.tick_lower >= 100 && best.tick_upper <= 120, "{best:?}");
        assert_eq!(best.passes_per_day, 24.0);
        assert!((best.pass_yield_per_day - 0.012).abs() < 1e-12);
        assert_eq!(best.observed_days, 1.0);

        // The seven-day lookback only has 71 hours of data, and says so.
        let best7 = seven_day[0];
        assert!((best7.observed_days - 71.0 / 24.0).abs() < 1e-9);
        assert!(best7.passes_per_day > 23.0);
        // The far excursion made ranges up to 405 candidates; the ones it
        // jumped over were passed twice in 71 hours.
        let far = seven_day
            .iter()
            .find(|r| r.tick_lower == 300 && r.tick_upper == 320)
            .unwrap();
        assert_eq!((far.passes_up, far.passes_down), (1, 1));
        assert_eq!(far.pass_back_p50_seconds, Some(HOUR));
    }

    #[test]
    fn the_zone_map_of_an_empty_path_is_empty() {
        assert!(build_zone_map("p", 0.0005, 10, &[], &[10], &[7], DAY).is_empty());
    }
}
