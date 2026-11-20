//! The replay engine: one pool, one strategy, one run.
//!
//! It replays the pool's stored events through the pool replica and holds
//! the bot's position beside it as a virtual one. The position is assumed
//! small against the liquidity in range (the passive fill model): the
//! historical swaps are left unchanged and the position takes its share of
//! each swap step's fee and each flash loan's fee, with its own liquidity
//! counted in beside the pool's. Entries are capped at a share of the
//! liquidity in their range, which keeps the assumption honest. Token
//! amounts are exact integers throughout; floating point is used only for
//! decisions and reporting.
//!
//! Decisions are taken at the end of a block and carried out at a later
//! block, on the state then, so nothing is ever decided and filled on the
//! same information.

use std::collections::HashMap;
use std::sync::Arc;

use crate::agent::{
    pct_to_ticks, ticks_to_pct, AgentAction, AgentDecision, AgentPlan, Observation, PositionView,
    ScaleView, SCALES,
};
use alloy_primitives::{I256, U256};
use lpbot_core::{Campaign, EntrySide, PoolId, Tick};
use lpbot_data::gas::BlockFees;
use lpbot_data::lake::TimedEvent;
use lpbot_data::odds::TickBuckets;
use lpbot_data::zone_map::{zone_stats, PathPoint, DAY};
use lpbot_risk::{parts_per_million, LiquidityShareCap, LossCheck, LossLimit, MILLION};
use lpbot_strategy::adaptive::{self, AdaptiveParams, Choice, Expected, Gamble, Options};
use lpbot_strategy::zone_round_trip::{
    around_price, choose_zone, entry_candidates, gap, ZoneScore,
};
use lpbot_strategy::{FixedRule, MarketView, PatienceAction, PatiencePolicy};
use lpbot_venues::uniswap_v3::events::PoolEvent;
use lpbot_venues::uniswap_v3::full_math::mul_div;
use lpbot_venues::uniswap_v3::liquidity_amounts::{
    get_amounts_for_liquidity, get_liquidity_for_amount0, get_liquidity_for_amount1,
    get_liquidity_for_amounts,
};
use lpbot_venues::uniswap_v3::pool::{
    fees_earned, Pool, PoolError, PoolSnapshot, Replay, StepInfo,
};
use lpbot_venues::uniswap_v3::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use lpbot_venues::uniswap_v3::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MIN_SQRT_RATIO,
};
use lpbot_venues::uniswap_v3::{MathError, Q128, Q96};

/// One of a pool's two tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Zero,
    One,
}

impl Token {
    pub fn other(self) -> Self {
        match self {
            Token::Zero => Token::One,
            Token::One => Token::Zero,
        }
    }
}

/// What each kind of action costs in gas, charged in the value token.
///
/// Each action pays the gas price of the block it lands in when the run has
/// the blocks' prices (`Settings::block_fees`), else `gas_price_gwei`. The
/// ETH it costs is turned into the value token at the pool's price at that
/// moment when the pool holds WETH, else at `value_raw_per_eth`. The run
/// turns the decimals into integers once, at its start (`GasPrice`); every
/// gas amount charged after that is integer math.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GasModel {
    pub open_units: u64,
    pub close_units: u64,
    pub swap_units: u64,
    /// The price of every block when the blocks' own are not given.
    pub gas_price_gwei: f64,
    /// The priority fee paid on top of a block's base fee. `None` pays the
    /// median priority fee of the block's transactions.
    pub tip_gwei: Option<f64>,
    /// Which of the pool's tokens is WETH, if one is.
    pub weth: Option<Token>,
    /// Smallest units of the value token that one ETH is worth, for a pool
    /// without WETH.
    pub value_raw_per_eth: f64,
}

impl GasModel {
    /// No gas cost at all.
    pub fn free() -> Self {
        Self {
            open_units: 0,
            close_units: 0,
            swap_units: 0,
            gas_price_gwei: 0.0,
            tip_gwei: None,
            weth: None,
            value_raw_per_eth: 0.0,
        }
    }
}

const WEI_PER_GWEI: f64 = 1e9;
const WEI_PER_ETH: u128 = 1_000_000_000_000_000_000;

/// The gas model's prices as integers: the boundary between the decimals
/// of the config and price signal, and the exact amounts charged.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GasPrice {
    /// Wei per gas when the blocks' own prices are not given.
    fixed_wei_per_gas: U256,
    /// Wei per gas paid on top of the base fee; `None`: the block's median.
    tip_wei: Option<U256>,
    eth: EthRate,
}

/// How the ETH that gas costs becomes the value token.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EthRate {
    /// The value token is WETH.
    Value,
    /// The other token is WETH: at the pool's price at the time.
    Other,
    /// Smallest units of the value token per ETH, fixed for the run.
    Fixed(U256),
}

impl GasPrice {
    fn new(model: &GasModel, value: Token) -> Result<Self, SimError> {
        let fixed_wei_per_gas = whole(model.gas_price_gwei * WEI_PER_GWEI).ok_or_else(|| {
            SimError::Settings(format!(
                "the gas price of {} gwei is not a price; set costs.gas_price_gwei to a number \
                 of at least 0",
                model.gas_price_gwei
            ))
        })?;
        let tip_wei = match model.tip_gwei {
            None => None,
            Some(tip) => Some(whole(tip * WEI_PER_GWEI).ok_or_else(|| {
                SimError::Settings(format!(
                    "the priority fee of {tip} gwei is not a price; set costs.priority_fee_gwei \
                     to a number of at least 0, or to `median`"
                ))
            })?),
        };
        let eth = match model.weth {
            Some(token) if token == value => EthRate::Value,
            Some(_) => EthRate::Other,
            None => EthRate::Fixed(whole(model.value_raw_per_eth).ok_or_else(|| {
                SimError::Settings(format!(
                    "one ETH is said to be worth {} of the value token's smallest unit, which \
                     cannot price gas; check costs.eth_price_usd and the pool's price",
                    model.value_raw_per_eth
                ))
            })?),
        };
        Ok(Self {
            fixed_wei_per_gas,
            tip_wei,
            eth,
        })
    }
}

/// A non-negative decimal rounded to the nearest whole number. `None` for
/// anything negative, not a number, or too large for 128 bits.
fn whole(value: f64) -> Option<U256> {
    (value.is_finite() && value >= 0.0 && value < u128::MAX as f64)
        .then(|| U256::from(value.round() as u128))
}

/// How the run decides when price has left a position behind.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Policy {
    /// Always the same action: the baselines.
    Fixed(FixedRule),
    /// Compare waiting, flipping and closing each time.
    Adaptive(AdaptiveParams),
}

/// Seconds per step of the price history the signals count on, unless the
/// strategy sets `signal_step`.
pub const DEFAULT_SIGNAL_STEP: u64 = 600;

/// Which token of the pool is a dollar stable, for results in USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dollar {
    /// The value token is: a USD figure is the value-token figure.
    Value,
    /// The other token is: a USD figure is the value-token figure times
    /// the price of the value token in the other token.
    Other,
}

/// How a campaign goes in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// All of the value token in a range on the side price must travel
    /// into: no swap, nothing earned until price arrives.
    OneSided,
    /// Part of the value token swapped into the other token, and both in
    /// the range centred on the price: earns at once, holds both tokens.
    AroundPrice,
}

impl Shape {
    /// The name used in the config and the result tables.
    pub fn name(self) -> &'static str {
        match self {
            Shape::OneSided => "one_sided",
            Shape::AroundPrice => "around_price",
        }
    }
}

/// One of a followed provider's positions on the run's pool: its range,
/// and when the provider put liquidity in and took the last of it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FollowSpan {
    pub open_time: u64,
    /// `None` while the provider's position is still open.
    pub close_time: Option<u64>,
    pub lower: Tick,
    pub upper: Tick,
}

/// Which way price must have moved for the entry signal, as the dollar
/// price of the pool's volatile token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveDirection {
    Any,
    Up,
    Down,
}

