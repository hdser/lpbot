//! Where the zone round trip stands: which range to enter, in tick terms.
//!
//! This is the fixed rule the backtests start from. The long lookback says
//! where price keeps crossing; the short one says whether it still does.
//! The calibrated version of this choice is phase 2.

use lpbot_core::{EntrySide, Tick};

/// What the zone map says about one candidate range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneScore {
    /// Full passes per day on the longest lookback.
    pub passes_per_day_long: f64,
    /// Full passes per day on the shortest lookback.
    pub passes_per_day_short: f64,
    /// Days of history behind the long figure.
    pub observed_days: f64,
}

fn round_down(tick: Tick, step: Tick) -> Tick {
    tick.div_euclid(step) * step
}

/// The distance between neighbouring candidate ranges: half the width
/// where that is a valid tick step, else the width.
pub fn candidate_step(width: Tick, spacing: Tick) -> Tick {
    let half = width / 2;
    if width % 2 == 0 && half > 0 && half % spacing == 0 {
        half
    } else {
        width
    }
}

/// How far a one-sided range is from the price, in ticks. Negative when
/// price is already inside or beyond it.
pub fn gap(tick: Tick, lower: Tick, upper: Tick, side: EntrySide) -> Tick {
    match side {
        EntrySide::Above => lower - tick,
        EntrySide::Below => tick - upper,
    }
}

/// One-sided ranges of `width` on `side` of the price and at most `max_gap`
/// ticks away from it, nearest first. `width` must be a positive multiple
/// of `spacing`.
pub fn entry_candidates(
    tick: Tick,
    spacing: Tick,
    width: Tick,
    side: EntrySide,
    max_gap: Tick,
) -> Vec<(Tick, Tick)> {
    let mut ranges = Vec::new();
    if spacing <= 0 || width <= 0 || width % spacing != 0 {
        return ranges;
    }
    let step = candidate_step(width, spacing);
    match side {
        EntrySide::Above => {
            // All token0 needs the whole range above the current tick.
            let mut lower = round_down(tick, step) + step;
            while lower - tick <= max_gap.max(step) {
                ranges.push((lower, lower + width));
                lower += step;
            }
        }
        EntrySide::Below => {
            // All token1 needs the current tick at or above the upper bound.
            let mut upper = round_down(tick, step);
            while tick - upper <= max_gap.max(step) {
                ranges.push((upper - width, upper));
                upper -= step;
            }
        }
    }
    ranges
}

/// The range of `width` around the price: the one, on the tick spacing,
/// that holds `tick` (`lower <= tick < upper`) and whose middle is nearest
/// to it, the higher one on a tie. Such a range holds both tokens. `None`
/// when `width` is not a positive multiple of `spacing`.
pub fn around_price(tick: Tick, spacing: Tick, width: Tick) -> Option<(Tick, Tick)> {
    if spacing <= 0 || width <= 0 || width % spacing != 0 {
        return None;
    }
    // The lower bound of a range centred exactly on the tick, rounded to
    // the nearest bound on the spacing, then moved one step if that range
    // misses the tick.
    let centred = tick - width / 2;
    let mut lower = (centred + spacing / 2).div_euclid(spacing) * spacing;
    if lower > tick {
        lower -= spacing;
    }
    if lower + width <= tick {
        lower += spacing;
    }
    Some((lower, lower + width))
}

