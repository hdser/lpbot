//! The adaptive patience decision.
//!
//! When price has left a position wholly in the other token, the bot
//! compares what it expects from waiting, from flipping and from closing,
//! all in the value token, and takes the best. A switch away from waiting
//! needs a clear margin, so the bot does not flip back and forth.
//!
//! This module only does the comparison. The figures come from the pool's
//! own history and from the position, and are worked out by whoever calls.

/// Settings of the decision. All of them are meant to be calibrated.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveParams {
    /// How far ahead the comparison looks, in seconds.
    pub horizon: u64,
    /// What flipping or closing must beat waiting by, as a fraction of the
    /// position's value.
    pub switch_margin: f64,
    /// Share of the best zone's yield credited to capital that closing
    /// frees: what waiting costs. Between 0 and 1.
    pub reinvest_credit: f64,
    /// History the odds are counted over, in days.
    pub lookback_days: u32,
}

impl Default for AdaptiveParams {
    fn default() -> Self {
        Self {
            horizon: 86_400,
            switch_margin: 0.002,
            reinvest_credit: 0.25,
            lookback_days: 30,
        }
    }
}

/// An action whose result depends on whether price makes a move in time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gamble {
    /// Odds that price makes the move within the horizon.
    pub odds: f64,
    /// Result in the value token if it does.
    pub if_made: f64,
    /// Result in the value token if it does not.
    pub if_missed: f64,
}

impl Gamble {
    pub fn expected(&self) -> f64 {
        let odds = self.odds.clamp(0.0, 1.0);
        odds * self.if_made + (1.0 - odds) * self.if_missed
    }
}

/// The three options, each as what it leaves in the value token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Options {
    /// Leave the position; it pays if price comes back through its range.
    pub wait: Gamble,
    /// Move next to the price; it pays if price passes through the new
    /// range. `None` when there is nowhere nearer to move to.
    pub flip: Option<Gamble>,
    /// Swap back now, with the credit for what the freed capital can earn.
    pub close: f64,
    /// What the position is worth now, to scale the margin.
    pub position_value: f64,
}

/// Expected result of each option, in the value token.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Expected {
    pub wait: f64,
    pub flip: Option<f64>,
    pub close: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Wait,
    Flip,
    Close,
}

pub fn expected(options: &Options) -> Expected {
    Expected {
        wait: options.wait.expected(),
        flip: options.flip.map(|flip| flip.expected()),
        close: options.close,
    }
}

/// Waits unless flipping or closing is expected to do better by more than
/// the margin. Between those two the better one is taken; closing wins a tie.
pub fn choose(expected: &Expected, position_value: f64, switch_margin: f64) -> Choice {
    let (best, value) = match expected.flip {
        Some(flip) if flip > expected.close => (Choice::Flip, flip),
        _ => (Choice::Close, expected.close),
    };
    let margin = position_value.abs() * switch_margin.max(0.0);
    if value.is_finite() && expected.wait.is_finite() && value - expected.wait > margin {
        best
    } else {
        Choice::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(wait_odds: f64, flip_odds: Option<f64>) -> Options {
        Options {
            // Back through the range returns 100; stuck leaves 95.
            wait: Gamble {
                odds: wait_odds,
                if_made: 100.0,
                if_missed: 95.0,
            },
            // A pass through the nearer range returns 98; stuck leaves 94.9.
            flip: flip_odds.map(|odds| Gamble {
                odds,
                if_made: 98.0,
                if_missed: 94.9,
            }),
            close: 96.0,
            position_value: 96.0,
        }
    }

    #[test]
    fn a_gamble_is_worth_its_odds_weighted_results() {
        let gamble = Gamble {
            odds: 0.25,
            if_made: 100.0,
            if_missed: 80.0,
        };
        assert_eq!(gamble.expected(), 85.0);
        // Odds outside 0 to 1 are clamped, never extrapolated.
        assert_eq!(
            Gamble {
                odds: 1.5,
                ..gamble
            }
            .expected(),
            100.0
        );
        assert_eq!(
            Gamble {
                odds: -1.0,
                ..gamble
            }
            .expected(),
            80.0
        );
    }

    #[test]
    fn good_return_odds_mean_waiting() {
        let e = expected(&options(0.8, Some(0.9)));
        assert_eq!(e.wait, 99.0);
        assert_eq!(choose(&e, 96.0, 0.002), Choice::Wait);
    }

    #[test]
    fn poor_return_odds_and_a_busy_range_nearby_mean_flipping() {
        let e = expected(&options(0.05, Some(0.9)));
        assert!((e.wait - 95.25).abs() < 1e-12);
        assert!((e.flip.unwrap() - 97.69).abs() < 1e-12);
        assert_eq!(choose(&e, 96.0, 0.002), Choice::Flip);
    }

    #[test]
    fn poor_odds_everywhere_mean_closing() {
        let e = expected(&options(0.05, Some(0.1)));
        assert_eq!(choose(&e, 96.0, 0.002), Choice::Close);
        // With nowhere to flip to, the choice is between waiting and closing.
        let e = expected(&options(0.05, None));
        assert_eq!(e.flip, None);
        assert_eq!(choose(&e, 96.0, 0.002), Choice::Close);
    }

    #[test]
    fn a_switch_needs_to_clear_the_margin() {
        // Closing beats waiting by 0.75 on a position worth 96: 0.78%.
        let e = expected(&options(0.05, None));
        assert_eq!(choose(&e, 96.0, 0.007), Choice::Close);
        assert_eq!(choose(&e, 96.0, 0.008), Choice::Wait);
        // Figures that cannot be trusted never trigger a switch.
        let broken = Expected {
            wait: f64::NAN,
            flip: None,
            close: 96.0,
        };
        assert_eq!(choose(&broken, 96.0, 0.0), Choice::Wait);
        let broken = Expected {
            wait: 95.0,
            flip: Some(f64::INFINITY),
            close: 96.0,
        };
        assert_eq!(choose(&broken, 96.0, 0.0), Choice::Wait);
    }

    #[test]
    fn closing_wins_a_tie_with_flipping() {
        let e = Expected {
            wait: 90.0,
            flip: Some(96.0),
            close: 96.0,
        };
        assert_eq!(choose(&e, 96.0, 0.0), Choice::Close);
    }
}