/// Everything that defines one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The token results are counted in.
    pub value: Token,
    /// How campaigns go in.
    pub shape: Shape,
    /// Which token counts as the dollar, for the results in USD.
    pub dollar: Dollar,
    pub decimals0: u8,
    pub decimals1: u8,
    /// Width of the ranges, in ticks. A multiple of the tick spacing.
    pub width: Tick,
    /// How far from the price an entry range may sit, in ticks.
    pub max_gap: Tick,
    /// Capital of the run in the value token, smallest unit.
    pub capital: U256,
    /// Share of the free capital put into a campaign, between 0 and 1.
    /// Larger values are taken as 1.
    pub capital_share: f64,
    /// Largest share of the liquidity in its range a position may have at
    /// entry (`universe.max_share_of_in_range_liquidity`), above 0 and at
    /// most 1. What the capital would buy beyond it stays as cash. `None`
    /// leaves entries uncapped.
    pub max_share_of_in_range_liquidity: Option<f64>,
    pub loss_limit: LossLimit,
    pub policy: Policy,
    /// Seconds between patience decisions.
    pub decide_every: u64,
    /// Seconds between looks at where to enter or whether to move.
    pub check_every: u64,
    /// Blocks between a decision and its execution. At least one.
    pub latency_blocks: u64,
    /// How far back the zone figures look, in seconds: the shortest and the
    /// longest are used. Each is measured again every twelfth of its
    /// length, at least every minute and at most once a day.
    pub lookbacks: Vec<u64>,
    /// Length of the price steps the volatility, move and odds signals
    /// count on, in seconds (`signal_step`).
    pub signal_step: u64,
    /// How far back the volatility signals look, in seconds
    /// (`entry.vol_window`). The volatility is per day whatever the window.
    pub vol_window: u64,
    /// A resting position moves only to a range crossed this many times
    /// as often as its own.
    pub move_margin: f64,
    /// Full passes per day a range needs on the longest lookback to be
    /// entered. 0 lets any range in that price crossed lately qualify.
    pub min_passes_per_day: f64,
    /// Entry gate: the day's realized volatility, in ticks, at most this
    /// many times the range width. `None`: no gate.
    pub max_vol_to_width: Option<f64>,
    /// Entry gate: the day's realized volatility at most this many ticks.
    /// `None`: no gate.
    pub max_daily_vol_ticks: Option<f64>,
    /// An entry must put in at least this many times the gas of opening,
    /// closing and one swap, or it is not made. `None`: any size.
    pub min_gas_multiple: Option<f64>,
    /// A position wholly in the other token for this many seconds is
    /// closed, whatever the patience rule says. `None`: no limit.
    pub max_seconds_beyond: Option<u64>,
    /// A resting position steps aside, back to cash, when the day's
    /// volatility exceeds this many times the range width. `None`: never.
    pub exit_vol_to_width: Option<f64>,
    /// An around-price position wholly in one token this many seconds is
    /// moved to the range around the price, swapping only what the new
    /// range needs. 0 moves it as soon as price leaves. `None`: never; the
    /// one-sided rules apply instead. Only for `Shape::AroundPrice`.
    pub recenter_after: Option<u64>,
    /// A campaign open this long is closed, back to the value token.
    /// `None`: no limit.
    pub max_hold: Option<u64>,
    /// A campaign whose value has fallen this many percent below what
    /// went in, in the value token at the pool price, is closed back to
    /// the value token. `None`: no stop.
    pub max_loss_pct: Option<f64>,
    /// A campaign whose value has fallen by this many percent of what went
    /// in from its best since entry is closed. `None`: no stop.
    pub max_drawdown_pct: Option<f64>,
    /// Once price has been this many ticks past the range on the other
    /// token's side, the campaign is closed as soon as price is back in
    /// the range. `None`: never.
    pub exit_on_return_ticks: Option<Tick>,
    /// Close a campaign once it has made this many round trips: back wholly
    /// in the value token after being wholly in the other one, its fees on
    /// top (`exit.after_round_trips`). `None` lets it rest for more.
    pub after_round_trips: Option<u32>,
    /// After a campaign ends, no new one starts for this many seconds
    /// (`entry.cooldown`): one entry per move, not one per look while the
    /// move signal still holds. `None`: at the next look.
    pub cooldown: Option<u64>,
    /// A resting range price has moved away from, or a much busier one
    /// nearby, is moved to the range now chosen (`entry.reposition`, the
    /// default). Off, it stays where it was placed until an exit takes it
    /// out.
    pub reposition: bool,
    /// Entry signal: a campaign starts only once price has moved at least
    /// this many ticks over the last `move_window` seconds, in
    /// `move_direction`. `None`: no signal.
    pub after_move_ticks: Option<Tick>,
    pub move_window: u64,
    pub move_direction: MoveDirection,
    /// A new range is this many times the day's volatility wide, in
    /// ticks, rounded up to the tick spacing; `width` while there is not
    /// enough history to measure it. `None`: always `width`.
    pub width_vol_multiple: Option<f64>,
    /// A provider's positions to follow instead of the run's own entries,
    /// sorted by when they opened: each is entered in the provider's range,
    /// swapping into its mix, when the provider opens it, and closed when
    /// the provider closes it. The patience rule and moves do not apply;
    /// the campaign exits still do. `None`: the run's own strategy.
    pub follow: Option<Vec<FollowSpan>>,
    /// A followed position is joined only this many seconds after the
    /// provider opened it, or not at all: what is copied is the entry.
    pub follow_join_within: u64,
    /// The agent mode: entries and exits come from these decisions, one
    /// per look, and the run stops at the first look without one, leaving
    /// its observation in `Output::pending`. The patience rule, moves and
    /// the entry signals do not apply; the campaign exits and the loss
    /// limit do.
    pub agent: Option<AgentPlan>,
    pub gas: GasModel,
    /// Each block's base fee and median priority fee. `None` charges
    /// `gas.gas_price_gwei` on every block. When given it must cover every
    /// block from the start of trading to the end of the history.
    pub block_fees: Option<Arc<BlockFees>>,
    /// Seconds between rows of the series.
    pub series_every: u64,
    /// Trading starts here. Earlier events only build up history.
    pub trade_from: u64,
    /// The run ends here and whatever is open is closed.
    pub trade_to: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("the pool replica refused an event at block {block}, log {log_index}: {source}")]
    Replay {
        block: u64,
        log_index: u32,
        source: PoolError,
    },
    #[error("pool math failed while handling the position: {0}")]
    Math(#[from] MathError),
    #[error("a simulated swap failed: {0}")]
    Swap(#[from] PoolError),
    #[error("{0}")]
    Settings(String),
}

/// One campaign: an entry, what it earned and how it ended. Amounts are in
/// whole value tokens.
#[derive(Debug, Clone, PartialEq)]
pub struct CampaignRow {
    pub id: u32,
    pub tick_lower: Tick,
    pub tick_upper: Tick,
    pub opened_time: u64,
    pub opened_block: u64,
    pub closed_time: u64,
    pub closed_block: u64,
    /// `round_trip_complete`, `repositioned`, `patience_close`,
    /// `patience_timeout` (stranded longer than the longest wait),
    /// `stepped_aside` (a resting position taken out because the day turned
    /// too volatile for its range), `flip_failed` (a flip whose new range
    /// could not open, closed instead), `recenter_failed` (likewise for a
    /// move to the range around the price), `max_hold` (open as long as
    /// allowed), `stop_loss` (fell the most allowed below what went in),
    /// `trailing_stop` (fell the most allowed from its best),
    /// `return_exit` (closed as price came back after a deep excursion),
    /// `round_trip_exit` (closed once back in the value token after the
    /// round trips allowed),
    /// `loss_limit` or `window_end`.
    pub exit_reason: String,
    /// `one_sided` or `around_price`: how the campaign went in.
    pub shape: String,
    pub amount_in: f64,
    /// Capital after the campaign minus capital before it: fees, losses,
    /// gas and swap costs all included.
    pub result: f64,
    pub fees: f64,
    pub gas: f64,
    /// Full passes of price through the campaign's ranges.
    pub passes: u32,
    pub flips: u32,
    /// Moves to the range around the price (`Settings::recenter_after`).
    pub recentres: u32,
    pub seconds_beyond: u64,
    /// Seconds price spent inside the campaign's ranges, block by block.
    pub seconds_in_range: u64,
}

/// One patience decision.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRow {
    pub campaign: u32,
    pub time: u64,
    pub block: u64,
    pub tick: Tick,
    /// `entry` or `flip`: which leg of the campaign was stranded; `agent`
    /// for a decision of the agent mode.
    pub leg: String,
    pub seconds_stranded: u64,
    /// `wait`, `flip`, `close` or `recenter`. What came of it is in the
    /// campaign's row: a flip whose new range could not open ends it as
    /// `flip_failed`, a move to the range around the price as
    /// `recenter_failed`.
    pub action: String,
    /// For a flip or a move, the range it went to: the one next to or
    /// around the price when it was carried out, which a move during the
    /// latency can make differ from the one when it was decided.
    pub target_lower: Option<Tick>,
    pub target_upper: Option<Tick>,
    /// What the adaptive decision expected from each option, in whole
    /// value tokens. Empty for a fixed rule, and for flipping when there
    /// was nowhere nearer to move to.
    pub expected_wait: Option<f64>,
    pub expected_flip: Option<f64>,
    pub expected_close: Option<f64>,
}

/// One row of the run's time series.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesRow {
    pub time: u64,
    pub block: u64,
    pub tick: Tick,
    /// Price of the value token in the other token.
    pub price: f64,
    /// Capital plus position, marked at the pool price, in value tokens.
    pub equity: f64,
    /// What simply holding the value token would be worth: the capital.
    pub hold: f64,
    /// `equity` in dollars at the pool price.
    pub equity_usd: f64,
    /// `hold` in dollars at the pool price: holding the value token.
    pub hold_usd: f64,
    pub fees: f64,
    /// `flat`, `resting`, `in_range`, `beyond` or `flipped`.
    pub state: String,
    pub tick_lower: Option<Tick>,
    pub tick_upper: Option<Tick>,
    /// The range's bounds as prices of the value token, low then high.
    pub price_lower: Option<f64>,
    pub price_upper: Option<f64>,
}

/// What the zone map said about a range when the engine looked at it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneLook {
    pub time: u64,
    /// The lookback in days; under a day it is a fraction.
    pub lookback_days: f64,
    pub tick_lower: Tick,
    pub tick_upper: Tick,
    pub observed_days: f64,
    pub passes_per_day: f64,
    pub time_in_range: f64,
    pub return_odds_1d: Option<f64>,
}

/// Headline results of a run, in whole value tokens.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Summary {
    /// In the value token.
    pub start_capital: f64,
    pub end_capital: f64,
    pub result_pct: f64,
    /// Price of the value token in the other token when trading starts and
    /// when the run ends.
    pub start_price: f64,
    pub end_price: f64,
    /// The capital in dollars at those prices, and the result in dollars.
    pub start_usd: f64,
    pub end_usd: f64,
    pub result_pct_usd: f64,
    /// What simply holding the value token returned in dollars over the
    /// same time: the price change for ETH, nothing for a dollar stable.
    pub hold_value_pct_usd: f64,
    pub fees: f64,
    pub gas: f64,
    /// The average gas price the run's actions paid, in gwei.
    pub gas_gwei: f64,
    /// Seconds of the traded window: with price inside a campaign's
    /// range, in a campaign with price outside it, and with no campaign
    /// open. Counted block by block.
    pub seconds_in_range: u64,
    pub seconds_out_of_range: u64,
    pub seconds_flat: u64,
    pub campaigns: u32,
    pub passes: u32,
    pub max_drawdown_pct: f64,
    pub halted_by_loss_limit: bool,
    pub events: u64,
    /// Entries made smaller by the cap on the position's share of the
    /// liquidity in its range.
    pub capped_entries: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Output {
    pub campaigns: Vec<CampaignRow>,
    pub decisions: Vec<DecisionRow>,
    pub series: Vec<SeriesRow>,
    pub zones: Vec<ZoneLook>,
    pub summary: Summary,
    /// In the agent mode, the look the run stopped at for want of a
    /// decision, and how the pool looked then.
    pub pending: Option<Observation>,
    /// In the agent mode, decisions left over that no look asked for.
    pub unused_decisions: usize,
}

/// What may hold a new leg back: the cap on its share of the liquidity in
/// its range, and the least it must put in.
#[derive(Debug, Clone, Copy)]
struct LegLimits {
    cap: Option<LiquidityShareCap>,
    min_used: U256,
}

impl LegLimits {
    const NONE: LegLimits = LegLimits {
        cap: None,
        min_used: U256::ZERO,
    };
}

/// A virtual position.
#[derive(Debug, Clone)]
struct Leg {
    /// False for the entry leg, which starts in the value token. True for
    /// a flip leg, which starts in the other token.
    flip: bool,
    lower: Tick,
    upper: Tick,
    sqrt_lower: U256,
    sqrt_upper: U256,
    liquidity: u128,
    /// Fee growth inside the range per unit of liquidity since the leg
    /// opened, in Q128, with the leg's own liquidity counted in.
    growth0: U256,
    growth1: U256,
}

impl Leg {
    /// Fees the leg has earned in each token: its liquidity times its fee
    /// growth, rounded down, as the pool pays them.
    fn fees(&self) -> (U256, U256) {
        (
            fees_earned(self.growth0, U256::ZERO, self.liquidity),
            fees_earned(self.growth1, U256::ZERO, self.liquidity),
        )
    }

    fn is_active_at(&self, tick: Tick) -> bool {
        self.lower <= tick && tick < self.upper
    }
}

/// Fee growth per unit of liquidity from `fee` paid to LPs with `pool`
/// liquidity active, once a position with `position` liquidity is counted
/// in beside it, in Q128.
///
/// The passive fill model leaves the historical swaps as they were, so the
/// replica's active liquidity never includes the virtual position. Dividing
/// by the pool's liquidity alone would pay a position larger than the pool
/// more than the fee there was; counting it in keeps its share below the
/// whole fee however large it is. Where the pool had no liquidity at all
/// nothing was earned in history, and the passive model cannot say what
/// the position alone would have traded, so it earns nothing.
pub(crate) fn diluted_fee_growth(fee: U256, pool: u128, position: u128) -> U256 {
    if pool == 0 || fee.is_zero() {
        return U256::ZERO;
    }
    // The divisor is at least one, and the quotient is below the fee times
    // 2^128, so this cannot fail.
    mul_div(fee, Q128, U256::from(pool) + U256::from(position)).unwrap_or(U256::ZERO)
}

/// The LPs' part of a fee once the protocol has taken its cut, as the pool
/// takes it: one `fee_protocol`-th, rounded down, or nothing when off.
pub(crate) fn after_protocol(fee: U256, fee_protocol: u8) -> U256 {
    if fee_protocol == 0 {
        fee
    } else {
        fee - fee / U256::from(fee_protocol)
    }
}

/// The least active liquidity the pool has anywhere in `[lower, upper)`,
/// read from the replica's tick map.
///
/// This is what "the liquidity in range" means for a range the price has
/// not reached yet, as an entry range never contains the price: the
/// position's share of each swap's fee, and the error of the passive fill
/// model, are largest where the pool is thinnest. A range with a gap in
/// the pool's liquidity reads zero.
pub(crate) fn thinnest_liquidity(pool: &Pool, lower: Tick, upper: Tick) -> u128 {
    let current = pool.tick();
    // The pool's liquidity holds for the ticks between the initialized
    // ticks around the current one. Crossing a tick upwards adds its net
    // liquidity, crossing it downwards takes it away.
    let net_between = |from: Tick, to: Tick| {
        pool.ticks()
            .filter(|(t, _)| *t > from && *t <= to)
            .fold(0i128, |sum, (_, info)| {
                sum.saturating_add(info.liquidity_net)
            })
    };
    let mut liquidity = pool.liquidity();
    liquidity = if lower > current {
        liquidity.saturating_add_signed(net_between(current, lower))
    } else {
        liquidity.saturating_add_signed(net_between(lower, current).saturating_neg())
    };
    // `liquidity` is now what is active at `lower`; walk up the range.
    let mut thinnest = liquidity;
    for (_, info) in pool.ticks().filter(|(t, _)| *t > lower && *t < upper) {
        liquidity = liquidity.saturating_add_signed(info.liquidity_net);
        thinnest = thinnest.min(liquidity);
    }
    thinnest
}

/// `amount` times a share in parts per million, rounded down.
pub(crate) fn share_of(amount: U256, ppm: u32) -> U256 {
    // `ppm` is at most a million, so the result never exceeds `amount`.
    mul_div(amount, U256::from(ppm), U256::from(MILLION)).unwrap_or(amount)
}

/// What a leg that opened put in.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Opened {
    used: U256,
    /// The share cap made it smaller than the amount would have bought.
    capped: bool,
}

