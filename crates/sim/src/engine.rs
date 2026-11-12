//! The replay engine: one pool, one strategy, one run.
//!
//! It replays the pool's stored events through the pool replica and holds
//! the bot's position beside it as a virtual one. The position is assumed
//! small against the liquidity in range (the passive fill model): the
//! historical swaps are left unchanged and the position takes its pro-rata
//! share of each swap step's fee. Token amounts are exact integers
//! throughout; floating point is used only for decisions and reporting.
//!
//! Decisions are taken at the end of a block and carried out at a later
//! block, on the state then, so nothing is ever decided and filled on the
//! same information.

use std::collections::HashMap;

use alloy_primitives::{I256, U256};
use lpbot_core::{Campaign, EntrySide, PoolId, Tick};
use lpbot_data::lake::TimedEvent;
use lpbot_data::zone_map::{zone_stats, PathPoint, DAY};
use lpbot_risk::{LossCheck, LossLimit};
use lpbot_strategy::zone_round_trip::{choose_zone, entry_candidates, gap, ZoneScore};
use lpbot_strategy::{FixedRule, MarketView, PatienceAction, PatiencePolicy};
use lpbot_venues::uniswap_v3::events::PoolEvent;
use lpbot_venues::uniswap_v3::full_math::mul_div;
use lpbot_venues::uniswap_v3::liquidity_amounts::{
    get_amounts_for_liquidity, get_liquidity_for_amount0, get_liquidity_for_amount1,
};
use lpbot_venues::uniswap_v3::pool::{Pool, PoolError, PoolSnapshot, Replay, StepInfo};
use lpbot_venues::uniswap_v3::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use lpbot_venues::uniswap_v3::tick_math::{get_sqrt_ratio_at_tick, MAX_SQRT_RATIO, MIN_SQRT_RATIO};
use lpbot_venues::uniswap_v3::MathError;

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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GasModel {
    pub open_units: u64,
    pub close_units: u64,
    pub swap_units: u64,
    pub gas_price_gwei: f64,
    /// Smallest units of the value token that one ETH is worth.
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
            value_raw_per_eth: 0.0,
        }
    }

    fn cost(&self, units: u64) -> U256 {
        let raw = units as f64 * self.gas_price_gwei * 1e-9 * self.value_raw_per_eth;
        if raw.is_finite() && raw > 0.0 {
            U256::from(raw as u128)
        } else {
            U256::ZERO
        }
    }
}

/// Everything that defines one run.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The token results are counted in.
    pub value: Token,
    pub decimals0: u8,
    pub decimals1: u8,
    /// Width of the ranges, in ticks. A multiple of the tick spacing.
    pub width: Tick,
    /// How far from the price an entry range may sit, in ticks.
    pub max_gap: Tick,
    /// Capital of the run in the value token, smallest unit.
    pub capital: U256,
    /// Share of the free capital put into a campaign.
    pub capital_share: f64,
    pub loss_limit: LossLimit,
    pub rule: FixedRule,
    /// Seconds between patience decisions.
    pub decide_every: u64,
    /// Seconds between looks at where to enter or whether to move.
    pub check_every: u64,
    /// Blocks between a decision and its execution. At least one.
    pub latency_blocks: u64,
    pub lookbacks_days: Vec<u32>,
    /// A resting position moves only to a range crossed this many times
    /// as often as its own.
    pub move_margin: f64,
    pub gas: GasModel,
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
    pub exit_reason: String,
    pub amount_in: f64,
    /// Capital after the campaign minus capital before it: fees, losses,
    /// gas and swap costs all included.
    pub result: f64,
    pub fees: f64,
    pub gas: f64,
    /// Full passes of price through the campaign's ranges.
    pub passes: u32,
    pub flips: u32,
    pub seconds_beyond: u64,
}

/// One patience decision.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRow {
    pub campaign: u32,
    pub time: u64,
    pub block: u64,
    pub tick: Tick,
    /// `entry` or `flip`: which leg of the campaign was stranded.
    pub leg: String,
    pub seconds_stranded: u64,
    /// `wait`, `flip` or `close`.
    pub action: String,
    pub target_lower: Option<Tick>,
    pub target_upper: Option<Tick>,
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
    pub fees: f64,
    /// `flat`, `resting`, `in_range`, `beyond` or `flipped`.
    pub state: String,
    pub tick_lower: Option<Tick>,
    pub tick_upper: Option<Tick>,
}

