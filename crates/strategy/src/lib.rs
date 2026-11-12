//! Strategy and patience-policy interfaces, plus the fixed patience rules.
//!
//! A strategy only declares the positions it wants. Sizing limits, transaction
//! building and safety live outside it. The three fixed rules here are the
//! baselines the adaptive patience decision in `adaptive` has to beat.

pub mod adaptive;
pub mod zone_round_trip;

use lpbot_core::config::PatienceMode;
use lpbot_core::{Campaign, EntrySide, Event, Intent, RangeTarget, Tick};

/// Read-only context a strategy gets with each event. Filled in with the
/// zone map and pool ranking in phase 1.
#[derive(Debug, Clone, Default)]
pub struct Ctx {
    pub block_number: u64,
    pub timestamp: u64,
}

/// A strategy maps market events to declarative intents.
pub trait Strategy: Send {
    fn id(&self) -> &str;
    fn on_event(&mut self, ctx: &Ctx, event: &Event) -> Vec<Intent>;
}

/// What the patience decision can see about the pool.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketView {
    pub current_tick: Tick,
    pub tick_spacing: Tick,
}

/// The three things the bot can do when price is beyond a campaign's range.
#[derive(Debug, Clone, PartialEq)]
pub enum PatienceAction {
    /// Leave the position in place; it converts back if price returns.
    Wait,
    /// Move the proceeds into a one-sided range next to the current price,
    /// on the side that buys the value token back.
    Flip { target: RangeTarget },
    /// Swap back to the value token.
    Close,
}

/// Decides what to do with a campaign whose range price has left.
pub trait PatiencePolicy: Send {
    fn decide(&self, campaign: &Campaign, market: &MarketView) -> PatienceAction;
}

/// A patience policy that always takes the same action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixedRule {
    Wait,
    Flip,
    Close,
}

impl FixedRule {
    /// The fixed rule for a configured patience mode, or `None` for `adaptive`.
    pub fn from_mode(mode: PatienceMode) -> Option<Self> {
        match mode {
            PatienceMode::Adaptive => None,
            PatienceMode::Wait => Some(Self::Wait),
            PatienceMode::Flip => Some(Self::Flip),
            PatienceMode::Close => Some(Self::Close),
        }
    }
}

impl PatiencePolicy for FixedRule {
    fn decide(&self, campaign: &Campaign, market: &MarketView) -> PatienceAction {
        if !campaign.is_beyond_range(market.current_tick) {
            return PatienceAction::Wait;
        }
        match self {
            Self::Wait => PatienceAction::Wait,
            Self::Close => PatienceAction::Close,
            Self::Flip => PatienceAction::Flip {
                target: flip_target(campaign, market),
            },
        }
    }
}

/// The range a flip moves into: same width as the campaign, touching the
/// current price on the side price would have to come back through.
///
/// A campaign entered above the price is now fully in token1, so the flip
/// range sits just below the current tick. One entered below is the mirror.
pub fn flip_target(campaign: &Campaign, market: &MarketView) -> RangeTarget {
    let spacing = market.tick_spacing.max(1);
    let width = round_up(campaign.width().max(spacing), spacing);
    match campaign.side {
        EntrySide::Above => {
            let upper = round_down(market.current_tick, spacing);
            RangeTarget {
                tick_lower: upper - width,
                tick_upper: upper,
                weight: 1.0,
            }
        }
        EntrySide::Below => {
            let lower = round_up(market.current_tick + 1, spacing);
            RangeTarget {
                tick_lower: lower,
                tick_upper: lower + width,
                weight: 1.0,
            }
        }
    }
}

fn round_down(tick: Tick, spacing: Tick) -> Tick {
    tick.div_euclid(spacing) * spacing
}

fn round_up(tick: Tick, spacing: Tick) -> Tick {
    -round_down(-tick, spacing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_core::PoolId;

    fn campaign(side: EntrySide) -> Campaign {
        Campaign {
            id: "c1".into(),
            pool: PoolId("weth-usdc-5".into()),
            value_token: "WETH".into(),
            tick_lower: 1000,
            tick_upper: 1200,
            side,
            amount_in_raw: 1,
            opened_at_block: 0,
        }
    }

    fn market(tick: Tick) -> MarketView {
        MarketView {
            current_tick: tick,
            tick_spacing: 10,
        }
    }

    #[test]
    fn nothing_happens_while_price_is_not_beyond_the_range() {
        for rule in [FixedRule::Wait, FixedRule::Flip, FixedRule::Close] {
            assert_eq!(
                rule.decide(&campaign(EntrySide::Above), &market(1100)),
                PatienceAction::Wait
            );
        }
    }

    #[test]
    fn fixed_rules_act_once_price_is_beyond_the_range() {
        let c = campaign(EntrySide::Above);
        let m = market(1307);
        assert_eq!(FixedRule::Wait.decide(&c, &m), PatienceAction::Wait);
        assert_eq!(FixedRule::Close.decide(&c, &m), PatienceAction::Close);
        assert!(matches!(
            FixedRule::Flip.decide(&c, &m),
            PatienceAction::Flip { .. }
        ));
    }

    #[test]
    fn a_flip_after_an_upward_run_sits_just_below_the_price() {
        let t = flip_target(&campaign(EntrySide::Above), &market(1307));
        assert_eq!((t.tick_lower, t.tick_upper), (1100, 1300));
        assert!(t.tick_upper <= 1307);
    }

    #[test]
    fn a_flip_after_a_downward_run_sits_just_above_the_price() {
        let t = flip_target(&campaign(EntrySide::Below), &market(893));
        assert_eq!((t.tick_lower, t.tick_upper), (900, 1100));
        assert!(t.tick_lower > 893);
    }

    #[test]
    fn flip_ranges_are_aligned_for_negative_ticks_too() {
        let t = flip_target(&campaign(EntrySide::Above), &market(-7));
        assert_eq!((t.tick_lower, t.tick_upper), (-210, -10));
        let t = flip_target(&campaign(EntrySide::Below), &market(-7));
        assert_eq!((t.tick_lower, t.tick_upper), (0, 200));
    }

    #[test]
    fn adaptive_mode_has_no_fixed_rule() {
        assert_eq!(FixedRule::from_mode(PatienceMode::Adaptive), None);
        assert_eq!(
            FixedRule::from_mode(PatienceMode::Flip),
            Some(FixedRule::Flip)
        );
    }
}