/// What a leg around the price took, and what it left.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mixed {
    /// Value token and other token not in the range: what the range could
    /// not take after the swap, and what the share cap held back.
    value_left: U256,
    other_left: U256,
    /// A swap was made to get the range's mix of the two tokens.
    swapped: bool,
    /// The share cap made it smaller than the amounts would have bought.
    capped: bool,
}

/// The last side of its range price left a campaign's position on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extreme {
    /// Wholly in the value token.
    Value,
    /// Wholly in the other token.
    Other,
    /// Neither yet: a range around the price, which starts in the middle.
    Neither,
}

#[derive(Debug, Clone)]
struct OpenCampaign {
    id: u32,
    tick_lower: Tick,
    tick_upper: Tick,
    opened_time: u64,
    opened_block: u64,
    cash_before: U256,
    amount_in: U256,
    fees: f64,
    gas: U256,
    passes: u32,
    flips: u32,
    recentres: u32,
    seconds_beyond: u64,
    extreme: Extreme,
    /// Since when the position has been wholly in the other token.
    other_since: Option<u64>,
    /// Since when the position has been wholly in one token, either one.
    out_since: Option<u64>,
    /// The run's highest equity since the campaign opened, in value-token
    /// units, for the drawdown stop.
    best: f64,
    /// Price has been `exit_on_return_ticks` or more past the range.
    far_beyond: bool,
    /// Since when price has been inside the range, and the seconds it
    /// spent there before.
    in_range_since: Option<u64>,
    seconds_in_range: u64,
    /// Times the position came back wholly into the value token after
    /// being wholly in the other one.
    round_trips: u32,
}

