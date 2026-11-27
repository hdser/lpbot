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

    /// Realized volatility over the `window` seconds before `now`, scaled
    /// to a day, in ticks: the standard deviation of the closing tick's
    /// change from one bucket to the next, times the square root of the
    /// buckets in a day. A tick is about 0.01% of the price. Only buckets
    /// that ended before `now` count. `None` with fewer than six changes
    /// to measure.
    pub fn daily_vol_ticks(&self, now: u64, window: u64) -> Option<f64> {
        let start = self.start?;
        let end = (now.checked_sub(start)? / self.step) as usize;
        let end = end.min(self.close.len());
        let wanted = (window / self.step) as usize + 1;
        let closes = &self.close[end.saturating_sub(wanted)..end];
        if closes.len() < 7 {
            return None;
        }
        let changes: Vec<f64> = closes.windows(2).map(|w| f64::from(w[1] - w[0])).collect();
        let n = changes.len() as f64;
        let mean = changes.iter().sum::<f64>() / n;
        let variance = changes.iter().map(|c| (c - mean).powi(2)).sum::<f64>() / (n - 1.0);
        Some(variance.sqrt() * (86_400.0 / self.step as f64).sqrt())
    }

    /// How far the price moved over the `window` seconds before `now`, in
    /// ticks: the close of the last bucket that ended by `now` less the
    /// close `window` earlier. `None` without that much history.
    pub fn move_ticks(&self, now: u64, window: u64) -> Option<Tick> {
        let start = self.start?;
        let end = ((now.checked_sub(start)? / self.step) as usize).min(self.close.len());
        let back = (window / self.step) as usize;
        if end < back + 1 {
            return None;
        }
        Some(self.close[end - 1] - self.close[end - 1 - back])
    }

    /// The buckets that ended by `now` and cover the `window` seconds
    /// before it, as indexes into the buckets: the first, and one past the
    /// last. `None` without that much history.
    fn window_buckets(&self, now: u64, window: u64) -> Option<(usize, usize)> {
        let start = self.start?;
        let end = ((now.checked_sub(start)? / self.step) as usize).min(self.close.len());
        let back = (window / self.step) as usize;
        (back > 0 && end > back).then(|| (end - back, end))
    }

    /// The band the price held over the `window` seconds before `now`: its
    /// lowest and highest tick, and the close of the last bucket that
    /// ended by `now`. `None` without that much history.
    pub fn band_ticks(&self, now: u64, window: u64) -> Option<(Tick, Tick, Tick)> {
        let (first, end) = self.window_buckets(now, window)?;
        let low = self.low[first..end].iter().copied().min()?;
        let high = self.high[first..end].iter().copied().max()?;
        Some((low, high, self.close[end - 1]))
    }

    /// How straight the price went over the `window` seconds before `now`:
    /// the window is cut into `steps` equal steps, and the move from its
    /// first close to its last is divided by the sum of the steps' moves,
    /// each counted as positive. 1 is a price that only went one way; a
    /// price that came back to where it started is 0; a random walk gives
    /// about one over the square root of `steps`. A price that did not
    /// move at all is 0. `None` without that much history, or with a step
    /// shorter than a bucket.
    pub fn efficiency(&self, now: u64, window: u64, steps: usize) -> Option<f64> {
        let (first, end) = self.window_buckets(now, window)?;
        // The close before the window opens is where its first step starts.
        let buckets = end - first;
        if steps == 0 || buckets < steps {
            return None;
        }
        let close_at = |step: usize| self.close[first - 1 + buckets * step / steps];
        let mut path = 0i64;
        for step in 1..=steps {
            path += i64::from((close_at(step) - close_at(step - 1)).abs());
        }
        if path == 0 {
            return Some(0.0);
        }
        let net = i64::from((close_at(steps) - close_at(0)).abs());
        Some(net as f64 / path as f64)
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
    fn the_move_over_a_window_is_close_to_close() {
        let mut buckets = TickBuckets::new(600);
        assert_eq!(buckets.move_ticks(10_000, 1_200), None);
        // Closes 0, 10, 30, 60 in the buckets starting at 0, 600, 1200, 1800.
        for (time, tick) in [(0, 0), (700, 10), (1_300, 30), (1_900, 60)] {
            buckets.push(time, tick);
        }
        // At 2400 the bucket from 1800 has ended: 60 against 30 and 0.
        assert_eq!(buckets.move_ticks(2_400, 600), Some(30));
        assert_eq!(buckets.move_ticks(2_400, 1_800), Some(60));
        // Only buckets that ended count: at 2399 the last one is still open.
        assert_eq!(buckets.move_ticks(2_399, 600), Some(20));
        // Not that much history.
        assert_eq!(buckets.move_ticks(2_400, 2_400), None);
    }

    #[test]
    fn the_band_is_the_lowest_and_highest_tick_of_the_window() {
        let mut buckets = TickBuckets::new(HOUR);
        // Hour 0 closes at 10 after touching 50; hours 1 to 3 close at 20,
        // 5 and 30.
        buckets.push(10, 50);
        buckets.push(20, 10);
        buckets.push(HOUR + 10, 20);
        buckets.push(2 * HOUR + 10, 5);
        buckets.push(3 * HOUR + 10, 30);
        // At hour 4 all four hours have ended.
        assert_eq!(buckets.band_ticks(4 * HOUR, 3 * HOUR), Some((5, 30, 30)));
        // The spike to 50 in the first hour is outside both windows.
        assert_eq!(buckets.band_ticks(4 * HOUR, 2 * HOUR), Some((5, 30, 30)));
        assert_eq!(buckets.band_ticks(3 * HOUR, 2 * HOUR), Some((5, 20, 5)));
        // The hour still filling is left out.
        assert_eq!(buckets.band_ticks(4 * HOUR - 1, 2 * HOUR), Some((5, 20, 5)));
        // A window as long as the history, or none at all, has no band.
        assert_eq!(buckets.band_ticks(4 * HOUR, 4 * HOUR), None);
        assert_eq!(buckets.band_ticks(4 * HOUR, 0), None);
        assert_eq!(TickBuckets::new(HOUR).band_ticks(HOUR, HOUR), None);
    }

    #[test]
    fn efficiency_is_one_for_a_straight_move_and_low_for_a_price_that_comes_back() {
        // Up ten ticks an hour.
        let straight = hourly(&(0..30).map(|i| i * 10).collect::<Vec<Tick>>());
        assert_eq!(straight.efficiency(30 * HOUR, 24 * HOUR, 24), Some(1.0));
        assert_eq!(straight.efficiency(30 * HOUR, 24 * HOUR, 6), Some(1.0));
        // Down just the same.
        let down = hourly(&(0..30).map(|i| -i * 10).collect::<Vec<Tick>>());
        assert_eq!(down.efficiency(30 * HOUR, 24 * HOUR, 24), Some(1.0));
        // Up and down by ten every hour: back where it started.
        let chop = hourly(&(0..30).map(|i| (i % 2) * 10).collect::<Vec<Tick>>());
        assert_eq!(chop.efficiency(30 * HOUR, 24 * HOUR, 24), Some(0.0));
        // Twelve hours up by ten, then twelve flat hours: twelve steps of
        // ten, all one way.
        let half = hourly(&(0..30).map(|i| i.min(17) * 10).collect::<Vec<Tick>>());
        assert_eq!(half.efficiency(30 * HOUR, 24 * HOUR, 24), Some(1.0));
        // Three hours up by ten, then down by ten, repeated: over 24 hours
        // a net of 120 ticks for 240 walked.
        let closes: Vec<Tick> = (0..33)
            .map(|i| (i / 4) * 20 + [0, 10, 20, 30][(i % 4) as usize])
            .collect();
        let saw = hourly(&closes);
        assert_eq!(closes[32] - closes[8], 120);
        assert_eq!(saw.efficiency(33 * HOUR, 24 * HOUR, 24), Some(0.5));
        // Measured in six steps of four hours it only ever rose.
        assert_eq!(saw.efficiency(33 * HOUR, 24 * HOUR, 6), Some(1.0));
        // A flat price went nowhere.
        assert_eq!(
            hourly(&[7; 30]).efficiency(30 * HOUR, 24 * HOUR, 24),
            Some(0.0)
        );
        // Not enough history, no steps, or steps shorter than a bucket.
        assert_eq!(straight.efficiency(30 * HOUR, 30 * HOUR, 24), None);
        assert_eq!(straight.efficiency(30 * HOUR, 24 * HOUR, 0), None);
        assert_eq!(straight.efficiency(30 * HOUR, 12 * HOUR, 24), None);
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

    #[test]
    fn realized_volatility_is_scaled_to_a_day_and_needs_enough_buckets() {
        let mut buckets = TickBuckets::new(600);
        // A tick that alternates up and down by 10 every ten minutes.
        for i in 0..200u64 {
            buckets.push(i * 600, if i % 2 == 0 { 0 } else { 10 });
        }
        let vol = buckets.daily_vol_ticks(200 * 600, 86_400).unwrap();
        // Changes of +10 and -10: a standard deviation of about 10 per
        // bucket, 144 buckets a day.
        assert!((vol / (10.0 * 12.0) - 1.0).abs() < 0.01, "{vol}");
        // A flat price has none.
        let mut flat = TickBuckets::new(600);
        for i in 0..50u64 {
            flat.push(i * 600, 7);
        }
        assert_eq!(flat.daily_vol_ticks(50 * 600, 86_400), Some(0.0));
        // Too little history, or a time before it, gives nothing.
        assert_eq!(flat.daily_vol_ticks(5 * 600, 86_400), None);
        assert_eq!(TickBuckets::new(600).daily_vol_ticks(1_000, 86_400), None);
    }
}