/// Picks the range to enter from candidates listed nearest first.
///
/// Without a day of history there is nothing to rank on, and the nearest
/// range is taken. With history, a range must have been crossed on the
/// short lookback to qualify, and at least `min_passes_per_day` times a day
/// on the long one: a range that only a sudden move crossed, where price
/// has not been trading, is not a zone. Of those, the one crossed most on
/// the long lookback wins; the nearer one wins a tie. `None` means no range
/// qualifies and the bot stays out.
pub fn choose_zone(
    candidates: &[(Tick, Tick)],
    min_passes_per_day: f64,
    mut score: impl FnMut(Tick, Tick) -> ZoneScore,
) -> Option<(Tick, Tick)> {
    let scored: Vec<((Tick, Tick), ZoneScore)> = candidates
        .iter()
        .map(|&(lower, upper)| ((lower, upper), score(lower, upper)))
        .collect();
    if scored.iter().all(|(_, s)| s.observed_days < 1.0) {
        return candidates.first().copied();
    }
    let mut best: Option<((Tick, Tick), f64)> = None;
    for (range, s) in scored {
        if s.passes_per_day_short <= 0.0 || s.passes_per_day_long < min_passes_per_day {
            continue;
        }
        if best.is_none_or(|(_, top)| s.passes_per_day_long > top) {
            best = Some((range, s.passes_per_day_long));
        }
    }
    best.map(|(range, _)| range)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn score(long: f64, short: f64, days: f64) -> ZoneScore {
        ZoneScore {
            passes_per_day_long: long,
            passes_per_day_short: short,
            observed_days: days,
        }
    }

    #[test]
    fn the_range_around_the_price_holds_the_tick_and_is_centred_on_it() {
        // Centred exactly: the tick is the middle.
        assert_eq!(around_price(0, 10, 20), Some((-10, 10)));
        assert_eq!(around_price(180, 60, 120), Some((120, 240)));
        assert_eq!(around_price(200, 60, 120), Some((120, 240)));
        // Between two centred ranges the higher one wins the tie.
        assert_eq!(around_price(5, 10, 20), Some((0, 20)));
        assert_eq!(around_price(4, 10, 20), Some((-10, 10)));
        // A width of one spacing has a single range holding the tick.
        assert_eq!(around_price(7, 10, 10), Some((0, 10)));
        assert_eq!(around_price(10, 10, 10), Some((10, 20)));
        // Negative ticks, as for USDC/WETH, round the same way.
        assert_eq!(around_price(-198_969, 10, 40), Some((-198_990, -198_950)));
        assert_eq!(around_price(-7, 10, 10), Some((-10, 0)));
        for tick in -200..200 {
            for (spacing, width) in [(1, 1), (1, 7), (10, 10), (10, 20), (10, 50), (60, 120)] {
                let (lower, upper) = around_price(tick, spacing, width).unwrap();
                assert!(lower <= tick && tick < upper, "{tick} {spacing} {width}");
                assert_eq!(upper - lower, width);
                assert_eq!(lower.rem_euclid(spacing), 0);
                // No other range on the spacing holding the tick is closer
                // to centred.
                let off = |l: Tick| (2 * (tick - l) - width).abs();
                let mut other = lower - spacing;
                while other + width > tick {
                    assert!(off(other) >= off(lower), "{tick} {spacing} {width}");
                    other -= spacing;
                }
            }
        }
    }

    #[test]
    fn a_width_off_the_spacing_has_no_range_around_the_price() {
        assert_eq!(around_price(0, 10, 15), None);
        assert_eq!(around_price(0, 10, 0), None);
        assert_eq!(around_price(0, 0, 10), None);
    }

    #[test]
    fn candidates_sit_on_the_right_side_nearest_first() {
        // Above: every range starts above the tick, so it holds only token0.
        assert_eq!(
            entry_candidates(203, 10, 20, EntrySide::Above, 30),
            vec![(210, 230), (220, 240), (230, 250)]
        );
        // Below: the tick is at or above every upper bound: only token1.
        assert_eq!(
            entry_candidates(203, 10, 20, EntrySide::Below, 30),
            vec![(180, 200), (170, 190), (160, 180)]
        );
        // On a boundary the range below may end exactly at the tick.
        assert_eq!(
            entry_candidates(200, 10, 20, EntrySide::Below, 0),
            vec![(180, 200), (170, 190)]
        );
        assert_eq!(
            entry_candidates(200, 10, 20, EntrySide::Above, 0),
            vec![(210, 230)]
        );
    }

    #[test]
    fn candidates_work_below_zero_and_reject_bad_widths() {
        assert_eq!(
            entry_candidates(-203, 10, 10, EntrySide::Below, 15),
            vec![(-220, -210)]
        );
        assert_eq!(
            entry_candidates(-203, 10, 10, EntrySide::Above, 15),
            vec![(-200, -190), (-190, -180)]
        );
        assert!(entry_candidates(0, 10, 15, EntrySide::Above, 100).is_empty());
        assert!(entry_candidates(0, 0, 10, EntrySide::Above, 100).is_empty());
        for (lower, upper) in entry_candidates(-7, 60, 120, EntrySide::Below, 500) {
            assert!(upper <= -7 && (upper - lower) == 120 && lower % 60 == 0);
        }
    }

    #[test]
    fn the_step_is_half_the_width_only_where_that_is_a_tick_step() {
        assert_eq!(candidate_step(20, 10), 10);
        assert_eq!(candidate_step(10, 10), 10);
        assert_eq!(candidate_step(30, 10), 30);
        assert_eq!(candidate_step(120, 60), 60);
    }

    #[test]
    fn the_gap_is_measured_from_the_near_edge() {
        assert_eq!(gap(203, 210, 230, EntrySide::Above), 7);
        assert_eq!(gap(203, 180, 200, EntrySide::Below), 3);
        assert_eq!(gap(215, 210, 230, EntrySide::Above), -5);
    }

    #[test]
    fn the_most_crossed_range_that_is_still_active_wins() {
        let candidates = [(0, 10), (10, 20), (20, 30)];
        let by_lower = |lower: Tick, _: Tick| match lower {
            0 => score(2.0, 1.0, 30.0),
            10 => score(9.0, 0.0, 30.0), // crossed most, but not lately
            _ => score(5.0, 0.5, 30.0),
        };
        assert_eq!(choose_zone(&candidates, 0.0, by_lower), Some((20, 30)));
        // A tie goes to the nearer range.
        assert_eq!(
            choose_zone(&candidates, 0.0, |_, _| score(3.0, 1.0, 30.0)),
            Some((0, 10))
        );
        // Nothing crossed lately: stay out.
        assert_eq!(
            choose_zone(&candidates, 0.0, |_, _| score(3.0, 0.0, 30.0)),
            None
        );
    }

    #[test]
    fn without_history_the_nearest_range_is_taken() {
        let candidates = [(0, 10), (10, 20)];
        assert_eq!(
            choose_zone(&candidates, 0.0, |_, _| score(0.0, 0.0, 0.2)),
            Some((0, 10))
        );
        assert_eq!(choose_zone(&[], 0.0, |_, _| score(1.0, 1.0, 9.0)), None);
    }

    #[test]
    fn a_range_must_have_been_crossed_often_enough_on_the_long_lookback() {
        let candidates = [(0, 10), (10, 20), (20, 30)];
        // The nearest range was crossed only by a recent sudden move.
        let by_lower = |lower: Tick, _: Tick| match lower {
            0 => score(0.05, 2.0, 90.0),
            10 => score(0.8, 0.5, 90.0),
            _ => score(0.3, 0.2, 90.0),
        };
        assert_eq!(choose_zone(&candidates, 0.0, by_lower), Some((10, 20)));
        assert_eq!(choose_zone(&candidates, 0.5, by_lower), Some((10, 20)));
        // Only ranges that are zones on the long lookback qualify.
        let wick = |lower: Tick, _: Tick| match lower {
            0 => score(0.05, 2.0, 90.0),
            _ => score(0.0, 0.0, 90.0),
        };
        assert_eq!(choose_zone(&candidates, 0.0, wick), Some((0, 10)));
        assert_eq!(choose_zone(&candidates, 0.1, wick), None);
    }
}