impl OpenCampaign {
    /// Price is not inside the range from `time` on.
    fn left_range(&mut self, time: u64) {
        if let Some(since) = self.in_range_since.take() {
            self.seconds_in_range += time.saturating_sub(since);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Open {
        lower: Tick,
        upper: Tick,
        /// The followed provider's position this enters, by index.
        follow: Option<usize>,
    },
    /// Withdraw and swap everything back to the value token.
    Close { reason: &'static str },
    /// Withdraw and place the other token next to the price. `decision` is
    /// the row of the decision it carries out.
    Flip { decision: usize },
    /// Withdraw a position that is wholly in the value token.
    Withdraw { reason: &'static str },
    /// Withdraw a position that price has left and put it in the range
    /// around the price, swapping only what that range needs. `decision`
    /// is the row of the decision it carries out.
    Recenter { decision: usize },
}

/// One range's figures over one lookback, and when they were measured.
#[derive(Debug, Clone, Copy)]
struct Look {
    period: u64,
    logged_hour: u64,
    passes_per_day: f64,
    observed_seconds: f64,
}

pub(crate) fn to_f64(value: U256) -> f64 {
    value.to_string().parse().unwrap_or(f64::INFINITY)
}

struct Engine<'a> {
    s: &'a Settings,
    pool: Pool,
    path: Vec<PathPoint>,
    cash: U256,
    leg: Option<Leg>,
    campaign: Option<OpenCampaign>,
    pending: Option<(u64, Action)>,
    next_check: u64,
    next_decide: u64,
    next_series: u64,
    halted: bool,
    /// The latest look at each range over each lookback.
    scores: HashMap<(Tick, Tick, u64), Look>,
    buckets: TickBuckets,
    out: Output,
    campaigns_opened: u32,
    fees_total: f64,
    gas: GasPrice,
    gas_total: U256,
    /// Gas the free capital could not cover yet, in the value token.
    gas_owed: U256,
    /// Gas units charged and what they cost in wei, for the average price.
    gas_units: f64,
    gas_wei: f64,
    /// The block being handled: the one an action lands in.
    block: u64,
    /// The capital share in parts per million.
    capital_ppm: u32,
    share_cap: Option<LiquidityShareCap>,
    /// The other token held outside any position: what a flip's range
    /// could not take. It stays as it would in the Safe, and goes back to
    /// the value token with the campaign's closing swap.
    loose_other: U256,
    /// The followed position the open campaign copies, and the first one
    /// not yet looked at.
    following: Option<usize>,
    follow_next: usize,
    peak: f64,
    value_unit: f64,
    /// Price of the value token in the other token on the first block of
    /// trading.
    start_price: Option<f64>,
    /// When the last campaign ended, for the cooldown.
    last_closed: Option<u64>,
    /// The agent mode: when the next look is due, how many decisions are
    /// used, the range to enter once the position being moved is closed,
    /// and whether the run stopped at a look without a decision.
    agent_next: u64,
    agent_used: usize,
    agent_reenter: Option<(Tick, Tick)>,
    stopped: bool,
}

impl<'a> Engine<'a> {
    /// Dollars one whole value token is worth when it trades at `price` in
    /// the other token.
    fn usd_per_value(&self, price: f64) -> f64 {
        match self.s.dollar {
            Dollar::Value => 1.0,
            Dollar::Other => price,
        }
    }

    /// Realized volatility in ticks per day over the volatility window,
    /// from the price seen so far.
    fn daily_vol(&self, now: u64) -> Option<f64> {
        self.buckets.daily_vol_ticks(now, self.s.vol_window)
    }

    /// The width of a range opened now: the run's, or the day's
    /// volatility times `width_vol_multiple`, rounded up to the tick
    /// spacing, once there is a day's history to measure it on.
    fn width_now(&self, now: u64) -> Tick {
        let Some(multiple) = self.s.width_vol_multiple else {
            return self.s.width;
        };
        let spacing = self.pool.tick_spacing().max(1);
        match self.daily_vol(now) {
            Some(vol) if vol.is_finite() && vol > 0.0 => {
                // Ranges wider than half the tick range make no sense.
                let ticks = (multiple * vol).ceil().clamp(1.0, 400_000.0) as Tick;
                ((ticks + spacing - 1) / spacing) * spacing
            }
            _ => self.s.width,
        }
    }

    /// Whether price has moved far enough lately for the entry signal.
    /// Without a signal, always; without the history to measure, never.
    fn moved_enough(&self, now: u64) -> bool {
        let Some(needed) = self.s.after_move_ticks else {
            return true;
        };
        let Some(moved) = self.buckets.move_ticks(now, self.s.move_window) else {
            return false;
        };
        // The volatile token's dollar price rises with the tick when the
        // dollar is token1, and falls with it when the dollar is token0.
        let dollar = match self.s.dollar {
            Dollar::Value => self.s.value,
            Dollar::Other => self.s.value.other(),
        };
        let up = if dollar == Token::One { moved } else { -moved };
        match self.s.move_direction {
            MoveDirection::Any => up.abs() >= needed,
            MoveDirection::Up => up >= needed,
            MoveDirection::Down => -up >= needed,
        }
    }

    /// Whether the volatility gates let a campaign start now. A gate with
    /// no history to measure keeps the bot out.
    fn calm_enough(&self, now: u64) -> bool {
        if self.s.max_vol_to_width.is_none() && self.s.max_daily_vol_ticks.is_none() {
            return true;
        }
        let Some(vol) = self.daily_vol(now) else {
            return false;
        };
        let width = f64::from(self.width_now(now));
        self.s.max_vol_to_width.is_none_or(|r| vol <= r * width)
            && self.s.max_daily_vol_ticks.is_none_or(|m| vol <= m)
    }

    fn side(&self) -> EntrySide {
        // Token0 alone sits above the price, token1 alone below it.
        match self.s.value {
            Token::Zero => EntrySide::Above,
            Token::One => EntrySide::Below,
        }
    }

    /// Smallest units of the value token one smallest unit of the other
    /// token is worth at the pool price.
    fn value_per_other(&self) -> f64 {
        let sqrt = to_f64(self.pool.sqrt_price_x96()) / 2f64.powi(96);
        let token1_per_token0 = sqrt * sqrt;
        match self.s.value {
            Token::One => token1_per_token0,
            Token::Zero => 1.0 / token1_per_token0,
        }
    }

    /// Price of the value token in the other token, in whole tokens.
    fn price(&self) -> f64 {
        let scale = 10f64.powi(i32::from(self.s.decimals0) - i32::from(self.s.decimals1));
        let sqrt = to_f64(self.pool.sqrt_price_x96()) / 2f64.powi(96);
        let token1_per_token0 = sqrt * sqrt * scale;
        match self.s.value {
            Token::One => 1.0 / token1_per_token0,
            Token::Zero => token1_per_token0,
        }
    }

    /// Price of the value token in the other token at a tick.
    fn price_at(&self, tick: Tick) -> f64 {
        let scale = 10f64.powi(i32::from(self.s.decimals0) - i32::from(self.s.decimals1));
        let token1_per_token0 = 1.0001f64.powi(tick) * scale;
        match self.s.value {
            Token::One => 1.0 / token1_per_token0,
            Token::Zero => token1_per_token0,
        }
    }

    /// (token0, token1) as (value token, other token). The same swap turns
    /// (value token, other token) back into (token0, token1).
    fn split(&self, amount0: U256, amount1: U256) -> (U256, U256) {
        match self.s.value {
            Token::Zero => (amount0, amount1),
            Token::One => (amount1, amount0),
        }
    }

    /// `amount` of the other token in value-token units at the pool price,
    /// rounded down. Token1 is worth 2^192 / sqrtPrice² of token0.
    fn other_in_value(&self, amount: U256) -> Result<U256, MathError> {
        let sqrt = self.pool.sqrt_price_x96();
        match self.s.value {
            Token::Zero => mul_div(mul_div(amount, Q96, sqrt)?, Q96, sqrt),
            Token::One => mul_div(mul_div(amount, sqrt, Q96)?, sqrt, Q96),
        }
    }

    /// `amount` of the value token in units of the other token at the pool
    /// price, rounded down.
    fn value_in_other(&self, amount: U256) -> Result<U256, MathError> {
        let sqrt = self.pool.sqrt_price_x96();
        match self.s.value {
            Token::Zero => mul_div(mul_div(amount, sqrt, Q96)?, sqrt, Q96),
            Token::One => mul_div(mul_div(amount, Q96, sqrt)?, Q96, sqrt),
        }
    }

    /// What the leg holds now, fees left out: (value token, other token).
    fn holdings(&self, leg: &Leg) -> Result<(U256, U256), MathError> {
        let (amount0, amount1) = get_amounts_for_liquidity(
            self.pool.sqrt_price_x96(),
            leg.sqrt_lower,
            leg.sqrt_upper,
            leg.liquidity,
        )?;
        Ok(self.split(amount0, amount1))
    }

    /// The leg's fees: (value token, other token).
    fn leg_fees(&self, leg: &Leg) -> (U256, U256) {
        let (fee0, fee1) = leg.fees();
        self.split(fee0, fee1)
    }

    /// Capital plus position at the pool price, less the gas still owed, in
    /// value-token units. The other token is marked after the pool's swap
    /// fee.
    fn equity(&self) -> Result<f64, MathError> {
        let mut value = self.cash;
        let mut other = self.loose_other;
        if let Some(leg) = &self.leg {
            let (held_value, held_other) = self.holdings(leg)?;
            let (fee_value, fee_other) = self.leg_fees(leg);
            value += held_value + fee_value;
            other += held_other + fee_other;
        }
        let keep = 1.0 - f64::from(self.pool.fee_pips()) / f64::from(MILLION);
        Ok(to_f64(value) + to_f64(other) * self.value_per_other() * keep - to_f64(self.gas_owed))
    }

    /// Wei per gas in the block being handled.
    fn wei_per_gas(&self) -> U256 {
        match self.s.block_fees.as_deref().and_then(|f| f.at(self.block)) {
            Some((base, tip)) => U256::from(base) + self.gas.tip_wei.unwrap_or(U256::from(tip)),
            None => self.gas.fixed_wei_per_gas,
        }
    }

    /// What `units` of gas cost in the value token's smallest unit in the
    /// block being handled, rounded down.
    fn gas_cost(&self, units: u64) -> U256 {
        let wei = U256::from(units) * self.wei_per_gas();
        // Only fails on a cost beyond 2^256, which no real price makes.
        match self.gas.eth {
            EthRate::Value => wei,
            EthRate::Other => self.other_in_value(wei).unwrap_or(U256::MAX),
            EthRate::Fixed(rate) => {
                mul_div(wei, rate, U256::from(WEI_PER_ETH)).unwrap_or(U256::MAX)
            }
        }
    }

    /// Takes an action's gas out of the free capital. When the free capital
    /// cannot cover it, as after a flip that put everything back into the
    /// pool, it is owed and settled with the next charge; until then the
    /// equity counts it as spent.
    fn charge_gas(&mut self, units: u64) {
        let wei_per_gas = self.wei_per_gas();
        self.gas_units += units as f64;
        self.gas_wei += units as f64 * to_f64(wei_per_gas);
        let cost = self.gas_cost(units) + std::mem::take(&mut self.gas_owed);
        if cost > self.cash {
            self.gas_owed = cost;
            return;
        }
        self.cash -= cost;
        self.gas_total += cost;
        if let Some(campaign) = &mut self.campaign {
            campaign.gas += cost;
        }
    }

    /// What the patience decision sees of the pool now.
    fn market(&self) -> Result<MarketView, MathError> {
        let price = self.pool.sqrt_price_x96();
        let tick = get_tick_at_sqrt_ratio(price)?;
        let on_tick = get_sqrt_ratio_at_tick(tick)? == price;
        Ok(MarketView {
            current_tick: self.pool.tick(),
            tick_spacing: self.pool.tick_spacing(),
            price_on_tick: on_tick.then_some(tick),
        })
    }

    /// Puts `amount` of one token into a range that holds only that token
    /// at the current price, shrunk to `cap`'s share of the range's
    /// thinnest liquidity when given. `None` when the price is not on the
    /// right side of the range or the amount buys no liquidity.
    fn open_leg(
        &mut self,
        token: Token,
        amount: U256,
        lower: Tick,
        upper: Tick,
        flip: bool,
        limits: LegLimits,
    ) -> Result<Option<Opened>, MathError> {
        let LegLimits { cap, min_used } = limits;
        let sqrt_lower = get_sqrt_ratio_at_tick(lower)?;
        let sqrt_upper = get_sqrt_ratio_at_tick(upper)?;
        let price = self.pool.sqrt_price_x96();
        let bought = match token {
            Token::Zero if price > sqrt_lower => return Ok(None),
            Token::One if price < sqrt_upper => return Ok(None),
            Token::Zero => get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount)?,
            Token::One => get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount)?,
        };
        let (liquidity, capped) = match cap {
            Some(cap) => cap.shrink(bought, thinnest_liquidity(&self.pool, lower, upper)),
            None => (bought, false),
        };
        let used = match token {
            Token::Zero => get_amount0_delta(sqrt_lower, sqrt_upper, liquidity, true)?,
            Token::One => get_amount1_delta(sqrt_lower, sqrt_upper, liquidity, true)?,
        };
        if liquidity == 0 || used > amount || used < min_used {
            return Ok(None);
        }
        if self.pool.watch_tick(lower).is_err() {
            return Ok(None);
        }
        if self.pool.watch_tick(upper).is_err() {
            self.pool.unwatch_tick(lower);
            return Ok(None);
        }
        self.leg = Some(Leg {
            flip,
            lower,
            upper,
            sqrt_lower,
            sqrt_upper,
            liquidity,
            growth0: U256::ZERO,
            growth1: U256::ZERO,
        });
        Ok(Some(Opened { used, capped }))
    }

    /// Whether the pool price is strictly inside `[lower, upper)`, so a
    /// range there holds both tokens.
    fn holds_both(&self, lower: Tick, upper: Tick) -> Result<bool, MathError> {
        let price = self.pool.sqrt_price_x96();
        Ok(get_sqrt_ratio_at_tick(lower)? < price && price < get_sqrt_ratio_at_tick(upper)?)
    }

    /// Puts `value` and `other` into `[lower, upper)`, wherever the price
    /// is: whichever token is over the range's mix at the pool price is
    /// swapped into the other one, against a copy of the pool as a closing
    /// swap is, and the liquidity both amounts then buy is added. A range
    /// the price is below takes only token0, one it is above only token1.
    /// The share cap is applied first, on the liquidity the whole amount
    /// would buy, so what it holds back is not swapped. `None`, with
    /// nothing changed, when the amounts are worth less than
    /// `limits.min_used` or buy no liquidity.
    fn open_mixed(
        &mut self,
        value: U256,
        other: U256,
        lower: Tick,
        upper: Tick,
        limits: LegLimits,
    ) -> Result<Option<Mixed>, SimError> {
        let sqrt_lower = get_sqrt_ratio_at_tick(lower)?;
        let sqrt_upper = get_sqrt_ratio_at_tick(upper)?;
        let price = self.pool.sqrt_price_x96();
        // The range's mix at the price, from what a reference liquidity
        // holds, both sides counted in the value token.
        const REFERENCE: u128 = 1 << 96;
        let (ref0, ref1) = get_amounts_for_liquidity(price, sqrt_lower, sqrt_upper, REFERENCE)?;
        let (ref_value, ref_other) = self.split(ref0, ref1);
        let ref_other = self.other_in_value(ref_other)?;
        let ref_total = ref_value + ref_other;
        if ref_total.is_zero() {
            return Ok(None);
        }

        let (mut value_in, mut other_in, mut capped) = (value, other, false);
        if let Some(cap) = limits.cap {
            let total = value + self.other_in_value(other)?;
            let bought = u128::try_from(mul_div(U256::from(REFERENCE), total, ref_total)?)
                .unwrap_or(u128::MAX);
            let (allowed, shrunk) =
                cap.shrink(bought, thinnest_liquidity(&self.pool, lower, upper));
            if shrunk {
                value_in = mul_div(value, U256::from(allowed), U256::from(bought))?;
                other_in = mul_div(other, U256::from(allowed), U256::from(bought))?;
                capped = true;
            }
        }
        let held_other = self.other_in_value(other_in)?;
        let total_in = value_in + held_other;
        if total_in.is_zero() || total_in < limits.min_used {
            return Ok(None);
        }

        // Swap whichever token is over the mix.
        let target_other = mul_div(total_in, ref_other, ref_total)?;
        let (mut value_have, mut other_have) = (value_in, other_in);
        let mut swapped = false;
        if held_other < target_other {
            let sell = (target_other - held_other).min(value_have);
            let got = self.swap_from(self.s.value, sell)?;
            value_have -= sell;
            other_have += got;
            swapped = !sell.is_zero();
        } else if held_other > target_other {
            let sell = self
                .value_in_other(held_other - target_other)?
                .min(other_have);
            let got = self.swap_from(self.s.value.other(), sell)?;
            other_have -= sell;
            value_have += got;
            swapped = !sell.is_zero();
        }

        // The liquidity both amounts buy at the pool price. The pool rounds
        // up what a mint takes, so where that asks for a unit more than
        // there is, one unit less liquidity is added.
        let (amount0, amount1) = self.split(value_have, other_have);
        let mut liquidity =
            get_liquidity_for_amounts(price, sqrt_lower, sqrt_upper, amount0, amount1)?;
        let mut taken = None;
        for _ in 0..4 {
            if liquidity == 0 {
                return Ok(None);
            }
            let (take0, take1) = if price <= sqrt_lower {
                (
                    get_amount0_delta(sqrt_lower, sqrt_upper, liquidity, true)?,
                    U256::ZERO,
                )
            } else if price < sqrt_upper {
                (
                    get_amount0_delta(price, sqrt_upper, liquidity, true)?,
                    get_amount1_delta(sqrt_lower, price, liquidity, true)?,
                )
            } else {
                (
                    U256::ZERO,
                    get_amount1_delta(sqrt_lower, sqrt_upper, liquidity, true)?,
                )
            };
            if take0 <= amount0 && take1 <= amount1 {
                taken = Some((take0, take1));
                break;
            }
            liquidity -= 1;
        }
        let Some((take0, take1)) = taken else {
            return Ok(None);
        };
        if self.pool.watch_tick(lower).is_err() {
            return Ok(None);
        }
        if self.pool.watch_tick(upper).is_err() {
            self.pool.unwatch_tick(lower);
            return Ok(None);
        }
        self.leg = Some(Leg {
            flip: false,
            lower,
            upper,
            sqrt_lower,
            sqrt_upper,
            liquidity,
            growth0: U256::ZERO,
            growth1: U256::ZERO,
        });
        let (took_value, took_other) = self.split(take0, take1);
        Ok(Some(Mixed {
            value_left: (value - value_in) + (value_have - took_value),
            other_left: (other - other_in) + (other_have - took_other),
            swapped,
            capped,
        }))
    }

    /// Takes the position out: (value token, other token), fees included.
    fn withdraw(&mut self) -> Result<(U256, U256), MathError> {
        let Some(leg) = self.leg.take() else {
            return Ok((U256::ZERO, U256::ZERO));
        };
        let (value, other) = self.holdings(&leg)?;
        let (fee_value, fee_other) = self.leg_fees(&leg);
        self.pool.unwatch_tick(leg.lower);
        self.pool.unwatch_tick(leg.upper);
        let earned = to_f64(fee_value) + to_f64(fee_other) * self.value_per_other();
        self.fees_total += earned;
        if let Some(campaign) = &mut self.campaign {
            campaign.fees += earned;
        }
        Ok((value + fee_value, other + fee_other))
    }

    /// Swaps the other token into the value token against a copy of the
    /// pool, so the fee and the price impact are the real ones.
    fn swap_to_value(&self, amount_other: U256) -> Result<U256, SimError> {
        self.swap_from(self.s.value.other(), amount_other)
    }

    /// Swaps `amount` of `from` into the pool's other token against a copy
    /// of the pool, and returns what comes out.
    fn swap_from(&self, from: Token, amount: U256) -> Result<U256, SimError> {
        if amount.is_zero() {
            return Ok(U256::ZERO);
        }
        let zero_for_one = from == Token::Zero;
        let limit = if zero_for_one {
            MIN_SQRT_RATIO + U256::from(1u8)
        } else {
            MAX_SQRT_RATIO - U256::from(1u8)
        };
        let amount = I256::try_from(amount)
            .map_err(|_| SimError::Settings("an amount is too large to swap".into()))?;
        let amounts = self
            .pool
            .clone()
            .swap(zero_for_one, amount, limit, &mut ())?;
        let out = if zero_for_one {
            amounts.amount1
        } else {
            amounts.amount0
        };
        Ok(out.unsigned_abs())
    }

    fn end_campaign(&mut self, reason: &str, time: u64, block: u64) {
        let Some(mut campaign) = self.campaign.take() else {
            return;
        };
        campaign.left_range(time);
        self.last_closed = Some(time);
        let unit = self.value_unit;
        let seconds_beyond = campaign.seconds_beyond
            + campaign
                .other_since
                .map_or(0, |since| time.saturating_sub(since));
        self.out.campaigns.push(CampaignRow {
            id: campaign.id,
            tick_lower: campaign.tick_lower,
            tick_upper: campaign.tick_upper,
            opened_time: campaign.opened_time,
            opened_block: campaign.opened_block,
            closed_time: time,
            closed_block: block,
            exit_reason: reason.to_string(),
            shape: if self.s.follow.is_some() {
                "followed".to_string()
            } else {
                self.s.shape.name().to_string()
            },
            amount_in: to_f64(campaign.amount_in) / unit,
            result: (to_f64(self.cash) - to_f64(campaign.cash_before)) / unit,
            fees: campaign.fees / unit,
            gas: to_f64(campaign.gas) / unit,
            passes: campaign.passes,
            flips: campaign.flips,
            recentres: campaign.recentres,
            seconds_beyond,
            seconds_in_range: campaign.seconds_in_range,
        });
    }

    fn execute(&mut self, action: Action, time: u64, block: u64) -> Result<(), SimError> {
        self.block = block;
        match action {
            Action::Open {
                lower,
                upper,
                follow,
            } => {
                if self.leg.is_some() || self.halted {
                    return Ok(());
                }
                let around = self.s.shape == Shape::AroundPrice;
                // A followed range is entered wherever the price is, in
                // its mix; it may hold either token or both.
                let mixed = around || follow.is_some() || self.s.agent.is_some();
                // Gas comes out of the same capital, so it is set aside
                // first. An entry around the price swaps as well as opens.
                let swap_units = if mixed { self.s.gas.swap_units } else { 0 };
                let gas = self.gas_cost(self.s.gas.open_units + swap_units);
                if gas >= self.cash {
                    return Ok(());
                }
                let budget = share_of(self.cash - gas, self.capital_ppm);
                let cash_before = self.cash;
                // Too small to pay for its own way in and out: not made.
                let min_used = self.s.min_gas_multiple.map_or(U256::ZERO, |multiple| {
                    let round_trip = self.gas_cost(
                        self.s.gas.open_units + self.s.gas.close_units + self.s.gas.swap_units,
                    );
                    whole(multiple * to_f64(round_trip)).unwrap_or(U256::MAX)
                });
                let limits = LegLimits {
                    cap: self.share_cap,
                    min_used,
                };
                // `None`: price moved onto (or, around the price, out of)
                // the range in the meantime, or the pool has no liquidity in
                // part of it to take a share of. Look again later.
                let (used, capped, swapped) = if mixed {
                    // A price on or past a bound has no range around it.
                    if around && follow.is_none() && !self.holds_both(lower, upper)? {
                        return Ok(());
                    }
                    let Some(mixed) = self.open_mixed(budget, U256::ZERO, lower, upper, limits)?
                    else {
                        return Ok(());
                    };
                    // What the range could not take of the other token stays
                    // in it, and goes back with the campaign's closing swap.
                    self.loose_other += mixed.other_left;
                    (budget - mixed.value_left, mixed.capped, mixed.swapped)
                } else {
                    let Some(opened) =
                        self.open_leg(self.s.value, budget, lower, upper, false, limits)?
                    else {
                        return Ok(());
                    };
                    (opened.used, opened.capped, false)
                };
                // Whatever the cap left unused stays as cash.
                if capped {
                    self.out.summary.capped_entries += 1;
                }
                self.cash -= used;
                self.campaigns_opened += 1;
                // Where the new position starts, for counting passes. An
                // agent's range may hold either token or both.
                let start_extreme = if self.s.agent.is_some() {
                    match self
                        .leg
                        .as_ref()
                        .map(|leg| self.holdings(leg))
                        .transpose()?
                    {
                        Some((_, other)) if other.is_zero() => Extreme::Value,
                        Some((value, _)) if value.is_zero() => Extreme::Other,
                        _ => Extreme::Neither,
                    }
                } else if mixed {
                    Extreme::Neither
                } else {
                    Extreme::Value
                };
                self.campaign = Some(OpenCampaign {
                    id: self.campaigns_opened,
                    tick_lower: lower,
                    tick_upper: upper,
                    opened_time: time,
                    opened_block: block,
                    cash_before,
                    amount_in: used,
                    fees: 0.0,
                    gas: U256::ZERO,
                    passes: 0,
                    flips: 0,
                    recentres: 0,
                    seconds_beyond: 0,
                    extreme: start_extreme,
                    other_since: None,
                    out_since: None,
                    best: to_f64(cash_before),
                    far_beyond: false,
                    in_range_since: None,
                    seconds_in_range: 0,
                    round_trips: 0,
                });
                let swap_units = if swapped { self.s.gas.swap_units } else { 0 };
                self.charge_gas(self.s.gas.open_units + swap_units);
                self.following = follow;
            }
            Action::Recenter { decision } => {
                let Some(leg) = &self.leg else {
                    return Ok(());
                };
                // Back in its range in the meantime: it stays.
                let (held_value, held_other) = self.holdings(leg)?;
                if !held_value.is_zero() && !held_other.is_zero() {
                    return Ok(());
                }
                let Some((lower, upper)) = around_price(
                    self.pool.tick(),
                    self.pool.tick_spacing(),
                    self.width_now(time),
                ) else {
                    return Ok(());
                };
                // A price exactly on a bound of the new range has no range
                // around it yet: look again at the next decision.
                let price = self.pool.sqrt_price_x96();
                if (lower, upper) == (leg.lower, leg.upper)
                    || price <= get_sqrt_ratio_at_tick(lower)?
                    || price >= get_sqrt_ratio_at_tick(upper)?
                {
                    return Ok(());
                }
                if let Some(row) = self.out.decisions.get_mut(decision) {
                    row.target_lower = Some(lower);
                    row.target_upper = Some(upper);
                }
                let (value, other) = self.withdraw()?;
                let other = other + std::mem::take(&mut self.loose_other);
                let limits = LegLimits {
                    cap: self.share_cap,
                    min_used: U256::ZERO,
                };
                match self.open_mixed(value, other, lower, upper, limits)? {
                    Some(mixed) => {
                        self.cash += mixed.value_left;
                        self.loose_other = mixed.other_left;
                        let swap_units = if mixed.swapped {
                            self.s.gas.swap_units
                        } else {
                            0
                        };
                        self.charge_gas(
                            self.s.gas.close_units + self.s.gas.open_units + swap_units,
                        );
                        if let Some(campaign) = &mut self.campaign {
                            campaign.recentres += 1;
                            campaign.extreme = Extreme::Neither;
                            campaign.out_since = None;
                            campaign.far_beyond = false;
                            if let Some(since) = campaign.other_since.take() {
                                campaign.seconds_beyond += time.saturating_sub(since);
                            }
                        }
                    }
                    None => {
                        // The new range could not open: everything goes back
                        // to the value token and the campaign ends.
                        self.cash += value;
                        self.loose_other = other;
                        self.charge_gas(self.s.gas.close_units);
                        self.swap_loose_to_value()?;
                        self.end_campaign("recenter_failed", time, block);
                    }
                }
            }
            Action::Withdraw { reason } => {
                let Some(leg) = &self.leg else {
                    return Ok(());
                };
                // Only a position wholly in the value token leaves this way.
                if !self.holdings(leg)?.1.is_zero() {
                    return Ok(());
                }
                self.close_out(reason, time, block)?;
            }
            Action::Close { reason } => {
                if self.leg.is_none() {
                    return Ok(());
                }
                self.close_out(reason, time, block)?;
            }
            Action::Flip { decision } => {
                let Some(leg) = &self.leg else {
                    return Ok(());
                };
                // The flip goes next to the price as it is now, and only
                // while the position is still wholly in the other token.
                let (held_value, _) = self.holdings(leg)?;
                let view = self.campaign_view(leg);
                let market = self.market()?;
                let PatienceAction::Flip { target } = FixedRule::Flip.decide(&view, &market) else {
                    return Ok(());
                };
                if !held_value.is_zero()
                    || (target.tick_lower, target.tick_upper) == (leg.lower, leg.upper)
                {
                    return Ok(());
                }
                // The decision's row shows the range the flip went to. It
                // can differ from the one next to the price when it was
                // decided, if the price moved in the meantime.
                if let Some(row) = self.out.decisions.get_mut(decision) {
                    row.target_lower = Some(target.tick_lower);
                    row.target_upper = Some(target.tick_upper);
                }
                let (value, other) = self.withdraw()?;
                self.cash += value;
                let other = other + std::mem::take(&mut self.loose_other);
                let other_token = self.s.value.other();
                match self.open_leg(
                    other_token,
                    other,
                    target.tick_lower,
                    target.tick_upper,
                    true,
                    LegLimits::NONE,
                )? {
                    Some(opened) => {
                        // The few units the range could not take stay in
                        // the other token, as they would in the Safe. They
                        // go back with the campaign's closing swap, so no
                        // swap is made here.
                        self.loose_other = other - opened.used;
                        self.charge_gas(self.s.gas.close_units + self.s.gas.open_units);
                        if let Some(campaign) = &mut self.campaign {
                            campaign.flips += 1;
                            campaign.far_beyond = false;
                        }
                    }
                    None => {
                        // The new leg could not open: everything goes back
                        // to the value token and the campaign ends. Its exit
                        // says so, beside the decision row's `flip`.
                        self.loose_other = other;
                        self.charge_gas(self.s.gas.close_units);
                        self.swap_loose_to_value()?;
                        self.end_campaign("flip_failed", time, block);
                    }
                }
            }
        }
        Ok(())
    }

    /// Withdraws the leg, swaps all the other token back and ends the
    /// campaign.
    fn close_out(&mut self, reason: &str, time: u64, block: u64) -> Result<(), SimError> {
        let (value, other) = self.withdraw()?;
        self.cash += value;
        self.loose_other += other;
        self.charge_gas(self.s.gas.close_units);
        self.swap_loose_to_value()?;
        self.end_campaign(reason, time, block);
        Ok(())
    }

    /// Swaps all the other token held outside a position back to the value
    /// token, and charges the swap.
    fn swap_loose_to_value(&mut self) -> Result<(), SimError> {
        let other = std::mem::take(&mut self.loose_other);
        if !other.is_zero() {
            self.cash += self.swap_to_value(other)?;
            self.charge_gas(self.s.gas.swap_units);
        }
        Ok(())
    }

    /// The leg as the patience policy sees it: a campaign on the entry
    /// side whose range price has left.
    fn campaign_view(&self, leg: &Leg) -> Campaign {
        Campaign {
            id: String::new(),
            pool: PoolId(String::new()),
            value_token: String::new(),
            tick_lower: leg.lower,
            tick_upper: leg.upper,
            side: self.side(),
            amount_in_raw: 0,
            opened_at_block: 0,
        }
    }

    fn score(&mut self, lower: Tick, upper: Tick, now: u64) -> ZoneScore {
        let shortest = self.s.lookbacks.iter().copied().min().unwrap_or(DAY);
        let longest = self.s.lookbacks.iter().copied().max().unwrap_or(DAY);
        let short = self.zone_look(lower, upper, shortest, now);
        let long = self.zone_look(lower, upper, longest, now);
        ZoneScore {
            passes_per_day_long: long.passes_per_day,
            passes_per_day_short: short.passes_per_day,
            enough_history: long.observed_seconds >= longest.min(DAY) as f64,
        }
    }

    /// One range's figures over one lookback, measured again every twelfth
    /// of the lookback (at least a minute, at most a day). A look is
    /// recorded in the zone table at most once an hour per range and
    /// lookback.
    fn zone_look(&mut self, lower: Tick, upper: Tick, lookback: u64, now: u64) -> Look {
        let period = now / (lookback / 12).clamp(60, DAY);
        if let Some(look) = self.scores.get(&(lower, upper, lookback)) {
            if look.period == period {
                return *look;
            }
        }
        let stats = zone_stats(&self.path, lower, upper, now.saturating_sub(lookback), now);
        let hour = now / 3_600;
        let logged = self
            .scores
            .get(&(lower, upper, lookback))
            .map(|l| l.logged_hour);
        if logged != Some(hour) {
            self.out.zones.push(ZoneLook {
                time: now,
                lookback_days: lookback as f64 / DAY as f64,
                tick_lower: lower,
                tick_upper: upper,
                observed_days: stats.observed_days(),
                passes_per_day: stats.passes_per_day(),
                time_in_range: stats.time_in_range(),
                return_odds_1d: stats.return_odds(DAY, None),
            });
        }
        let look = Look {
            period,
            logged_hour: hour,
            passes_per_day: stats.passes_per_day(),
            observed_seconds: stats.observed_days() * DAY as f64,
        };
        self.scores.insert((lower, upper, lookback), look);
        look
    }

    /// The range to enter now. Around the price there is one, the range
    /// centred on it; with history, it must have been crossed at least
    /// `min_passes_per_day` times a day on the long lookback. One-sided,
    /// the zone choice among the ranges on the entry side.
    fn best_entry(&mut self, now: u64) -> Option<(Tick, Tick)> {
        if self.s.shape == Shape::AroundPrice {
            let width = self.width_now(now);
            let range = around_price(self.pool.tick(), self.pool.tick_spacing(), width)?;
            let score = self.score(range.0, range.1, now);
            return (!score.enough_history
                || score.passes_per_day_long >= self.s.min_passes_per_day)
                .then_some(range);
        }
        let candidates = entry_candidates(
            self.pool.tick(),
            self.pool.tick_spacing(),
            self.width_now(now),
            self.side(),
            self.s.max_gap,
        );
        let scores: Vec<ZoneScore> = candidates
            .iter()
            .map(|&(lower, upper)| self.score(lower, upper, now))
            .collect();
        let mut next = scores.into_iter();
        choose_zone(&candidates, self.s.min_passes_per_day, |_, _| {
            next.next().expect("one score each")
        })
    }

    /// The adaptive patience decision for a leg that is wholly in the
    /// other token: what waiting, flipping and closing are each expected to
    /// leave in the value token, and the choice. Without enough history to
    /// count odds on it waits and expects nothing.
    fn adaptive_decision(
        &mut self,
        params: &AdaptiveParams,
        leg: &Leg,
        view: &Campaign,
        market: &MarketView,
        now: u64,
    ) -> Result<(PatienceAction, Option<Expected>), SimError> {
        let tick = market.current_tick;
        let fee = f64::from(self.pool.fee_pips()) / 1_000_000.0;
        let per_other = self.value_per_other();
        let (held_value, held_other) = self.holdings(leg)?;
        let (fee_value, fee_other) = self.leg_fees(leg);
        // The other token the leg does not hold as liquidity: its fees and
        // what an earlier flip's range could not take.
        let other_outside = fee_other + self.loose_other;
        let other = to_f64(held_other + other_outside);
        let kept = to_f64(held_value + fee_value);
        let gas = |units: u64| to_f64(self.gas_cost(units));
        let (open, close, swap) = (
            self.s.gas.open_units,
            self.s.gas.close_units,
            self.s.gas.swap_units,
        );
        // The other token is worth 1.0001^ticks more of the value token
        // for every tick the price moves towards the value side.
        let toward_value = |ticks: f64| match self.s.value {
            Token::One => 1.0001f64.powf(ticks),
            Token::Zero => 1.0001f64.powf(-ticks),
        };
        // Ticks price has to travel to pass through a range to the value side.
        let pass_through = |lower: Tick, upper: Tick| match self.s.value {
            Token::One => upper - tick,
            Token::Zero => lower - 1 - tick,
        };
        let swapped_at = |ticks_moved: f64| {
            other * per_other * toward_value(ticks_moved) * (1.0 - fee) + kept - gas(close + swap)
        };
        let lookback = u64::from(params.lookback_days) * DAY;

        // Waiting: the leg pays out its whole range in the value token,
        // plus the fee on the pass, if price comes back through it.
        let back = pass_through(leg.lower, leg.upper);
        let Some(back_odds) = self.buckets.move_odds(now, lookback, back, params.horizon) else {
            return Ok((PatienceAction::Wait, None));
        };
        let whole_range = to_f64(match self.s.value {
            Token::Zero => get_amount0_delta(leg.sqrt_lower, leg.sqrt_upper, leg.liquidity, false)?,
            Token::One => get_amount1_delta(leg.sqrt_lower, leg.sqrt_upper, leg.liquidity, false)?,
        });
        let wait = Gamble {
            odds: back_odds.p_hit,
            if_made: whole_range * (1.0 + fee) + kept + to_f64(other_outside) * per_other,
            if_missed: swapped_at(back_odds.drift_if_missed),
        };

        // Flipping: the same token in the range next to the price.
        let target = match FixedRule::Flip.decide(view, market) {
            PatienceAction::Flip { target }
                if (target.tick_lower, target.tick_upper) != (leg.lower, leg.upper) =>
            {
                Some(target)
            }
            _ => None,
        };
        let flip = target.as_ref().and_then(|target| {
            let through = pass_through(target.tick_lower, target.tick_upper);
            let odds = self
                .buckets
                .move_odds(now, lookback, through, params.horizon)?;
            let middle = f64::from(target.tick_lower + target.tick_upper) / 2.0 - f64::from(tick);
            Some(Gamble {
                odds: odds.p_hit,
                if_made: other * per_other * toward_value(middle) * (1.0 + fee) + kept
                    - gas(close + open)
                    - gas(close),
                if_missed: swapped_at(odds.drift_if_missed) - gas(close + open),
            })
        });

        // Closing: the swap now, plus what the freed capital can earn in
        // the best range at hand over the same horizon.
        let closed = swapped_at(0.0);
        let best_yield = match self.best_entry(now) {
            Some((lower, upper)) => self.score(lower, upper, now).passes_per_day_long * fee,
            None => 0.0,
        };
        let horizon_days = params.horizon as f64 / DAY as f64;
        let options = Options {
            wait,
            flip,
            close: closed * (1.0 + best_yield * horizon_days * params.reinvest_credit),
            position_value: closed,
        };
        let expected = adaptive::expected(&options);
        let action = match adaptive::choose(&expected, options.position_value, params.switch_margin)
        {
            Choice::Wait => PatienceAction::Wait,
            Choice::Close => PatienceAction::Close,
            Choice::Flip => match target {
                Some(target) => PatienceAction::Flip { target },
                None => PatienceAction::Wait,
            },
        };
        Ok((action, Some(expected)))
    }

    /// One block's step when following a provider: an open campaign is
    /// closed by its own exit rules or once the provider has closed the
    /// position it copies; flat, the latest of the provider's positions
    /// opened within the join window and still open is entered.
    fn follow_step(
        &mut self,
        spans: &[FollowSpan],
        time: u64,
        block: u64,
        equity: f64,
        state: &str,
    ) {
        if self.leg.is_some() {
            if let Some(reason) = self.campaign_exit(time, equity, state) {
                self.schedule(block, Action::Close { reason });
            } else if self
                .following
                .and_then(|i| spans.get(i))
                .is_none_or(|span| span.close_time.is_some_and(|close| time >= close))
            {
                self.schedule(
                    block,
                    Action::Close {
                        reason: "leader_exit",
                    },
                );
            }
            return;
        }
        let mut chosen = None;
        while let Some(span) = spans.get(self.follow_next) {
            if span.open_time > time {
                break;
            }
            let joinable = span.open_time.saturating_add(self.s.follow_join_within) >= time
                && span.close_time.is_none_or(|close| close > time);
            if joinable {
                chosen = Some(self.follow_next);
            }
            self.follow_next += 1;
        }
        if let Some(i) = chosen {
            let span = spans[i];
            self.schedule(
                block,
                Action::Open {
                    lower: span.lower,
                    upper: span.upper,
                    follow: Some(i),
                },
            );
        }
    }

    /// +1 when the volatile token's price rises with the tick, -1 when it
    /// falls with it.
    fn rise_sign(&self) -> Tick {
        let dollar = match self.s.dollar {
            Dollar::Value => self.s.value,
            Dollar::Other => self.s.value.other(),
        };
        if dollar == Token::One {
            1
        } else {
            -1
        }
    }

    /// The volatile token's price at `tick`, in percent from its price now.
    fn pct_from_now(&self, tick: Tick) -> f64 {
        ticks_to_pct(f64::from(self.rise_sign() * (tick - self.pool.tick())))
    }

    /// The agent mode at the end of a block: the campaign exits, then the
    /// look if one is due, with its decision or, without one, a stop.
    fn agent_step(
        &mut self,
        plan: &AgentPlan,
        time: u64,
        block: u64,
        equity: f64,
        state: &str,
    ) -> Result<(), SimError> {
        if self.leg.is_some() {
            if let Some(reason) = self.campaign_exit(time, equity, state) {
                self.agent_reenter = None;
                self.schedule(block, Action::Close { reason });
                return Ok(());
            }
        } else if let Some((lower, upper)) = self.agent_reenter.take() {
            // The second half of a move: the old range is closed.
            self.schedule(
                block,
                Action::Open {
                    lower,
                    upper,
                    follow: None,
                },
            );
            return Ok(());
        }
        if time < self.agent_next {
            return Ok(());
        }
        let look = self.agent_next;
        let Some(decision) = plan
            .decisions
            .get(self.agent_used)
            .filter(|d| d.look == look)
        else {
            self.out.pending = Some(self.observe(look, time, block, equity, state)?);
            self.stopped = true;
            return Ok(());
        };
        self.agent_used += 1;
        self.agent_next = time + decision.next_look.max(60);
        self.apply_agent(decision, time, block)
    }

    fn apply_agent(
        &mut self,
        decision: &AgentDecision,
        time: u64,
        block: u64,
    ) -> Result<(), SimError> {
        let tick = self.pool.tick();
        let target = match decision.action {
            AgentAction::Enter {
                lower_pct,
                upper_pct,
            } => self.agent_range(lower_pct, upper_pct),
            _ => None,
        };
        self.out.decisions.push(DecisionRow {
            campaign: self.campaign.as_ref().map_or(0, |c| c.id),
            time,
            block,
            tick,
            leg: "agent".to_string(),
            seconds_stranded: 0,
            action: decision.action.name().to_string(),
            target_lower: target.map(|t| t.0),
            target_upper: target.map(|t| t.1),
            expected_wait: None,
            expected_flip: None,
            expected_close: None,
        });
        match decision.action {
            AgentAction::StayOut | AgentAction::Hold => {}
            AgentAction::Exit => {
                if self.leg.is_some() {
                    self.schedule(
                        block,
                        Action::Close {
                            reason: "agent_exit",
                        },
                    );
                }
            }
            AgentAction::Enter { .. } => {
                let Some((lower, upper)) = target else {
                    return Ok(());
                };
                match &self.leg {
                    Some(leg) if (leg.lower, leg.upper) == (lower, upper) => {}
                    Some(_) => {
                        self.agent_reenter = Some((lower, upper));
                        self.schedule(
                            block,
                            Action::Close {
                                reason: "agent_move",
                            },
                        );
                    }
                    None => self.schedule(
                        block,
                        Action::Open {
                            lower,
                            upper,
                            follow: None,
                        },
                    ),
                }
            }
        }
        Ok(())
    }

    /// The range between two volatile-token prices given in percent from
    /// the price now, widened out to the tick spacing. `None` for prices
    /// that are not prices.
    fn agent_range(&self, lower_pct: f64, upper_pct: f64) -> Option<(Tick, Tick)> {
        if !(lower_pct.is_finite() && upper_pct.is_finite())
            || lower_pct <= -100.0
            || upper_pct <= -100.0
        {
            return None;
        }
        let tick = self.pool.tick();
        let sign = self.rise_sign();
        let to_tick = |pct: f64| tick + sign * pct_to_ticks(pct).round() as Tick;
        let (a, b) = (to_tick(lower_pct), to_tick(upper_pct));
        let spacing = self.pool.tick_spacing().max(1);
        let lower = a.min(b).div_euclid(spacing) * spacing;
        let mut upper = (a.max(b) + spacing - 1).div_euclid(spacing) * spacing;
        if upper <= lower {
            upper = lower + spacing;
        }
        Some((lower, upper))
    }

    /// How the pool and the position look now, for a look at `look`.
    fn observe(
        &self,
        look: u64,
        time: u64,
        block: u64,
        equity: f64,
        state: &str,
    ) -> Result<Observation, SimError> {
        let tick_now = self.pool.tick();
        let sign = self.rise_sign();
        let rise = |tick: Tick| sign * (tick - tick_now);
        let month = 30 * DAY;
        let month_from = self
            .path
            .partition_point(|p| p.time <= time.saturating_sub(month));
        let month_volume: f64 = self.path[month_from..].iter().map(|p| p.volume1).sum();
        let month_seconds = time
            .saturating_sub(self.path.first().map_or(time, |p| p.time))
            .clamp(1, month);
        let mut scales = Vec::new();
        for (label, seconds) in SCALES {
            let from = time.saturating_sub(seconds);
            let first = self.path.partition_point(|p| p.time <= from);
            let start_tick = if first > 0 {
                self.path[first - 1].tick
            } else {
                self.path.first().map_or(tick_now, |p| p.tick)
            };
            let window = &self.path[first..];
            let (mut high, mut low) = (rise(start_tick), rise(start_tick));
            for point in window {
                high = high.max(rise(point.tick));
                low = low.min(rise(point.tick));
            }
            high = high.max(0);
            low = low.min(0);
            let volume: f64 = window.iter().map(|p| p.volume1).sum();
            let pace = volume / seconds as f64;
            let month_pace = month_volume / month_seconds as f64;
            scales.push(ScaleView {
                label,
                seconds,
                change_pct: -ticks_to_pct(f64::from(rise(start_tick)))
                    / (1.0 + ticks_to_pct(f64::from(rise(start_tick))) / 100.0),
                high_pct: ticks_to_pct(f64::from(high)),
                low_pct: ticks_to_pct(f64::from(low)),
                vol_pct_per_day: self
                    .buckets
                    .daily_vol_ticks(time, seconds)
                    .map(ticks_to_pct),
                swaps: window.len(),
                volume_vs_30d: (month_pace > 0.0).then(|| pace / month_pace),
            });
        }
        let round_trip =
            self.gas_cost(self.s.gas.open_units + self.s.gas.close_units + self.s.gas.swap_units);
        let position = match (&self.leg, &self.campaign) {
            (Some(leg), Some(campaign)) => {
                let (value, other) = self.holdings(leg)?;
                let (fee_value, fee_other) = self.leg_fees(leg);
                let per_other = self.value_per_other();
                let amount = to_f64(campaign.amount_in).max(1.0);
                let held_value = to_f64(value);
                let held_total = held_value + to_f64(other) * per_other;
                let (a, b) = (self.pct_from_now(leg.lower), self.pct_from_now(leg.upper));
                Some(PositionView {
                    lower_pct: a.min(b),
                    upper_pct: a.max(b),
                    state: state.to_string(),
                    held_seconds: time.saturating_sub(campaign.opened_time),
                    result_pct: (equity - to_f64(campaign.cash_before)) / amount * 100.0,
                    fees_pct: (to_f64(fee_value) + to_f64(fee_other) * per_other) / amount * 100.0,
                    value_share_pct: if held_total > 0.0 {
                        held_value / held_total * 100.0
                    } else {
                        100.0
                    },
                })
            }
            _ => None,
        };
        Ok(Observation {
            look,
            looks_before: self.agent_used,
            day: time.saturating_sub(self.s.trade_from) as f64 / DAY as f64,
            block,
            scales,
            fee_pct: f64::from(self.pool.fee_pips()) / 10_000.0,
            spacing_pct: ticks_to_pct(f64::from(self.pool.tick_spacing())),
            gas_gwei: to_f64(self.wei_per_gas()) / WEI_PER_GWEI,
            round_trip_gas_pct: to_f64(round_trip) / equity.max(1.0) * 100.0,
            result_pct: (equity / to_f64(self.s.capital) - 1.0) * 100.0,
            campaigns: self.campaigns_opened,
            since_last_close: self.last_closed.map(|closed| time.saturating_sub(closed)),
            position,
        })
    }

    /// The exit rule, if any, that ends the open campaign now, given the
    /// run's equity and the position's state.
    fn campaign_exit(&self, time: u64, equity: f64, state: &str) -> Option<&'static str> {
        let campaign = self.campaign.as_ref()?;
        if self
            .s
            .max_hold
            .is_some_and(|limit| time.saturating_sub(campaign.opened_time) >= limit)
        {
            return Some("max_hold");
        }
        // Only the campaign changes the equity while it is open, gas
        // included, so its value is the equity less what stayed outside.
        let amount = to_f64(campaign.amount_in);
        if amount > 0.0 {
            let loss = (to_f64(campaign.cash_before) - equity) / amount * 100.0;
            if self.s.max_loss_pct.is_some_and(|limit| loss >= limit) {
                return Some("stop_loss");
            }
            let fall = (campaign.best - equity) / amount * 100.0;
            if self.s.max_drawdown_pct.is_some_and(|limit| fall >= limit) {
                return Some("trailing_stop");
            }
        }
        if campaign.far_beyond && (state == "in_range" || state == "resting") {
            return Some("return_exit");
        }
        if state == "resting"
            && self
                .s
                .after_round_trips
                .is_some_and(|n| campaign.round_trips >= n)
        {
            return Some("round_trip_exit");
        }
        None
    }

