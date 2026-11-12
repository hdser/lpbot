//! Limits and exit paths.
//!
//! Only the loss limit exists so far. It is the hard backstop under every
//! adaptive decision: a required run input, counted in the value token.

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
    fn untrustworthy_values_count_as_a_breach() {
        let limit = LossLimit::new(5.0).unwrap();
        assert!(matches!(limit.check(0.0, 1.0), LossCheck::Breached { .. }));
        assert!(matches!(
            limit.check(10.0, f64::NAN),
            LossCheck::Breached { .. }
        ));
    }
}
