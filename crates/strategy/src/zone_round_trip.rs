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

/// Picks the range to enter from candidates listed nearest first.
///
/// Without a day of history there is nothing to rank on, and the nearest
/// range is taken. With history, a range must have been crossed on the
/// short lookback to qualify, and the one crossed most on the long
/// lookback wins; the nearer one wins a tie. `None` means no range
/// qualifies and the bot stays out.
pub fn choose_zone(
    candidates: &[(Tick, Tick)],
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
        if s.passes_per_day_short <= 0.0 {
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
        assert_eq!(choose_zone(&candidates, by_lower), Some((20, 30)));
        // A tie goes to the nearer range.
        assert_eq!(
            choose_zone(&candidates, |_, _| score(3.0, 1.0, 30.0)),
            Some((0, 10))
        );
        // Nothing crossed lately: stay out.
        assert_eq!(choose_zone(&candidates, |_, _| score(3.0, 0.0, 30.0)), None);
    }

    #[test]
    fn without_history_the_nearest_range_is_taken() {
        let candidates = [(0, 10), (10, 20)];
        assert_eq!(
            choose_zone(&candidates, |_, _| score(0.0, 0.0, 0.2)),
            Some((0, 10))
        );
        assert_eq!(choose_zone(&[], |_, _| score(1.0, 1.0, 9.0)), None);
    }
}
