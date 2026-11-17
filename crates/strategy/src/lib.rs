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
    /// The pool's tick, as the contract reports it.
    pub current_tick: Tick,
    pub tick_spacing: Tick,
    /// The tick whose price the pool price is exactly, if it is one.
    ///
    /// A price on a tick boundary is the edge of the ranges on both sides
    /// of it, and the tick alone does not say which way it reads: after a
    /// swap down that stops exactly on an initialized tick the pool
    /// reports the tick below, otherwise the tick itself. A range whose
    /// bound the price sits on is wholly in one token either way.
    pub price_on_tick: Option<Tick>,
}

/// True when price has gone through the whole range away from the entry
/// side, so the position is wholly in the other token.
///
/// Decided on the price, so it agrees with what the position holds even
/// with the price exactly on the range's far bound, where the tick alone
/// can say the price is still inside.
pub fn is_beyond_range(campaign: &Campaign, market: &MarketView) -> bool {
    let past = match campaign.side {
        EntrySide::Above => market.current_tick >= campaign.tick_upper,
        EntrySide::Below => market.current_tick < campaign.tick_lower,
    };
    past || on_far_bound(campaign, market)
}

/// True when the price sits exactly on the bound the position converted
/// towards: wholly in the other token, and still right next to the price.
fn on_far_bound(campaign: &Campaign, market: &MarketView) -> bool {
    let far = match campaign.side {
        EntrySide::Above => campaign.tick_upper,
        EntrySide::Below => campaign.tick_lower,
    };
    market.price_on_tick == Some(far)
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
        if !is_beyond_range(campaign, market) {
            return PatienceAction::Wait;
        }
        match self {
            Self::Wait => PatienceAction::Wait,
            Self::Close => PatienceAction::Close,
            Self::Flip => {
                // A position the price has only just left is already where
                // a flip would put it: next to the price, in the other
                // token. Flipping it would move it nowhere, or further away.
                let target = flip_target(campaign, market);
                let same = (target.tick_lower, target.tick_upper)
                    == (campaign.tick_lower, campaign.tick_upper);
                if same || on_far_bound(campaign, market) {
                    PatienceAction::Wait
                } else {
                    PatienceAction::Flip { target }
                }
            }
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
            price_on_tick: None,
        }
    }

    /// The price exactly on the price of tick `on`, the pool reporting `tick`.
    fn on_tick(tick: Tick, on: Tick) -> MarketView {
        MarketView {
            price_on_tick: Some(on),
            ..market(tick)
        }
    }

    #[test]
    fn a_price_on_the_far_bound_is_beyond_the_range_whichever_tick_the_pool_reports() {
        // Above: the range [1000, 1200) is wholly token1 from the price of
        // tick 1200 on. After a swap down onto an initialized 1200 the pool
        // reports 1199; arriving from below it reports 1200.
        let above = campaign(EntrySide::Above);
        assert!(!is_beyond_range(&above, &market(1199)));
        assert!(is_beyond_range(&above, &on_tick(1199, 1200)));
        assert!(is_beyond_range(&above, &on_tick(1200, 1200)));
        assert!(!is_beyond_range(&above, &on_tick(1199, 1199)));

        // Below: the range is wholly token0 up to the price of tick 1000,
        // where the tick reads 1000, which on its own says inside.
        let below = campaign(EntrySide::Below);
        assert!(!is_beyond_range(&below, &market(1000)));
        assert!(is_beyond_range(&below, &on_tick(1000, 1000)));
        assert!(is_beyond_range(&below, &on_tick(999, 1000)));
        assert!(!is_beyond_range(&below, &on_tick(1001, 1001)));
    }

    #[test]
    fn on_the_far_bound_close_closes_and_flip_waits() {
        for (side, market) in [
            (EntrySide::Below, on_tick(1000, 1000)),
            (EntrySide::Above, on_tick(1199, 1200)),
        ] {
            let c = campaign(side);
            assert_eq!(FixedRule::Close.decide(&c, &market), PatienceAction::Close);
            // Already next to the price: there is nowhere nearer to flip to.
            assert_eq!(FixedRule::Flip.decide(&c, &market), PatienceAction::Wait);
            assert_eq!(FixedRule::Wait.decide(&c, &market), PatienceAction::Wait);
        }
    }

    #[test]
    fn a_flip_into_the_campaigns_own_range_is_a_wait() {
        // Within one tick spacing beyond the bound, the range next to the
        // price is the campaign's own.
        let above = campaign(EntrySide::Above);
        assert_eq!(
            FixedRule::Flip.decide(&above, &market(1203)),
            PatienceAction::Wait
        );
        let below = campaign(EntrySide::Below);
        assert_eq!(
            FixedRule::Flip.decide(&below, &market(995)),
            PatienceAction::Wait
        );
        assert!(matches!(
            FixedRule::Flip.decide(&below, &market(985)),
            PatienceAction::Flip { .. }
        ));
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
