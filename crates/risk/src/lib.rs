//! Limits and exit paths.
//!
//! Two limits exist so far. The loss limit is the hard backstop under every
//! adaptive decision: a required run input, counted in the value token. The
//! liquidity share cap shrinks a position to a share of the liquidity the
//! pool has where it goes.

/// Parts per million: how shares that decide token amounts are held, so
/// the amounts themselves are exact integer math.
pub const MILLION: u32 = 1_000_000;

/// A share between 0 and 1 in parts per million, rounded to the nearest.
/// `None` for anything outside 0 to 1, or not a number.
pub fn parts_per_million(share: f64) -> Option<u32> {
    (share.is_finite() && (0.0..=1.0).contains(&share))
        .then(|| (share * f64::from(MILLION)).round() as u32)
}

/// The largest share of a range's liquidity the bot may own
/// (`universe.max_share_of_in_range_liquidity`). A pre-trade limit: it
/// shrinks a position to fit and never grows one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiquidityShareCap {
    ppm: u32,
}

impl LiquidityShareCap {
    /// `share` must be above 0 and at most 1.
    pub fn new(share: f64) -> Option<Self> {
        parts_per_million(share)
            .filter(|ppm| *ppm > 0)
            .map(|ppm| Self { ppm })
    }

    pub fn parts_per_million(&self) -> u32 {
        self.ppm
    }

    /// The most liquidity the bot may hold beside `pool_liquidity`,
    /// rounded down.
    pub fn limit(&self, pool_liquidity: u128) -> u128 {
        let (million, ppm) = (u128::from(MILLION), u128::from(self.ppm));
        // Split so nothing overflows: whole millions, then the rest.
        pool_liquidity / million * ppm + pool_liquidity % million * ppm / million
    }

    /// `liquidity` shrunk to the limit, and whether it had to be.
    pub fn shrink(&self, liquidity: u128, pool_liquidity: u128) -> (u128, bool) {
        let limit = self.limit(pool_liquidity);
        if liquidity > limit {
            (limit, true)
        } else {
            (liquidity, false)
        }
    }
}

/// The run's loss limit, in percent of the value-token amount it started with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LossLimit {
    limit_pct: f64,
}

/// Result of checking the loss limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LossCheck {
    Within {
        loss_pct: f64,
    },
    /// The limit is hit: exit everything and pause.
    Breached {
        loss_pct: f64,
    },
}

impl LossLimit {
    /// `limit_pct` must be greater than 0 and at most 100.
    pub fn new(limit_pct: f64) -> Option<Self> {
        (limit_pct.is_finite() && limit_pct > 0.0 && limit_pct <= 100.0)
            .then_some(Self { limit_pct })
    }

    pub fn limit_pct(&self) -> f64 {
        self.limit_pct
    }

    /// Compares the value at the start of the loss window with the value now,
    /// both in the value token. A gain is a negative loss.
    pub fn check(&self, start_value: f64, current_value: f64) -> LossCheck {
        if !(start_value.is_finite() && start_value > 0.0 && current_value.is_finite()) {
            // Values that cannot be trusted are treated as a breach.
            return LossCheck::Breached { loss_pct: f64::NAN };
        }
        let loss_pct = (start_value - current_value) / start_value * 100.0;
        if loss_pct >= self.limit_pct {
            LossCheck::Breached { loss_pct }
        } else {
            LossCheck::Within { loss_pct }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_limits_outside_zero_to_one_hundred() {
        assert!(LossLimit::new(0.0).is_none());
        assert!(LossLimit::new(-1.0).is_none());
        assert!(LossLimit::new(100.1).is_none());
        assert!(LossLimit::new(f64::NAN).is_none());
        assert!(LossLimit::new(5.0).is_some());
    }

    #[test]
    fn breaches_at_the_limit_and_beyond() {
        let limit = LossLimit::new(5.0).unwrap();
        assert!(matches!(limit.check(10.0, 9.6), LossCheck::Within { .. }));
        assert!(matches!(limit.check(10.0, 9.5), LossCheck::Breached { .. }));
        assert!(matches!(limit.check(10.0, 8.0), LossCheck::Breached { .. }));
    }

    #[test]
    fn a_gain_is_within_the_limit() {
        let limit = LossLimit::new(5.0).unwrap();
        match limit.check(10.0, 11.0) {
            LossCheck::Within { loss_pct } => assert!(loss_pct < 0.0),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn shares_become_parts_per_million() {
        assert_eq!(parts_per_million(0.05), Some(50_000));
        assert_eq!(parts_per_million(0.25), Some(250_000));
        assert_eq!(parts_per_million(1.0), Some(MILLION));
        assert_eq!(parts_per_million(0.0), Some(0));
        assert_eq!(parts_per_million(0.000_000_4), Some(0));
        assert_eq!(parts_per_million(1.5), None);
        assert_eq!(parts_per_million(-0.1), None);
        assert_eq!(parts_per_million(f64::NAN), None);
    }

    #[test]
    fn the_share_cap_is_above_zero_and_at_most_one() {
        assert!(LiquidityShareCap::new(0.0).is_none());
        assert!(LiquidityShareCap::new(1.01).is_none());
        assert!(LiquidityShareCap::new(f64::INFINITY).is_none());
        assert_eq!(
            LiquidityShareCap::new(0.05).unwrap().parts_per_million(),
            50_000
        );
        assert!(LiquidityShareCap::new(1.0).is_some());
    }

    #[test]
    fn the_share_cap_shrinks_and_never_grows() {
        let cap = LiquidityShareCap::new(0.05).unwrap();
        assert_eq!(cap.limit(1_000_000), 50_000);
        // Rounded down, exactly.
        assert_eq!(cap.limit(999), 49);
        assert_eq!(cap.limit(0), 0);
        // No overflow at the top of the range.
        assert_eq!(cap.limit(u128::MAX), u128::MAX / 20);
        assert_eq!(cap.shrink(60_000, 1_000_000), (50_000, true));
        assert_eq!(cap.shrink(40_000, 1_000_000), (40_000, false));
        assert_eq!(cap.shrink(50_000, 1_000_000), (50_000, false));
        assert_eq!(cap.shrink(1, 0), (0, true));
    }

    #[test]
    fn untrustworthy_values_count_as_a_breach() {
        let limit = LossLimit::new(5.0).unwrap();
        assert!(matches!(limit.check(0.0, 1.0), LossCheck::Breached { .. }));
        assert!(matches!(
            limit.check(10.0, f64::NAN),
            LossCheck::Breached { .. }
        ));
    }
}