    fn schedule(&mut self, block: u64, action: Action) {
        self.pending = Some((block + self.s.latency_blocks.max(1), action));
    }

    /// Everything that happens once a block's events are applied.
    fn on_block_end(&mut self, time: u64, block: u64) -> Result<(), SimError> {
        self.block = block;
        if time < self.s.trade_from {
            return Ok(());
        }
        let tick = self.pool.tick();
        if self.start_price.is_none() {
            self.start_price = Some(self.price());
        }

        // Where the position stands, and the passes it has completed.
        let mut state = "flat";
        let mut stranded_since = None;
        let mut out_since = None;
        let mut resting = false;
        if let Some(leg) = &self.leg {
            let (value, other) = self.holdings(leg)?;
            let flip = leg.flip;
            let campaign = self.campaign.as_mut().expect("a leg belongs to a campaign");
            // A pass is counted from one side of the range to the other;
            // leaving a range around the price for the first time is half
            // of one.
            if value.is_zero() && !other.is_zero() {
                if campaign.extreme == Extreme::Value {
                    campaign.passes += 1;
                }
                campaign.extreme = Extreme::Other;
                campaign.other_since.get_or_insert(time);
                campaign.out_since.get_or_insert(time);
                campaign.left_range(time);
                // How far past its nearest bound price is.
                let past = if tick < leg.lower {
                    leg.lower - tick
                } else {
                    tick - leg.upper
                };
                if self
                    .s
                    .exit_on_return_ticks
                    .is_some_and(|limit| past >= limit)
                {
                    campaign.far_beyond = true;
                }
                stranded_since = campaign.other_since;
                state = if flip { "flipped" } else { "beyond" };
            } else if other.is_zero() {
                if campaign.extreme == Extreme::Other {
                    campaign.passes += 1;
                    campaign.round_trips += 1;
                }
                campaign.extreme = Extreme::Value;
                if let Some(since) = campaign.other_since.take() {
                    campaign.seconds_beyond += time.saturating_sub(since);
                }
                campaign.out_since.get_or_insert(time);
                campaign.left_range(time);
                resting = true;
                state = "resting";
            } else {
                campaign.out_since = None;
                campaign.in_range_since.get_or_insert(time);
                state = "in_range";
            }
            out_since = campaign.out_since;
        }

        // The loss limit is checked on every block, before anything else.
        let equity = self.equity()?;
        if let Some(campaign) = &mut self.campaign {
            campaign.best = campaign.best.max(equity);
        }
        self.peak = self.peak.max(equity);
        if self.peak > 0.0 {
            let drawdown = (self.peak - equity) / self.peak * 100.0;
            self.out.summary.max_drawdown_pct = self.out.summary.max_drawdown_pct.max(drawdown);
        }
        if !self.halted {
            if let LossCheck::Breached { .. } =
                self.s.loss_limit.check(to_f64(self.s.capital), equity)
            {
                self.halted = true;
                self.out.summary.halted_by_loss_limit = true;
                self.schedule(
                    block,
                    Action::Close {
                        reason: "loss_limit",
                    },
                );
            }
        }

        if time >= self.next_series {
            let unit = self.value_unit;
            let bounds = self.leg.as_ref().map(|leg| {
                let (a, b) = (self.price_at(leg.lower), self.price_at(leg.upper));
                (a.min(b), a.max(b))
            });
            let price = self.price();
            let usd = self.usd_per_value(price);
            self.out.series.push(SeriesRow {
                time,
                block,
                tick,
                price,
                equity: equity / unit,
                hold: to_f64(self.s.capital) / unit,
                equity_usd: equity / unit * usd,
                hold_usd: to_f64(self.s.capital) / unit * usd,
                fees: (self.fees_total
                    + self.leg.as_ref().map_or(0.0, |leg| {
                        let (fee_value, fee_other) = self.leg_fees(leg);
                        to_f64(fee_value) + to_f64(fee_other) * self.value_per_other()
                    }))
                    / unit,
                state: state.to_string(),
                tick_lower: self.leg.as_ref().map(|l| l.lower),
                tick_upper: self.leg.as_ref().map(|l| l.upper),
                price_lower: bounds.map(|b| b.0),
                price_upper: bounds.map(|b| b.1),
            });
            self.next_series =
                time - time % self.s.series_every.max(1) + self.s.series_every.max(1);
        }

        if self.halted || self.pending.is_some() {
            return Ok(());
        }

        // Following a provider replaces the run's own entries and moves.
        let settings = self.s;
        if let Some(spans) = &settings.follow {
            self.follow_step(spans, time, block, equity, state);
            return Ok(());
        }
        // So do an agent's decisions.
        if let Some(plan) = &settings.agent {
            return self.agent_step(plan, time, block, equity, state);
        }

        let Some(leg) = self.leg.clone() else {
            // Flat: look for a range to enter.
            let cooling = self
                .s
                .cooldown
                .zip(self.last_closed)
                .is_some_and(|(wait, closed)| time < closed.saturating_add(wait));
            if time >= self.next_check && !cooling {
                self.next_check = time + self.s.check_every.max(1);
                if self.calm_enough(time) && self.moved_enough(time) {
                    if let Some((lower, upper)) = self.best_entry(time) {
                        self.schedule(
                            block,
                            Action::Open {
                                lower,
                                upper,
                                follow: None,
                            },
                        );
                    }
                }
            }
            return Ok(());
        };

        // The campaign's own exits come first: its time, its stops, and
        // the return after a deep excursion.
        if let Some(reason) = self.campaign_exit(time, equity, state) {
            self.schedule(block, Action::Close { reason });
            return Ok(());
        }

        // An around-price position price has left for long enough moves
        // to the range around the price, at the pace of the decisions.
        if let (Some(after), Some(since), false) = (self.s.recenter_after, out_since, leg.flip) {
            if time.saturating_sub(since) >= after && time >= self.next_decide {
                self.next_decide = time + self.s.decide_every.max(1);
                let target = around_price(tick, self.pool.tick_spacing(), self.width_now(time));
                self.out.decisions.push(DecisionRow {
                    campaign: self.campaign.as_ref().map_or(0, |c| c.id),
                    time,
                    block,
                    tick,
                    leg: "entry".to_string(),
                    seconds_stranded: time.saturating_sub(since),
                    action: "recenter".to_string(),
                    target_lower: target.map(|t| t.0),
                    target_upper: target.map(|t| t.1),
                    expected_wait: None,
                    expected_flip: None,
                    expected_close: None,
                });
                let decision = self.out.decisions.len() - 1;
                self.schedule(block, Action::Recenter { decision });
                return Ok(());
            }
        }

        if resting {
            if leg.flip {
                // The flip leg is back in the value token: the campaign is done.
                self.schedule(
                    block,
                    Action::Withdraw {
                        reason: "round_trip_complete",
                    },
                );
            } else if time >= self.next_check {
                // A resting entry leg stays for the next pass, unless price
                // has moved away from it or a much busier range is at hand.
                self.next_check = time + self.s.check_every.max(1);
                // The day turned too volatile for the range: back to cash,
                // to come back in when the gates allow.
                let width = f64::from(leg.upper - leg.lower);
                if self
                    .s
                    .exit_vol_to_width
                    .zip(self.daily_vol(time))
                    .is_some_and(|(limit, vol)| vol > limit * width)
                {
                    self.schedule(
                        block,
                        Action::Withdraw {
                            reason: "stepped_aside",
                        },
                    );
                    self.next_check = time;
                    return Ok(());
                }
                // Around the price with a time to move set, moving is that
                // rule's alone; with moves off, nothing moves it.
                if self.s.recenter_after.is_some() || !self.s.reposition {
                    return Ok(());
                }
                let own_gap = gap(tick, leg.lower, leg.upper, self.side());
                let own = self.score(leg.lower, leg.upper, time);
                let best = self.best_entry(time);
                let move_to = match best {
                    Some(range) if range != (leg.lower, leg.upper) => {
                        // A range around the price moves only once price is
                        // far from it: a busier range there would take a
                        // swap to reach.
                        let score = self.score(range.0, range.1, time);
                        let busier = self.s.shape == Shape::OneSided
                            && score.passes_per_day_long
                                > own.passes_per_day_long * self.s.move_margin
                            && score.passes_per_day_long > 0.0;
                        (own_gap > self.s.max_gap.max(leg.upper - leg.lower) || busier)
                            .then_some(range)
                    }
                    _ => None,
                };
                if move_to.is_some() {
                    let passes = self.campaign.as_ref().map_or(0, |c| c.passes);
                    self.schedule(
                        block,
                        Action::Withdraw {
                            reason: if passes >= 2 {
                                "round_trip_complete"
                            } else {
                                "repositioned"
                            },
                        },
                    );
                    // Re-enter at the next look, on the state then.
                    self.next_check = time;
                }
            }
            return Ok(());
        }

        // Wholly in the other token: the patience decision.
        if let Some(since) = stranded_since {
            if time < self.next_decide {
                return Ok(());
            }
            self.next_decide = time + self.s.decide_every.max(1);
            // A flip leg that price is still next to is simply waiting its turn.
            let other_side = match self.side() {
                EntrySide::Above => EntrySide::Below,
                EntrySide::Below => EntrySide::Above,
            };
            if leg.flip && gap(tick, leg.lower, leg.upper, other_side) < leg.upper - leg.lower {
                return Ok(());
            }
            if self
                .s
                .max_seconds_beyond
                .is_some_and(|limit| time.saturating_sub(since) >= limit)
            {
                self.out.decisions.push(DecisionRow {
                    campaign: self.campaign.as_ref().map_or(0, |c| c.id),
                    time,
                    block,
                    tick,
                    leg: if leg.flip { "flip" } else { "entry" }.to_string(),
                    seconds_stranded: time.saturating_sub(since),
                    action: "close".to_string(),
                    target_lower: None,
                    target_upper: None,
                    expected_wait: None,
                    expected_flip: None,
                    expected_close: None,
                });
                self.schedule(
                    block,
                    Action::Close {
                        reason: "patience_timeout",
                    },
                );
                return Ok(());
            }
            let market = self.market()?;
            let view = self.campaign_view(&leg);
            let (action, expected) = match self.s.policy {
                Policy::Fixed(rule) => (rule.decide(&view, &market), None),
                Policy::Adaptive(params) => {
                    self.adaptive_decision(&params, &leg, &view, &market, time)?
                }
            };
            let unit = self.value_unit;
            let (name, target) = match &action {
                PatienceAction::Wait => ("wait", None),
                PatienceAction::Flip { target } => {
                    ("flip", Some((target.tick_lower, target.tick_upper)))
                }
                PatienceAction::Close => ("close", None),
            };
            self.out.decisions.push(DecisionRow {
                campaign: self.campaign.as_ref().map_or(0, |c| c.id),
                time,
                block,
                tick,
                leg: if leg.flip { "flip" } else { "entry" }.to_string(),
                seconds_stranded: time.saturating_sub(since),
                action: name.to_string(),
                target_lower: target.map(|t| t.0),
                target_upper: target.map(|t| t.1),
                expected_wait: expected.map(|e| e.wait / unit),
                expected_flip: expected.and_then(|e| e.flip).map(|v| v / unit),
                expected_close: expected.map(|e| e.close / unit),
            });
            match action {
                PatienceAction::Wait => {}
                PatienceAction::Flip { .. } => {
                    let decision = self.out.decisions.len() - 1;
                    self.schedule(block, Action::Flip { decision });
                }
                PatienceAction::Close => self.schedule(
                    block,
                    Action::Close {
                        reason: "patience_close",
                    },
                ),
            }
        }
        Ok(())
    }
}

