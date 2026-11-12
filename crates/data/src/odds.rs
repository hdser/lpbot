//! How often price has made a move of a given size within a given time.
//!
//! The patience decision asks: price is this many ticks away from where it
//! has to get to; how often, in this pool's own recent history, did it
//! cover that distance within the horizon? And when it did not, where was
//! it at the end? Both are counted here, from every starting point in the
//! lookback, on a price path thinned into fixed time steps.
//!
//! This is the plain frequency over the lookback. It does not yet condition
//! on the state the pool is in.

use lpbot_core::Tick;

/// The price path in fixed steps: last, highest and lowest tick of each.
#[derive(Debug, Clone, PartialEq)]
pub struct TickBuckets {
    step: u64,
    /// Start time of the first bucket.
    start: Option<u64>,
    close: Vec<Tick>,
    high: Vec<Tick>,
    low: Vec<Tick>,
}

/// What history says about a move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveOdds {
    /// Share of starting points from which the move was made in time.
    pub p_hit: f64,
    /// Average tick change over the horizon from the starting points that
    /// did not make it. Zero when all of them did.
    pub drift_if_missed: f64,
    /// Starting points counted.
    pub samples: usize,
}

impl TickBuckets {
    /// `step` is the bucket length in seconds.
    pub fn new(step: u64) -> Self {
        Self {
            step: step.max(1),
            start: None,
            close: Vec::new(),
            high: Vec::new(),
            low: Vec::new(),
        }
    }

    pub fn step(&self) -> u64 {
        self.step
    }

    pub fn len(&self) -> usize {
        self.close.len()
    }

    pub fn is_empty(&self) -> bool {
        self.close.is_empty()
    }

    /// Adds a price observation. Times must not go backwards.
    pub fn push(&mut self, time: u64, tick: Tick) {
        let start = *self.start.get_or_insert(time - time % self.step);
        let index = (time.saturating_sub(start) / self.step) as usize;
        // Quiet buckets carry the last price forward.
        while self.close.len() <= index {
            let last = self.close.last().copied().unwrap_or(tick);
            self.close.push(last);
            self.high.push(last);
            self.low.push(last);
        }
        let i = self.close.len() - 1;
        self.close[i] = tick;
        self.high[i] = self.high[i].max(tick);
        self.low[i] = self.low[i].min(tick);
    }