/// What the zone map said about a range when the engine looked at it.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneLook {
    pub time: u64,
    pub lookback_days: u32,
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
    pub start_capital: f64,
    pub end_capital: f64,
    pub result_pct: f64,
    pub fees: f64,
    pub gas: f64,
    pub campaigns: u32,
    pub passes: u32,
    pub max_drawdown_pct: f64,
    pub halted_by_loss_limit: bool,
    pub events: u64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Output {
    pub campaigns: Vec<CampaignRow>,
    pub decisions: Vec<DecisionRow>,
    pub series: Vec<SeriesRow>,
    pub zones: Vec<ZoneLook>,
    pub summary: Summary,
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
    fee0: U256,
    fee1: U256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extreme {
    Value,
    Other,
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
    seconds_beyond: u64,
    extreme: Extreme,
    /// Since when the position has been wholly in the other token.
    other_since: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
enum Action {
    Open {
        lower: Tick,
        upper: Tick,
    },
    /// Withdraw and swap everything back to the value token.
    Close {
        reason: &'static str,
    },
    /// Withdraw and place the other token next to the price.
    Flip,
    /// Withdraw a position that is wholly in the value token.
    Withdraw {
        reason: &'static str,
    },
}

fn to_f64(value: U256) -> f64 {
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
    scores: HashMap<(Tick, Tick, u64), ZoneScore>,
    out: Output,
    campaigns_opened: u32,
    fees_total: f64,
    gas_total: U256,
    gas_owed_units: u64,
    peak: f64,
    value_unit: f64,
}

impl<'a> Engine<'a> {
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

    fn split(&self, amount0: U256, amount1: U256) -> (U256, U256) {
        match self.s.value {
            Token::Zero => (amount0, amount1),
            Token::One => (amount1, amount0),
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

    /// Capital plus position at the pool price, in value-token units. The
    /// other token is marked after the pool's swap fee.
    fn equity(&self) -> Result<f64, MathError> {
        let mut total = to_f64(self.cash);
        if let Some(leg) = &self.leg {
            let (value, other) = self.holdings(leg)?;
            let (fee_value, fee_other) = self.split(leg.fee0, leg.fee1);
            let keep = 1.0 - f64::from(self.pool.fee_pips()) / 1_000_000.0;
            total += to_f64(value + fee_value)
                + to_f64(other + fee_other) * self.value_per_other() * keep;
        }
        Ok(total)
    }

    /// Takes an action's gas out of the free capital. When the free capital
    /// cannot cover it, as after a flip that put everything back into the
    /// pool, it is owed and settled with the next charge.
    fn charge_gas(&mut self, units: u64) {
        let units = units + std::mem::take(&mut self.gas_owed_units);
        let cost = self.s.gas.cost(units);
        if cost > self.cash {
            self.gas_owed_units = units;
            return;
        }
        self.cash -= cost;
        self.gas_total += cost;
        if let Some(campaign) = &mut self.campaign {
            campaign.gas += cost;
        }
    }

    /// Puts `amount` of one token into a range that holds only that token
    /// at the current price. `None` when the price is not on the right
    /// side of the range or the amount buys no liquidity.
    fn open_leg(
        &mut self,
        token: Token,
        amount: U256,
        lower: Tick,
        upper: Tick,
        flip: bool,
    ) -> Result<Option<U256>, MathError> {
        let sqrt_lower = get_sqrt_ratio_at_tick(lower)?;
        let sqrt_upper = get_sqrt_ratio_at_tick(upper)?;
        let price = self.pool.sqrt_price_x96();
        let (liquidity, used) = match token {
            Token::Zero => {
                if price > sqrt_lower {
                    return Ok(None);
                }
                let liquidity = get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount)?;
                (
                    liquidity,
                    get_amount0_delta(sqrt_lower, sqrt_upper, liquidity, true)?,
                )
            }
            Token::One => {
                if price < sqrt_upper {
                    return Ok(None);
                }
                let liquidity = get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount)?;
                (
                    liquidity,
                    get_amount1_delta(sqrt_lower, sqrt_upper, liquidity, true)?,
                )
            }
        };
        if liquidity == 0 || used > amount {
            return Ok(None);
        }
        if self.pool.watch_tick(lower).is_err() || self.pool.watch_tick(upper).is_err() {
            return Ok(None);
        }
        self.leg = Some(Leg {
            flip,
            lower,
            upper,
            sqrt_lower,
            sqrt_upper,
            liquidity,
            fee0: U256::ZERO,
            fee1: U256::ZERO,
        });
        Ok(Some(used))
    }

    /// Takes the position out: (value token, other token), fees included.
    fn withdraw(&mut self) -> Result<(U256, U256), MathError> {
        let Some(leg) = self.leg.take() else {
            return Ok((U256::ZERO, U256::ZERO));
        };
        let (value, other) = self.holdings(&leg)?;
        let (fee_value, fee_other) = self.split(leg.fee0, leg.fee1);
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
        if amount_other.is_zero() {
            return Ok(U256::ZERO);
        }
        let zero_for_one = self.s.value == Token::One;
        let limit = if zero_for_one {
            MIN_SQRT_RATIO + U256::from(1u8)
        } else {
            MAX_SQRT_RATIO - U256::from(1u8)
        };
        let amount = I256::try_from(amount_other)
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
        let Some(campaign) = self.campaign.take() else {
            return;
        };
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
            amount_in: to_f64(campaign.amount_in) / unit,
            result: (to_f64(self.cash) - to_f64(campaign.cash_before)) / unit,
            fees: campaign.fees / unit,
            gas: to_f64(campaign.gas) / unit,
            passes: campaign.passes,
            flips: campaign.flips,
            seconds_beyond,
        });
    }

    fn execute(&mut self, action: Action, time: u64, block: u64) -> Result<(), SimError> {
        match action {
            Action::Open { lower, upper } => {
                if self.leg.is_some() || self.halted {
                    return Ok(());
                }
                // Gas comes out of the same capital, so it is set aside first.
                let gas = self.s.gas.cost(self.s.gas.open_units);
                if gas >= self.cash {
                    return Ok(());
                }
                let share = self.s.capital_share.clamp(0.0, 1.0);
                let free = self.cash - gas;
                let budget = U256::from((to_f64(free) * share) as u128).min(free);
                let cash_before = self.cash;
                let Some(used) = self.open_leg(self.s.value, budget, lower, upper, false)? else {
                    // Price moved onto the range in the meantime. Look again later.
                    return Ok(());
                };
                self.cash -= used;
                self.campaigns_opened += 1;
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
                    seconds_beyond: 0,
                    extreme: Extreme::Value,
                    other_since: None,
                });
                self.charge_gas(self.s.gas.open_units);
            }
            Action::Withdraw { reason } => {
                let Some(leg) = &self.leg else {
                    return Ok(());
                };
                // Only a position wholly in the value token leaves this way.
                if !self.holdings(leg)?.1.is_zero() {
                    return Ok(());
                }
                let (value, other) = self.withdraw()?;
                self.cash += value;
                self.charge_gas(self.s.gas.close_units);
                if !other.is_zero() {
                    self.cash += self.swap_to_value(other)?;
                    self.charge_gas(self.s.gas.swap_units);
                }
                self.end_campaign(reason, time, block);
            }
            Action::Close { reason } => {
                if self.leg.is_none() {
                    return Ok(());
                }
                let (value, other) = self.withdraw()?;
                self.cash += value;
                self.charge_gas(self.s.gas.close_units);
                if !other.is_zero() {
                    self.cash += self.swap_to_value(other)?;
                    self.charge_gas(self.s.gas.swap_units);
                }
                self.end_campaign(reason, time, block);
            }
            Action::Flip => {
                let Some(leg) = &self.leg else {
                    return Ok(());
                };
                // The flip goes next to the price as it is now, and only
                // while the position is still wholly in the other token.
                let (held_value, _) = self.holdings(leg)?;
                let view = self.campaign_view(leg);
                let market = MarketView {
                    current_tick: self.pool.tick(),
                    tick_spacing: self.pool.tick_spacing(),
                };
                let PatienceAction::Flip { target } = FixedRule::Flip.decide(&view, &market) else {
                    return Ok(());
                };
                if !held_value.is_zero()
                    || (target.tick_lower, target.tick_upper) == (leg.lower, leg.upper)
                {
                    return Ok(());
                }
                let (value, other) = self.withdraw()?;
                self.cash += value;
                let other_token = self.s.value.other();
                match self.open_leg(
                    other_token,
                    other,
                    target.tick_lower,
                    target.tick_upper,
                    true,
                )? {
                    Some(used) => {
                        // Dust the range could not take is swapped back.
                        self.cash += self.swap_to_value(other - used)?;
                        self.charge_gas(self.s.gas.close_units + self.s.gas.open_units);
                        if let Some(campaign) = &mut self.campaign {
                            campaign.flips += 1;
                        }
                    }
                    None => {
                        self.cash += self.swap_to_value(other)?;
                        self.charge_gas(self.s.gas.close_units + self.s.gas.swap_units);
                        self.end_campaign("patience_close", time, block);
                    }
                }
            }
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
        let day = now / DAY;
        if let Some(score) = self.scores.get(&(lower, upper, day)) {
            return *score;
        }
        let mut lookbacks = self.s.lookbacks_days.clone();
        lookbacks.sort_unstable();
        let mut score = ZoneScore {
            passes_per_day_long: 0.0,
            passes_per_day_short: 0.0,
            observed_days: 0.0,
        };
        for (i, days) in lookbacks.iter().enumerate() {
            let from = now.saturating_sub(u64::from(*days) * DAY);
            let stats = zone_stats(&self.path, lower, upper, from, now);
            if i == 0 {
                score.passes_per_day_short = stats.passes_per_day();
            }
            score.passes_per_day_long = stats.passes_per_day();
            score.observed_days = stats.observed_days();
            self.out.zones.push(ZoneLook {
                time: now,
                lookback_days: *days,
                tick_lower: lower,
                tick_upper: upper,
                observed_days: stats.observed_days(),
                passes_per_day: stats.passes_per_day(),
                time_in_range: stats.time_in_range(),
                return_odds_1d: stats.return_odds(DAY, None),
            });
        }
        self.scores.insert((lower, upper, day), score);
        score
    }

    fn best_entry(&mut self, now: u64) -> Option<(Tick, Tick)> {
        let candidates = entry_candidates(
            self.pool.tick(),
            self.pool.tick_spacing(),
            self.s.width,
            self.side(),
            self.s.max_gap,
        );
        let scores: Vec<ZoneScore> = candidates
            .iter()
            .map(|&(lower, upper)| self.score(lower, upper, now))
            .collect();
        let mut next = scores.into_iter();
        choose_zone(&candidates, |_, _| next.next().expect("one score each"))
    }

    fn schedule(&mut self, block: u64, action: Action) {
        self.pending = Some((block + self.s.latency_blocks.max(1), action));
    }

    /// Everything that happens once a block's events are applied.
    fn on_block_end(&mut self, time: u64, block: u64) -> Result<(), SimError> {
        if time < self.s.trade_from {
            return Ok(());
        }
        let tick = self.pool.tick();

        // Where the position stands, and the passes it has completed.
        let mut state = "flat";
        let mut stranded_since = None;
        let mut resting = false;
        if let Some(leg) = &self.leg {
            let (value, other) = self.holdings(leg)?;
            let flip = leg.flip;
            let campaign = self.campaign.as_mut().expect("a leg belongs to a campaign");
            if value.is_zero() && !other.is_zero() {
                if campaign.extreme == Extreme::Value {
                    campaign.extreme = Extreme::Other;
                    campaign.passes += 1;
                }
                campaign.other_since.get_or_insert(time);
                stranded_since = campaign.other_since;
                state = if flip { "flipped" } else { "beyond" };
            } else if other.is_zero() {
                if campaign.extreme == Extreme::Other {
                    campaign.extreme = Extreme::Value;
                    campaign.passes += 1;
                }
                if let Some(since) = campaign.other_since.take() {
                    campaign.seconds_beyond += time.saturating_sub(since);
                }
                resting = true;
                state = "resting";
            } else {
                state = "in_range";
            }
        }

        // The loss limit is checked on every block, before anything else.
        let equity = self.equity()?;
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
            self.out.series.push(SeriesRow {
                time,
                block,
                tick,
                price: self.price(),
                equity: equity / unit,
                hold: to_f64(self.s.capital) / unit,
                fees: (self.fees_total
                    + self.leg.as_ref().map_or(0.0, |leg| {
                        let (fee_value, fee_other) = self.split(leg.fee0, leg.fee1);
                        to_f64(fee_value) + to_f64(fee_other) * self.value_per_other()
                    }))
                    / unit,
                state: state.to_string(),
                tick_lower: self.leg.as_ref().map(|l| l.lower),
                tick_upper: self.leg.as_ref().map(|l| l.upper),
            });
            self.next_series =
                time - time % self.s.series_every.max(1) + self.s.series_every.max(1);
        }

        if self.halted || self.pending.is_some() {
            return Ok(());
        }

        let Some(leg) = self.leg.clone() else {
            // Flat: look for a range to enter.
            if time >= self.next_check {
                self.next_check = time + self.s.check_every.max(1);
                if let Some((lower, upper)) = self.best_entry(time) {
                    self.schedule(block, Action::Open { lower, upper });
                }
            }
            return Ok(());
        };

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
                let own_gap = gap(tick, leg.lower, leg.upper, self.side());
                let own = self.score(leg.lower, leg.upper, time);
                let best = self.best_entry(time);
                let move_to = match best {
                    Some(range) if range != (leg.lower, leg.upper) => {
                        let score = self.score(range.0, range.1, time);
                        let busier = score.passes_per_day_long
                            > own.passes_per_day_long * self.s.move_margin
                            && score.passes_per_day_long > 0.0;
                        (own_gap > self.s.max_gap.max(self.s.width) || busier).then_some(range)
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
            if leg.flip && gap(tick, leg.lower, leg.upper, other_side) < self.s.width {
                return Ok(());
            }
            let market = MarketView {
                current_tick: tick,
                tick_spacing: self.pool.tick_spacing(),
            };
            let action = self.s.rule.decide(&self.campaign_view(&leg), &market);
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
            });
            match action {
                PatienceAction::Wait => {}
                PatienceAction::Flip { .. } => self.schedule(block, Action::Flip),
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
    if settings.lookbacks_days.is_empty() {
        return Err(SimError::Settings("the zone lookbacks are empty".into()));
    }
    let value_decimals = match settings.value {
        Token::Zero => settings.decimals0,
        Token::One => settings.decimals1,
    };

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
        out: Output::default(),
        campaigns_opened: 0,
        fees_total: 0.0,
        gas_total: U256::ZERO,
        gas_owed_units: 0,
        peak: to_f64(settings.capital),
        value_unit: 10f64.powi(i32::from(value_decimals)),
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

        // The position takes its share of every swap step inside its range.
        let mut earned = (U256::ZERO, U256::ZERO);
        let range = engine.leg.as_ref().map(|l| (l.lower, l.upper, l.liquidity));
        let mut observer = |step: &StepInfo| {
            let Some((lower, upper, liquidity)) = range else {
                return;
            };
            if step.liquidity == 0 || step.tick_start < lower || step.tick_start >= upper {
                return;
            }
            let share = mul_div(
                step.lp_fee,
                U256::from(liquidity),
                U256::from(step.liquidity),
            )
            .unwrap_or(U256::ZERO);
            if step.zero_for_one {
                earned.0 += share;
            } else {
                earned.1 += share;
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
            leg.fee0 += earned.0;
            leg.fee1 += earned.1;
        }

        if let PoolEvent::Swap(swap) = &event.event {
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
        }
    }

    // The run is over: whatever is open is closed on the last state.
    engine.pending = None;
    if engine.leg.is_some() {
        engine.execute(
            Action::Close {
                reason: "window_end",
            },
            last.0,
            last.1,
        )?;
    }

    let unit = engine.value_unit;
    let start = to_f64(settings.capital);
    let end = to_f64(engine.cash);
    let passes = engine.out.campaigns.iter().map(|c| c.passes).sum();
    engine.out.summary = Summary {
        start_capital: start / unit,
        end_capital: end / unit,
        result_pct: (end - start) / start * 100.0,
        fees: engine.fees_total / unit,
        gas: to_f64(engine.gas_total) / unit,
        campaigns: engine.out.campaigns.len() as u32,
        passes,
        ..engine.out.summary
    };
    Ok(engine.out)
}