/// Replays `events` on top of `snapshot` and trades the settings' strategy.
/// Events must be in chain order and start right after the snapshot.
pub fn run(
    snapshot: PoolSnapshot,
    events: &[TimedEvent],
    settings: &Settings,
) -> Result<Output, SimError> {
    let spacing = snapshot.tick_spacing;
    if settings.width <= 0 || spacing <= 0 || settings.width % spacing != 0 {
        return Err(SimError::Settings(format!(
            "the range width of {} ticks is not a positive multiple of the pool's tick spacing of {spacing}",
            settings.width
        )));
    }
    if settings.capital.is_zero() {
        return Err(SimError::Settings("the run has no capital".into()));
    }
    if settings.lookbacks.is_empty() || settings.lookbacks.contains(&0) {
        return Err(SimError::Settings(
            "the zone lookbacks are empty or hold a zero; set zone.lookbacks to durations such \
             as [6h, 7d]"
                .into(),
        ));
    }
    if settings.signal_step == 0 || settings.vol_window == 0 {
        return Err(SimError::Settings(
            "signal_step and entry.vol_window must be durations above 0, such as 1m and 1d".into(),
        ));
    }
    let positive = |value: Option<f64>| value.is_none_or(|v| v.is_finite() && v > 0.0);
    for (value, key) in [
        (settings.max_loss_pct, "exit.max_loss_pct"),
        (settings.max_drawdown_pct, "exit.max_drawdown_pct"),
        (settings.width_vol_multiple, "entry.width_vol_multiple"),
    ] {
        if !positive(value) {
            return Err(SimError::Settings(format!(
                "{key} is {}; set it to a number above 0, or leave it out",
                value.unwrap_or_default()
            )));
        }
    }
    if settings.after_move_ticks.is_some_and(|ticks| ticks <= 0) || settings.move_window == 0 {
        return Err(SimError::Settings(
            "entry.after_move_pct needs a move above 0 and entry.move_window a duration above 0"
                .into(),
        ));
    }
    if settings.agent.is_some() && settings.follow.is_some() {
        return Err(SimError::Settings(
            "a run follows a provider or an agent's decisions, not both; remove strategy.follow \
             or strategy.agent.decisions"
                .into(),
        ));
    }
    if settings.after_round_trips == Some(0) {
        return Err(SimError::Settings(
            "exit.after_round_trips must be at least 1, or left out".into(),
        ));
    }
    if settings
        .exit_on_return_ticks
        .is_some_and(|ticks| ticks <= 0)
    {
        return Err(SimError::Settings(
            "exit.exit_on_return_pct must be above 0, or left out".into(),
        ));
    }
    if settings.recenter_after.is_some() && settings.shape != Shape::AroundPrice {
        return Err(SimError::Settings(
            "exit.recenter_after moves a position to the range around the price, so it needs \
             entry.shape: around_price; one-sided positions move with entry.move_margin and the \
             patience rule. Set entry.shape: around_price or remove exit.recenter_after"
                .into(),
        ));
    }
    let value_decimals = match settings.value {
        Token::Zero => settings.decimals0,
        Token::One => settings.decimals1,
    };
    // The decimals that decide amounts become integers here, once.
    let capital_ppm =
        parts_per_million(settings.capital_share.clamp(0.0, 1.0)).ok_or_else(|| {
            SimError::Settings(
                "the strategy's capital_share is not a number; set it between 0 and 1".into(),
            )
        })?;
    let share_cap = match settings.max_share_of_in_range_liquidity {
        None => None,
        Some(share) => Some(LiquidityShareCap::new(share).ok_or_else(|| {
            SimError::Settings(format!(
                "universe.max_share_of_in_range_liquidity is {share}; set it above 0 and at \
                 most 1"
            ))
        })?),
    };
    let gas = GasPrice::new(&settings.gas, settings.value)?;
    if let Some(fees) = settings.block_fees.as_deref() {
        let mut traded = events
            .iter()
            .filter(|e| e.time >= settings.trade_from && e.time < settings.trade_to);
        let first = traded.clone().next().map(|e| e.block);
        let last = traded.next_back().map(|e| e.block);
        if let (Some(first), Some(last)) = (first, last) {
            if !fees.covers(first, last) {
                return Err(SimError::Settings(format!(
                    "the stored gas prices cover blocks {} to {}, the run trades blocks {first} \
                     to {last}; run `lpbot gas-history --from <first day>` to store them, or \
                     set costs.gas: fixed",
                    fees.first(),
                    fees.end().saturating_sub(1)
                )));
            }
        }
    }

    let mut engine = Engine {
        s: settings,
        pool: Pool::from_snapshot(snapshot),
        path: Vec::new(),
        cash: settings.capital,
        leg: None,
        campaign: None,
        pending: None,
        next_check: settings.trade_from,
        next_decide: settings.trade_from,
        next_series: settings.trade_from,
        halted: false,
        scores: HashMap::new(),
        buckets: TickBuckets::new(settings.signal_step),
        out: Output::default(),
        campaigns_opened: 0,
        fees_total: 0.0,
        gas,
        gas_total: U256::ZERO,
        gas_owed: U256::ZERO,
        gas_units: 0.0,
        gas_wei: 0.0,
        block: 0,
        capital_ppm,
        share_cap,
        loose_other: U256::ZERO,
        following: None,
        follow_next: 0,
        peak: to_f64(settings.capital),
        value_unit: 10f64.powi(i32::from(value_decimals)),
        start_price: None,
        last_closed: None,
        agent_next: settings.trade_from,
        agent_used: 0,
        agent_reenter: None,
        stopped: false,
    };

    let mut last = (settings.trade_from, 0u64);
    for (i, event) in events.iter().enumerate() {
        if event.time >= settings.trade_to {
            break;
        }
        last = (event.time, event.block);
        engine.out.summary.events += 1;

        // An action decided earlier is carried out on the state before
        // this block's events.
        if engine
            .pending
            .as_ref()
            .is_some_and(|(due, _)| event.block >= *due)
        {
            let (_, action) = engine.pending.take().expect("checked");
            engine.execute(action, event.time, event.block)?;
        }

        // A flash loan's fee goes to the liquidity active at the price, as
        // the pool computes it: after the protocol's cut, over its active
        // liquidity, which the position joins while the price is in its
        // range. Read before the event, which changes neither.
        let mut growth = (U256::ZERO, U256::ZERO);
        if let (PoolEvent::Flash { paid0, paid1 }, Some(leg)) = (&event.event, &engine.leg) {
            if leg.is_active_at(engine.pool.tick()) {
                let (protocol0, protocol1) = engine.pool.fee_protocol();
                let active = engine.pool.liquidity();
                let lp0 = after_protocol(*paid0, protocol0);
                let lp1 = after_protocol(*paid1, protocol1);
                growth.0 = diluted_fee_growth(lp0, active, leg.liquidity);
                growth.1 = diluted_fee_growth(lp1, active, leg.liquidity);
            }
        }

        // The position takes its share of every swap step inside its range,
        // as fee growth with its own liquidity counted in.
        let range = engine.leg.as_ref().map(|l| (l.lower, l.upper, l.liquidity));
        let mut observer = |step: &StepInfo| {
            let Some((lower, upper, liquidity)) = range else {
                return;
            };
            if step.tick_start < lower || step.tick_start >= upper {
                return;
            }
            let added = diluted_fee_growth(step.lp_fee, step.liquidity, liquidity);
            if step.zero_for_one {
                growth.0 = growth.0.wrapping_add(added);
            } else {
                growth.1 = growth.1.wrapping_add(added);
            }
        };
        // With a position in place its bounds split swap steps, so input
        // amounts may differ from the event by rounding.
        let mode = if range.is_some() {
            Replay::Tolerant
        } else {
            Replay::Strict
        };
        engine
            .pool
            .apply_event(&event.event, mode, &mut observer)
            .map_err(|source| SimError::Replay {
                block: event.block,
                log_index: event.log_index,
                source,
            })?;
        if let Some(leg) = &mut engine.leg {
            leg.growth0 = leg.growth0.wrapping_add(growth.0);
            leg.growth1 = leg.growth1.wrapping_add(growth.1);
        }

        if let PoolEvent::Swap(swap) = &event.event {
            engine.buckets.push(event.time, swap.tick);
            engine.path.push(PathPoint {
                time: event.time,
                tick: swap.tick,
                volume1: to_f64(swap.amount1.unsigned_abs()),
                liquidity: swap.liquidity as f64,
            });
        }

        let block_ends = events
            .get(i + 1)
            .is_none_or(|next| next.block != event.block);
        if block_ends {
            engine.on_block_end(event.time, event.block)?;
            if engine.stopped {
                break;
            }
        }
    }

    // The run is over: whatever is open is closed on the last state.
    engine.pending = None;
    if let Some(plan) = &settings.agent {
        engine.out.unused_decisions = plan.decisions.len().saturating_sub(engine.agent_used);
    }
    if engine.leg.is_some() {
        engine.execute(
            Action::Close {
                reason: if engine.stopped {
                    "agent_pending"
                } else {
                    "window_end"
                },
            },
            last.0,
            last.1,
        )?;
    }

    let unit = engine.value_unit;
    let start = to_f64(settings.capital);
    let end = to_f64(engine.cash);
    let passes = engine.out.campaigns.iter().map(|c| c.passes).sum();
    let seconds_in_range = engine
        .out
        .campaigns
        .iter()
        .map(|c| c.seconds_in_range)
        .sum();
    let in_campaigns: u64 = engine
        .out
        .campaigns
        .iter()
        .map(|c| c.closed_time.saturating_sub(c.opened_time))
        .sum();
    let traded = last.0.saturating_sub(settings.trade_from);
    // A run that never reached its window starts and ends at the same price.
    let end_price = engine.price();
    let start_price = engine.start_price.unwrap_or(end_price);
    let start_usd = start / unit * engine.usd_per_value(start_price);
    let end_usd = end / unit * engine.usd_per_value(end_price);
    engine.out.summary = Summary {
        start_capital: start / unit,
        end_capital: end / unit,
        result_pct: (end - start) / start * 100.0,
        start_price,
        end_price,
        start_usd,
        end_usd,
        result_pct_usd: (end_usd - start_usd) / start_usd * 100.0,
        hold_value_pct_usd: (engine.usd_per_value(end_price) / engine.usd_per_value(start_price)
            - 1.0)
            * 100.0,
        fees: engine.fees_total / unit,
        gas: to_f64(engine.gas_total) / unit,
        gas_gwei: if engine.gas_units > 0.0 {
            engine.gas_wei / engine.gas_units / WEI_PER_GWEI
        } else {
            0.0
        },
        campaigns: engine.out.campaigns.len() as u32,
        passes,
        seconds_in_range,
        seconds_out_of_range: in_campaigns.saturating_sub(seconds_in_range),
        seconds_flat: traded.saturating_sub(in_campaigns),
        ..engine.out.summary
    };
    Ok(engine.out)
}