    /// Odds of a move of `delta` ticks (up when positive, down when
    /// negative) within `horizon` seconds, over starting points in the
    /// `lookback` seconds before `now`. `None` when the history holds no
    /// starting point with a full horizon after it.
    pub fn move_odds(
        &self,
        now: u64,
        lookback: u64,
        delta: Tick,
        horizon: u64,
    ) -> Option<MoveOdds> {
        let start = self.start?;
        let ahead = (horizon / self.step).max(1) as usize;
        // The bucket `now` falls in is still filling; stop before it.
        let complete = ((now.saturating_sub(start)) / self.step) as usize;
        let complete = complete.min(self.close.len());
        let first = (now.saturating_sub(lookback).saturating_sub(start) / self.step) as usize;
        if complete <= ahead || first + ahead >= complete {
            return None;
        }

        let (mut hits, mut misses, mut drift) = (0usize, 0usize, 0f64);
        for i in first..complete - ahead {
            let from = self.close[i];
            let window = i + 1..=i + ahead;
            let hit = if delta >= 0 {
                self.high[window].iter().any(|high| high - from >= delta)
            } else {
                self.low[window].iter().any(|low| low - from <= delta)
            };
            if hit {
                hits += 1;
            } else {
                misses += 1;
                drift += f64::from(self.close[i + ahead] - from);
            }
        }
        let samples = hits + misses;
        Some(MoveOdds {
            p_hit: hits as f64 / samples as f64,
            drift_if_missed: if misses > 0 {
                drift / misses as f64
            } else {
                0.0
            },
            samples,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3_600;

    fn hourly(ticks: &[Tick]) -> TickBuckets {
        let mut buckets = TickBuckets::new(HOUR);
        for (i, tick) in ticks.iter().enumerate() {
            buckets.push(i as u64 * HOUR + 10, *tick);
        }
        buckets
    }

    #[test]
    fn buckets_keep_the_last_highest_and_lowest_tick_and_fill_gaps() {
        let mut buckets = TickBuckets::new(HOUR);
        buckets.push(HOUR + 5, 100);
        buckets.push(HOUR + 900, 140);
        buckets.push(HOUR + 1_800, 90);
        buckets.push(HOUR + 3_500, 120);
        // Nothing for two hours, then one observation.
        buckets.push(4 * HOUR + 1, 80);
        assert_eq!(buckets.len(), 4);
        assert_eq!(buckets.close, vec![120, 120, 120, 80]);
        assert_eq!(buckets.high, vec![140, 120, 120, 120]);
        assert_eq!(buckets.low, vec![90, 120, 120, 80]);
        assert_eq!(buckets.step(), HOUR);
        assert!(TickBuckets::new(60).is_empty());
    }

    #[test]
    fn odds_count_the_starting_points_that_made_the_move_in_time() {
        // A sawtooth: up 10 an hour for three hours, then back down 30.
        let ticks: Vec<Tick> = (0..40).map(|i| [0, 10, 20, 30][i % 4]).collect();
        let buckets = hourly(&ticks);
        let now = 40 * HOUR;
        // Up 10 within an hour: from 0, 10 and 20 yes; from 30 no.
        let odds = buckets.move_odds(now, 100 * HOUR, 10, HOUR).unwrap();
        assert_eq!(odds.samples, 39);
        assert!((odds.p_hit - 30.0 / 39.0).abs() < 1e-12, "{}", odds.p_hit);
        // Those that missed all started at 30 and fell to 0.
        assert_eq!(odds.drift_if_missed, -30.0);
        // Up 30 within three hours: only from the bottom of the tooth.
        let odds = buckets.move_odds(now, 100 * HOUR, 30, 3 * HOUR).unwrap();
        assert_eq!(odds.samples, 37);
        assert!((odds.p_hit - 10.0 / 37.0).abs() < 1e-12);
        // Down 30 within an hour: only from the top.
        let odds = buckets.move_odds(now, 100 * HOUR, -30, HOUR).unwrap();
        assert!((odds.p_hit - 9.0 / 39.0).abs() < 1e-12, "{}", odds.p_hit);
        // A move that never happened.
        assert_eq!(
            buckets
                .move_odds(now, 100 * HOUR, 31, 3 * HOUR)
                .unwrap()
                .p_hit,
            0.0
        );
        // Down at all within a full tooth: from everywhere but the bottom,
        // and from the bottom the price is back where it was.
        let down = buckets.move_odds(now, 100 * HOUR, -1, 4 * HOUR).unwrap();
        assert_eq!((down.p_hit, down.drift_if_missed), (0.75, 0.0));
        // Up at all within a full tooth: from everywhere but the top.
        let up = buckets.move_odds(now, 100 * HOUR, 1, 4 * HOUR).unwrap();
        assert_eq!((up.p_hit, up.drift_if_missed), (0.75, 0.0));
    }

    #[test]
    fn a_spike_inside_a_bucket_counts_as_reached() {
        let mut buckets = TickBuckets::new(HOUR);
        buckets.push(10, 0);
        buckets.push(HOUR + 10, 50); // spike
        buckets.push(HOUR + 20, 0); // and back within the same hour
        buckets.push(2 * HOUR + 10, 0);
        let odds = buckets
            .move_odds(2 * HOUR + 30, 10 * HOUR, 40, HOUR)
            .unwrap();
        assert_eq!((odds.samples, odds.p_hit), (1, 1.0));
    }

    #[test]
    fn only_the_lookback_is_counted_and_only_complete_horizons() {
        // Flat for 50 hours, then the sawtooth.
        let mut ticks = vec![0; 50];
        ticks.extend((0..40).map(|i| [0, 10, 20, 30][i % 4]));
        let buckets = hourly(&ticks);
        let now = 90 * HOUR;
        let all = buckets.move_odds(now, 1_000 * HOUR, 10, HOUR).unwrap();
        let recent = buckets.move_odds(now, 40 * HOUR, 10, HOUR).unwrap();
        assert_eq!(all.samples, 89);
        assert_eq!(recent.samples, 39);
        assert!(recent.p_hit > all.p_hit);

        // Not enough history for the horizon: no answer.
        assert_eq!(buckets.move_odds(now, 1_000 * HOUR, 10, 200 * HOUR), None);
        assert_eq!(buckets.move_odds(now, 2 * HOUR, 10, 5 * HOUR), None);
        assert_eq!(TickBuckets::new(HOUR).move_odds(now, HOUR, 1, HOUR), None);
        // The bucket still filling at `now` is not used as an end point.
        let early = buckets
            .move_odds(60 * HOUR + 1, 1_000 * HOUR, 10, HOUR)
            .unwrap();
        assert_eq!(early.samples, 59);
    }
}
