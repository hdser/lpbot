//! In-memory Uniswap v3 pool state.
//!
//! This is the pool-state replica: it holds what the contract holds (price,
//! tick, active liquidity, the tick map, fee growth) and changes it the way
//! the contract does. It is used in three ways:
//!
//! * `swap`, `mint` and `burn` are ports of the contract functions. They
//!   answer "what would this action do right now".
//! * `apply_swap_event` replays a historical `Swap` event. An event carries
//!   the final price and the amounts but not the caller's inputs, so the
//!   replay walks to the event's price and recovers the last step's fee from
//!   the event's input amount.
//! * A `SwapObserver` sees every step of a swap. The simulator uses it to
//!   give a virtual position its share of each step's fee.
//!
//! Ticks can also be *watched*: a watched tick splits swap steps at its price
//! without holding liquidity, so a virtual position's range boundaries line
//! up with step boundaries. A pool with watched ticks follows the same price
//! path, with input amounts that can differ from history by a few units of
//! rounding; the strict replica never has watched ticks. Watching never
//! changes what the contract would hold:
//!
//! * During a swap the replica walks as if watched ticks were initialized.
//!   At the end it puts the tick where the contract has it (`settle_tick`),
//!   so price, tick and active liquidity end equal to the contract's.
//! * Ticks that hold liquidity keep the contract's fee growth outside them.
//!   A watched tick keeps a second record of its own (`TickState`), flipped
//!   every time the replica's tick passes it, settling included.
//!   `fee_growth_inside` uses that record for a range bounded by a watched
//!   tick, so it agrees with what an observer credits step by step, also
//!   across real mints and burns on the tick.
//!
//! Every state change is atomic: when it returns an error, the pool is as it
//! was. A swap hands its steps to the observer only once it has succeeded.

use std::collections::BTreeMap;

use alloy_primitives::{I256, U256};
use thiserror::Error;

use super::full_math::{mul_div, mul_div_rounding_up};
use super::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use super::swap_math::compute_swap_step;
use super::tick_math::{
    get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO,
    MIN_TICK,
};
use super::{add_liquidity_delta, MathError, Q128};

/// State the contract keeps per initialized tick. For a tick that is only
/// watched, every field but `watchers` is zero: the contract has no such
/// tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickInfo {
    /// Total liquidity that references this tick.
    pub liquidity_gross: u128,
    /// Liquidity added when the tick is crossed left to right.
    pub liquidity_net: i128,
    pub fee_growth_outside0: U256,
    pub fee_growth_outside1: U256,
    /// Number of virtual positions that use this tick as a step boundary.
    pub watchers: u32,
}

impl TickInfo {
    fn is_boundary(&self) -> bool {
        self.liquidity_gross > 0 || self.watchers > 0
    }
}

/// A tick in the replica's map: what the contract holds for it, and, while
/// the tick is watched, the replica's own fee growth outside it.
///
/// The contract flips `fee_growth_outside` only when it crosses an
/// initialized tick, and starts it over whenever the tick is initialized
/// again. A virtual position needs a record that lasts as long as its watch
/// and follows the replica's tick, which also passes watched ticks the
/// contract never crosses. So a watched tick keeps one apart: it starts by
/// the contract's convention (all growth so far happened below the tick)
/// when the first watch is placed, and flips every time the replica's tick
/// moves across the tick, whether by a swap step or by `settle_tick`. It
/// never touches contract state, and it is zero while the tick is not
/// watched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TickState {
    info: TickInfo,
    watched_outside0: U256,
    watched_outside1: U256,
}

impl TickState {
    /// Flips the watched record: the replica's tick moved across this tick.
    fn flip_watched(&mut self, fee_growth_global0: U256, fee_growth_global1: U256) {
        if self.info.watchers > 0 {
            self.watched_outside0 = fee_growth_global0.wrapping_sub(self.watched_outside0);
            self.watched_outside1 = fee_growth_global1.wrapping_sub(self.watched_outside1);
        }
    }

    fn is_watched_only(&self) -> bool {
        self.info.liquidity_gross == 0 && self.info.watchers > 0
    }
}

/// One step of a swap, as seen by an observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepInfo {
    pub zero_for_one: bool,
    /// The pool's tick when the step started. A position is active during the
    /// step exactly when `tick_lower <= tick_start < tick_upper`.
    pub tick_start: i32,
    pub sqrt_price_start: U256,
    pub sqrt_price_end: U256,
    /// Liquidity that was active during the step.
    pub liquidity: u128,
    pub amount_in: U256,
    pub amount_out: U256,
    /// Fee that went to liquidity providers in this step, in the input token.
    pub lp_fee: U256,
    /// Fee that went to the protocol in this step, in the input token.
    pub protocol_fee: U256,
}

/// Receives every step of a swap, once the swap has succeeded.
pub trait SwapObserver {
    fn on_step(&mut self, step: &StepInfo);
}

/// The observer that ignores everything.
impl SwapObserver for () {
    fn on_step(&mut self, _step: &StepInfo) {}
}

impl<F: FnMut(&StepInfo)> SwapObserver for F {
    fn on_step(&mut self, step: &StepInfo) {
        self(step)
    }
}

/// Signed token amounts of a swap from the pool's point of view: positive
/// amounts are paid to the pool, negative amounts are paid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapAmounts {
    pub amount0: I256,
    pub amount1: I256,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PoolError {
    #[error(transparent)]
    Math(#[from] MathError),
    #[error("the pool is not initialized")]
    NotInitialized,
    #[error("the pool is already initialized")]
    AlreadyInitialized,
    #[error("ticks must satisfy MIN_TICK <= lower < upper <= MAX_TICK")]
    BadTickRange,
    #[error("tick {0} is not a multiple of the tick spacing")]
    TickNotSpaced(i32),
    #[error("liquidity per tick would exceed the maximum")]
    TickLiquidityOverflow,
    #[error("the swap amount must not be zero")]
    ZeroAmount,
    #[error("the price limit is on the wrong side of the current price or out of bounds")]
    BadPriceLimit,
    #[error("a flash needs active liquidity")]
    NoLiquidity,
    #[error("replay mismatch: {0}")]
    ReplayMismatch(String),
}

pub type PoolResult<T> = Result<T, PoolError>;

/// A `Swap` event as the pool emits it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapEvent {
    pub amount0: I256,
    pub amount1: I256,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
}

/// How strictly a replayed event must match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replay {
    /// Every figure must match the event exactly. For the replica.
    Strict,
    /// The price path must match; input amounts may differ by rounding.
    /// For pools with watched ticks.
    Tolerant,
}

/// Pool state as read from the chain at one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolSnapshot {
    pub fee_pips: u32,
    pub tick_spacing: i32,
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
    pub fee_growth_global0: U256,
    pub fee_growth_global1: U256,
    pub fee_protocol0: u8,
    pub fee_protocol1: u8,
    /// Every initialized tick with its state.
    pub ticks: Vec<(i32, TickInfo)>,
}

/// A mint or burn worked out against the current state, not yet applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PositionChange {
    /// The two ticks as they will be, or nothing for a zero change.
    ticks: Option<[(i32, TickState); 2]>,
    liquidity: u128,
    /// Owed to the pool for a mint, to the owner for a burn.
    pub(super) amounts: (U256, U256),
}

/// What a swap has done so far: the steps for the observer, and the state
/// before the swap so that a swap that fails can be undone.
struct SwapRecord {
    steps: Vec<StepInfo>,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    fee_growth_global0: U256,
    fee_growth_global1: U256,
    /// Ticks as they were before the swap changed them, oldest change first.
    ticks: Vec<(i32, TickState)>,
}

/// The pool-state replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pool {
    fee_pips: u32,
    tick_spacing: i32,
    max_liquidity_per_tick: u128,
    initialized: bool,
    sqrt_price_x96: U256,
    tick: i32,
    liquidity: u128,
    fee_growth_global0: U256,
    fee_growth_global1: U256,
    /// Protocol's share of fees as a denominator (0 means off), per input token.
    fee_protocol0: u8,
    fee_protocol1: u8,
    /// Every tick that holds liquidity or is watched, and no other.
    ticks: BTreeMap<i32, TickState>,
}

impl Pool {
    /// A pool that has been created but not initialized.
    pub fn new(fee_pips: u32, tick_spacing: i32) -> Self {
        assert!(tick_spacing > 0, "tick spacing must be positive");
        assert!(fee_pips < 1_000_000, "fee must be below 100%");
        Self {
            fee_pips,
            tick_spacing,
            max_liquidity_per_tick: tick_spacing_to_max_liquidity_per_tick(tick_spacing),
            initialized: false,
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            liquidity: 0,
            fee_growth_global0: U256::ZERO,
            fee_growth_global1: U256::ZERO,
            fee_protocol0: 0,
            fee_protocol1: 0,
            ticks: BTreeMap::new(),
        }
    }

    /// Rebuilds a pool from state read off the chain at one block.
    pub fn from_snapshot(snapshot: PoolSnapshot) -> Self {
        let mut pool = Self::new(snapshot.fee_pips, snapshot.tick_spacing);
        pool.initialized = true;
        pool.sqrt_price_x96 = snapshot.sqrt_price_x96;
        pool.tick = snapshot.tick;
        pool.liquidity = snapshot.liquidity;
        pool.fee_growth_global0 = snapshot.fee_growth_global0;
        pool.fee_growth_global1 = snapshot.fee_growth_global1;
        pool.fee_protocol0 = snapshot.fee_protocol0;
        pool.fee_protocol1 = snapshot.fee_protocol1;
        pool.ticks = snapshot
            .ticks
            .into_iter()
            .filter(|(_, info)| info.liquidity_gross > 0)
            .map(|(tick, info)| {
                (
                    tick,
                    TickState {
                        info: TickInfo {
                            watchers: 0,
                            ..info
                        },
                        ..TickState::default()
                    },
                )
            })
            .collect();
        pool
    }

    /// The pool's state in the form `from_snapshot` reads. Watched ticks
    /// without liquidity are left out, as they are not pool state.
    pub fn snapshot(&self) -> PoolSnapshot {
        PoolSnapshot {
            fee_pips: self.fee_pips,
            tick_spacing: self.tick_spacing,
            sqrt_price_x96: self.sqrt_price_x96,
            tick: self.tick,
            liquidity: self.liquidity,
            fee_growth_global0: self.fee_growth_global0,
            fee_growth_global1: self.fee_growth_global1,
            fee_protocol0: self.fee_protocol0,
            fee_protocol1: self.fee_protocol1,
            ticks: self.contract_ticks().into_iter().collect(),
        }
    }

    /// The ticks the contract holds, as it holds them.
    fn contract_ticks(&self) -> BTreeMap<i32, TickInfo> {
        self.ticks
            .iter()
            .filter(|(_, state)| state.info.liquidity_gross > 0)
            .map(|(tick, state)| {
                (
                    *tick,
                    TickInfo {
                        watchers: 0,
                        ..state.info
                    },
                )
            })
            .collect()
    }

    /// Lists every way this pool differs from another, in plain words.
    /// Watched ticks without liquidity are ignored. Empty means identical.
    pub fn differences(&self, other: &Pool) -> Vec<String> {
        let mut out = Vec::new();
        let mut field = |name: &str, a: String, b: String| {
            if a != b {
                out.push(format!("{name}: {a} vs {b}"));
            }
        };
        field("fee", self.fee_pips.to_string(), other.fee_pips.to_string());
        field(
            "tick spacing",
            self.tick_spacing.to_string(),
            other.tick_spacing.to_string(),
        );
        field(
            "sqrt price",
            self.sqrt_price_x96.to_string(),
            other.sqrt_price_x96.to_string(),
        );
        field("tick", self.tick.to_string(), other.tick.to_string());
        field(
            "liquidity",
            self.liquidity.to_string(),
            other.liquidity.to_string(),
        );
        field(
            "fee growth global 0",
            self.fee_growth_global0.to_string(),
            other.fee_growth_global0.to_string(),
        );
        field(
            "fee growth global 1",
            self.fee_growth_global1.to_string(),
            other.fee_growth_global1.to_string(),
        );
        field(
            "protocol fee",
            format!("{}/{}", self.fee_protocol0, self.fee_protocol1),
            format!("{}/{}", other.fee_protocol0, other.fee_protocol1),
        );

        let (mine, theirs) = (self.contract_ticks(), other.contract_ticks());
        let all_ticks: std::collections::BTreeSet<i32> =
            mine.keys().chain(theirs.keys()).copied().collect();
        for tick in all_ticks {
            match (mine.get(&tick), theirs.get(&tick)) {
                (Some(a), Some(b)) if a != b => out.push(format!(
                    "tick {tick}: gross {} net {} outside {}/{} vs gross {} net {} outside {}/{}",
                    a.liquidity_gross,
                    a.liquidity_net,
                    a.fee_growth_outside0,
                    a.fee_growth_outside1,
                    b.liquidity_gross,
                    b.liquidity_net,
                    b.fee_growth_outside0,
                    b.fee_growth_outside1
                )),
                (Some(_), None) => out.push(format!("tick {tick}: only in the first pool")),
                (None, Some(_)) => out.push(format!("tick {tick}: only in the second pool")),
                _ => {}
            }
        }
        out
    }

    // --- read access ------------------------------------------------------

    pub fn fee_pips(&self) -> u32 {
        self.fee_pips
    }
    pub fn tick_spacing(&self) -> i32 {
        self.tick_spacing
    }
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
    pub fn sqrt_price_x96(&self) -> U256 {
        self.sqrt_price_x96
    }
    pub fn tick(&self) -> i32 {
        self.tick
    }
    /// Liquidity that is in range at the current price.
    pub fn liquidity(&self) -> u128 {
        self.liquidity
    }
    pub fn fee_growth_global(&self) -> (U256, U256) {
        (self.fee_growth_global0, self.fee_growth_global1)
    }
    pub fn fee_protocol(&self) -> (u8, u8) {
        (self.fee_protocol0, self.fee_protocol1)
    }
    pub fn tick_info(&self, tick: i32) -> Option<&TickInfo> {
        self.ticks.get(&tick).map(|state| &state.info)
    }
    /// All ticks that hold liquidity or are watched, in ascending order.
    pub fn ticks(&self) -> impl Iterator<Item = (i32, &TickInfo)> {
        self.ticks.iter().map(|(t, state)| (*t, &state.info))
    }

    /// Fee growth per unit of liquidity inside a range, for both tokens.
    /// Differences of this value over time are what a position earned.
    ///
    /// A bound that is watched uses the tick's own record (see `TickState`)
    /// in place of the contract's, so differences stay right for a virtual
    /// position across swaps that end on its bounds and across real mints
    /// and burns on them. Take differences only while the bound stays
    /// watched: a new watch starts a new record.
    pub fn fee_growth_inside(&self, tick_lower: i32, tick_upper: i32) -> (U256, U256) {
        let (lower0, lower1) = self.fee_growth_outside_for_ranges(tick_lower);
        let (upper0, upper1) = self.fee_growth_outside_for_ranges(tick_upper);
        let (g0, g1) = (self.fee_growth_global0, self.fee_growth_global1);

        let (below0, below1) = if self.tick >= tick_lower {
            (lower0, lower1)
        } else {
            (g0.wrapping_sub(lower0), g1.wrapping_sub(lower1))
        };
        let (above0, above1) = if self.tick < tick_upper {
            (upper0, upper1)
        } else {
            (g0.wrapping_sub(upper0), g1.wrapping_sub(upper1))
        };
        (
            g0.wrapping_sub(below0).wrapping_sub(above0),
            g1.wrapping_sub(below1).wrapping_sub(above1),
        )
    }

    /// Fee growth outside a tick as range calculations read it: the watched
    /// record for a watched tick, the contract's for any other.
    fn fee_growth_outside_for_ranges(&self, tick: i32) -> (U256, U256) {
        match self.ticks.get(&tick) {
            Some(state) if state.info.watchers > 0 => {
                (state.watched_outside0, state.watched_outside1)
            }
            Some(state) => (
                state.info.fee_growth_outside0,
                state.info.fee_growth_outside1,
            ),
            None => (U256::ZERO, U256::ZERO),
        }
    }

    // --- state changes ----------------------------------------------------

    /// Sets the starting price (the `Initialize` event).
    pub fn initialize(&mut self, sqrt_price_x96: U256) -> PoolResult<()> {
        if self.initialized {
            return Err(PoolError::AlreadyInitialized);
        }
        self.tick = get_tick_at_sqrt_ratio(sqrt_price_x96)?;
        self.sqrt_price_x96 = sqrt_price_x96;
        self.initialized = true;
        Ok(())
    }

    /// Sets the protocol's share of fees (the `SetFeeProtocol` event).
    /// Each value is a denominator: 0 is off, otherwise 4 to 10.
    pub fn set_fee_protocol(&mut self, fee_protocol0: u8, fee_protocol1: u8) {
        self.fee_protocol0 = fee_protocol0;
        self.fee_protocol1 = fee_protocol1;
    }

    /// Adds liquidity to a range (the `Mint` event). Returns the token
    /// amounts owed to the pool, rounded up as the contract does.
    pub fn mint(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    ) -> PoolResult<(U256, U256)> {
        let change = self.plan_mint(tick_lower, tick_upper, amount)?;
        let amounts = change.amounts;
        self.apply_position_change(change);
        Ok(amounts)
    }

    /// Removes liquidity from a range (the `Burn` event). Returns the token
    /// amounts owed to the owner, rounded down as the contract does.
    pub fn burn(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    ) -> PoolResult<(U256, U256)> {
        let change = self.plan_burn(tick_lower, tick_upper, amount)?;
        let amounts = change.amounts;
        self.apply_position_change(change);
        Ok(amounts)
    }

    /// Works out a mint without applying it.
    pub(super) fn plan_mint(
        &self,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    ) -> PoolResult<PositionChange> {
        let delta = i128::try_from(amount).map_err(|_| MathError::LiquidityDelta)?;
        self.plan_position_change(tick_lower, tick_upper, delta)
    }

    /// Works out a burn without applying it.
    pub(super) fn plan_burn(
        &self,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
    ) -> PoolResult<PositionChange> {
        let delta = i128::try_from(amount).map_err(|_| MathError::LiquidityDelta)?;
        self.plan_position_change(tick_lower, tick_upper, -delta)
    }

    /// The contract's `_modifyPosition`, worked out against the current
    /// state. Everything that can fail fails here, before anything changes.
    fn plan_position_change(
        &self,
        tick_lower: i32,
        tick_upper: i32,
        delta: i128,
    ) -> PoolResult<PositionChange> {
        self.require_initialized()?;
        self.check_ticks(tick_lower, tick_upper)?;

        let ticks = if delta != 0 {
            Some([
                (tick_lower, self.updated_tick(tick_lower, delta, false)?),
                (tick_upper, self.updated_tick(tick_upper, delta, true)?),
            ])
        } else {
            None
        };

        let magnitude = delta.unsigned_abs();
        let round_up = delta > 0;
        let sqrt_lower = get_sqrt_ratio_at_tick(tick_lower)?;
        let sqrt_upper = get_sqrt_ratio_at_tick(tick_upper)?;

        let mut liquidity = self.liquidity;
        let amounts = if self.tick < tick_lower {
            // Below the range: all token0.
            (
                get_amount0_delta(sqrt_lower, sqrt_upper, magnitude, round_up)?,
                U256::ZERO,
            )
        } else if self.tick < tick_upper {
            liquidity = add_liquidity_delta(liquidity, delta)?;
            (
                get_amount0_delta(self.sqrt_price_x96, sqrt_upper, magnitude, round_up)?,
                get_amount1_delta(sqrt_lower, self.sqrt_price_x96, magnitude, round_up)?,
            )
        } else {
            // Above the range: all token1.
            (
                U256::ZERO,
                get_amount1_delta(sqrt_lower, sqrt_upper, magnitude, round_up)?,
            )
        };

        Ok(PositionChange {
            ticks,
            liquidity,
            amounts,
        })
    }

    /// Applies a change that `plan_mint` or `plan_burn` worked out against
    /// the pool as it still is. Cannot fail.
    pub(super) fn apply_position_change(&mut self, change: PositionChange) {
        for (tick, state) in change.ticks.into_iter().flatten() {
            if state.info.liquidity_gross > 0 {
                self.ticks.insert(tick, state);
            } else if state.info.watchers > 0 {
                // The contract deletes the tick; keep only the watch and its
                // record.
                self.ticks.insert(
                    tick,
                    TickState {
                        info: TickInfo {
                            watchers: state.info.watchers,
                            ..TickInfo::default()
                        },
                        ..state
                    },
                );
            } else {
                self.ticks.remove(&tick);
            }
        }
        self.liquidity = change.liquidity;
    }

    /// The contract's `Tick.update`: what a tick becomes when a position
    /// that uses it changes by `delta`.
    fn updated_tick(&self, tick: i32, delta: i128, upper: bool) -> PoolResult<TickState> {
        let mut state = self.ticks.get(&tick).copied().unwrap_or_default();
        let info = &mut state.info;

        let gross_before = info.liquidity_gross;
        let gross_after = add_liquidity_delta(gross_before, delta)?;
        if gross_after > self.max_liquidity_per_tick {
            return Err(PoolError::TickLiquidityOverflow);
        }
        if gross_before == 0 {
            // By convention all growth before a tick was initialized happened
            // below it. A watched tick's own record is kept apart and goes on
            // unchanged.
            let below = tick <= self.tick;
            info.fee_growth_outside0 = if below {
                self.fee_growth_global0
            } else {
                U256::ZERO
            };
            info.fee_growth_outside1 = if below {
                self.fee_growth_global1
            } else {
                U256::ZERO
            };
        }
        info.liquidity_gross = gross_after;
        info.liquidity_net = if upper {
            info.liquidity_net.checked_sub(delta)
        } else {
            info.liquidity_net.checked_add(delta)
        }
        .ok_or(MathError::LiquidityDelta)?;
        Ok(state)
    }

    /// Makes a tick a step boundary without adding liquidity.
    pub fn watch_tick(&mut self, tick: i32) -> PoolResult<()> {
        self.check_tick(tick)?;
        let below = tick <= self.tick;
        let (g0, g1) = (self.fee_growth_global0, self.fee_growth_global1);
        let state = self.ticks.entry(tick).or_default();
        if state.info.watchers == 0 {
            // The first watch starts the tick's own record, by the same
            // convention the contract uses for a new tick.
            state.watched_outside0 = if below { g0 } else { U256::ZERO };
            state.watched_outside1 = if below { g1 } else { U256::ZERO };
        }
        state.info.watchers += 1;
        Ok(())
    }

    /// Releases one watch on a tick.
    pub fn unwatch_tick(&mut self, tick: i32) {
        if let Some(state) = self.ticks.get_mut(&tick) {
            state.info.watchers = state.info.watchers.saturating_sub(1);
            if state.info.watchers == 0 {
                state.watched_outside0 = U256::ZERO;
                state.watched_outside1 = U256::ZERO;
            }
            if !state.info.is_boundary() {
                self.ticks.remove(&tick);
            }
        }
    }

    /// Credits flash-loan fees to liquidity providers (the `Flash` event).
    pub fn flash(&mut self, paid0: U256, paid1: U256) -> PoolResult<()> {
        self.require_initialized()?;
        if self.liquidity == 0 {
            return Err(PoolError::NoLiquidity);
        }
        let liquidity = U256::from(self.liquidity);
        let growth = |paid: U256, fee_protocol: u8| -> PoolResult<U256> {
            if paid.is_zero() {
                return Ok(U256::ZERO);
            }
            let protocol = protocol_cut(paid, fee_protocol);
            Ok(mul_div(paid - protocol, Q128, liquidity)?)
        };
        let growth0 = growth(paid0, self.fee_protocol0)?;
        let growth1 = growth(paid1, self.fee_protocol1)?;
        self.fee_growth_global0 = self.fee_growth_global0.wrapping_add(growth0);
        self.fee_growth_global1 = self.fee_growth_global1.wrapping_add(growth1);
        Ok(())
    }

    /// Executes a swap exactly as the contract would.
    ///
    /// `amount_specified` is positive for exact input, negative for exact
    /// output. The swap stops at `sqrt_price_limit_x96` if it gets there.
    pub fn swap(
        &mut self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x96: U256,
        observer: &mut impl SwapObserver,
    ) -> PoolResult<SwapAmounts> {
        let mut record = self.begin_swap();
        let result = self.run_swap(
            zero_for_one,
            amount_specified,
            sqrt_price_limit_x96,
            &mut record,
        );
        self.end_swap(record, result, observer)
    }

    fn run_swap(
        &mut self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x96: U256,
        record: &mut SwapRecord,
    ) -> PoolResult<SwapAmounts> {
        self.require_initialized()?;
        if amount_specified.is_zero() {
            return Err(PoolError::ZeroAmount);
        }
        let limit_ok = if zero_for_one {
            sqrt_price_limit_x96 < self.sqrt_price_x96 && sqrt_price_limit_x96 > MIN_SQRT_RATIO
        } else {
            sqrt_price_limit_x96 > self.sqrt_price_x96 && sqrt_price_limit_x96 < MAX_SQRT_RATIO
        };
        if !limit_ok {
            return Err(PoolError::BadPriceLimit);
        }

        let exact_input = amount_specified.is_positive();
        let mut remaining = amount_specified;
        let mut calculated = I256::ZERO;

        while !remaining.is_zero() && self.sqrt_price_x96 != sqrt_price_limit_x96 {
            let (tick_next, sqrt_price_next) = self.next_boundary(zero_for_one)?;
            let target = cap_target(zero_for_one, sqrt_price_next, sqrt_price_limit_x96);
            let step = compute_swap_step(
                self.sqrt_price_x96,
                target,
                self.liquidity,
                remaining,
                self.fee_pips,
            )?;

            let paid_in = to_i256(step.amount_in + step.fee_amount)?;
            let paid_out = to_i256(step.amount_out)?;
            if exact_input {
                remaining -= paid_in;
                calculated -= paid_out;
            } else {
                remaining += paid_out;
                calculated += paid_in;
            }

            self.finish_step(
                zero_for_one,
                tick_next,
                sqrt_price_next,
                step.sqrt_ratio_next,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
                record,
            )?;
        }

        self.settle_tick(zero_for_one, record)?;

        let specified_used = amount_specified - remaining;
        Ok(if zero_for_one == exact_input {
            SwapAmounts {
                amount0: specified_used,
                amount1: calculated,
            }
        } else {
            SwapAmounts {
                amount0: calculated,
                amount1: specified_used,
            }
        })
    }

    /// Replays a historical `Swap` event.
    ///
    /// The pool walks to the event's price. Every step except the last is
    /// fully determined by the tick map. The last step's fee is whatever is
    /// left of the event's input amount, which is how the contract computes
    /// it too. The resulting tick and liquidity must equal the event's, and in
    /// strict mode the steps must account for the event's amounts. The last
    /// step's `amount_out` is reported before the contract's exact-output
    /// cap, so it can exceed what was actually paid out.
    pub fn apply_swap_event(
        &mut self,
        event: &SwapEvent,
        mode: Replay,
        observer: &mut impl SwapObserver,
    ) -> PoolResult<()> {
        let mut record = self.begin_swap();
        let result = self.run_swap_event(event, mode, &mut record);
        self.end_swap(record, result, observer)
    }

    fn run_swap_event(
        &mut self,
        event: &SwapEvent,
        mode: Replay,
        record: &mut SwapRecord,
    ) -> PoolResult<()> {
        self.require_initialized()?;

        // The input token is the one with a positive amount. A swap through
        // empty ranges moves the price with both amounts at zero; then the
        // direction is read from the price.
        let zero_for_one = if event.amount0.is_positive() {
            true
        } else if event.amount1.is_positive() {
            false
        } else {
            event.sqrt_price_x96 < self.sqrt_price_x96
        };
        let (input, output) = if zero_for_one {
            (event.amount0, event.amount1)
        } else {
            (event.amount1, event.amount0)
        };
        let input_total = input.unsigned_abs();
        let output_total = output.unsigned_abs();
        let target_price = event.sqrt_price_x96;

        let moves_the_right_way = if zero_for_one {
            target_price <= self.sqrt_price_x96 && target_price >= MIN_SQRT_RATIO
        } else {
            target_price >= self.sqrt_price_x96 && target_price < MAX_SQRT_RATIO
        };
        if !moves_the_right_way {
            return Err(PoolError::ReplayMismatch(format!(
                "event price {target_price} is on the wrong side of {}",
                self.sqrt_price_x96
            )));
        }

        // An exact-input amount that can never run out, so that every step
        // goes all the way to its target.
        let unlimited = I256::MAX;
        let mut consumed = U256::ZERO;
        let mut produced = U256::ZERO;

        // When the price sits exactly on a step boundary, the contract's first
        // step has zero length: it crosses the boundary tick and moves the
        // tick index without trading. Every swap starts with a step, so this
        // happens even when the price does not move at all.
        let (tick_next, sqrt_price_next) = self.next_boundary(zero_for_one)?;
        if sqrt_price_next == self.sqrt_price_x96 {
            self.finish_step(
                zero_for_one,
                tick_next,
                sqrt_price_next,
                sqrt_price_next,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                record,
            )?;
        }

        while self.sqrt_price_x96 != target_price {
            let (tick_next, sqrt_price_next) = self.next_boundary(zero_for_one)?;
            let target = cap_target(zero_for_one, sqrt_price_next, target_price);
            let step = compute_swap_step(
                self.sqrt_price_x96,
                target,
                self.liquidity,
                unlimited,
                self.fee_pips,
            )?;
            debug_assert_eq!(step.sqrt_ratio_next, target);

            let ends_here = step.sqrt_ratio_next == target_price;
            let ends_inside_a_range = ends_here && step.sqrt_ratio_next != sqrt_price_next;
            let fee_amount = if ends_inside_a_range {
                // The swap ran out of input here: the rest of it is the fee.
                let spent = consumed + step.amount_in;
                match (input_total.checked_sub(spent), mode) {
                    (Some(rest), _) => rest,
                    (None, Replay::Tolerant) => U256::ZERO,
                    (None, Replay::Strict) => {
                        return Err(PoolError::ReplayMismatch(format!(
                            "steps consumed {spent} but the event's input is {input_total}"
                        )))
                    }
                }
            } else {
                step.fee_amount
            };

            consumed += step.amount_in + fee_amount;
            produced += step.amount_out;

            self.finish_step(
                zero_for_one,
                tick_next,
                sqrt_price_next,
                step.sqrt_ratio_next,
                step.amount_in,
                step.amount_out,
                fee_amount,
                record,
            )?;
        }

        // Input left over after the price stopped moving was taken as fee
        // by a final step that swapped nothing.
        match input_total.checked_sub(consumed) {
            Some(dust) if !dust.is_zero() => {
                let price = self.sqrt_price_x96;
                self.account_step(
                    zero_for_one,
                    price,
                    price,
                    U256::ZERO,
                    U256::ZERO,
                    dust,
                    record,
                )?;
            }
            Some(_) => {}
            None if mode == Replay::Tolerant => {}
            None => {
                return Err(PoolError::ReplayMismatch(format!(
                    "steps consumed {consumed} but the event's input is {input_total}"
                )))
            }
        }

        self.settle_tick(zero_for_one, record)?;

        if self.tick != event.tick || self.liquidity != event.liquidity {
            return Err(PoolError::ReplayMismatch(format!(
                "ended at tick {} with liquidity {}, event says tick {} with liquidity {}",
                self.tick, self.liquidity, event.tick, event.liquidity
            )));
        }
        // An exact-output swap caps what it hands out at the requested
        // amount, so the steps may produce more than the event shows, never less.
        if mode == Replay::Strict && produced < output_total {
            return Err(PoolError::ReplayMismatch(format!(
                "steps produced {produced} but the event's output is {output_total}"
            )));
        }
        Ok(())
    }

    // --- swap internals ---------------------------------------------------

    fn begin_swap(&self) -> SwapRecord {
        SwapRecord {
            steps: Vec::new(),
            sqrt_price_x96: self.sqrt_price_x96,
            tick: self.tick,
            liquidity: self.liquidity,
            fee_growth_global0: self.fee_growth_global0,
            fee_growth_global1: self.fee_growth_global1,
            ticks: Vec::new(),
        }
    }

    /// Hands a successful swap's steps to the observer, or puts back the
    /// state from before a failed one. A swap never adds or removes ticks,
    /// it only changes the ones it crosses.
    fn end_swap<T>(
        &mut self,
        record: SwapRecord,
        result: PoolResult<T>,
        observer: &mut impl SwapObserver,
    ) -> PoolResult<T> {
        match result {
            Ok(value) => {
                for step in &record.steps {
                    observer.on_step(step);
                }
                Ok(value)
            }
            Err(error) => {
                self.sqrt_price_x96 = record.sqrt_price_x96;
                self.tick = record.tick;
                self.liquidity = record.liquidity;
                self.fee_growth_global0 = record.fee_growth_global0;
                self.fee_growth_global1 = record.fee_growth_global1;
                for (tick, state) in record.ticks.into_iter().rev() {
                    self.ticks.insert(tick, state);
                }
                Err(error)
            }
        }
    }

    /// Puts the tick where the contract has it at the end of a swap.
    ///
    /// The replica walks as if watched ticks were initialized: a step that
    /// reaches a watched tick going down leaves the tick one below it, and a
    /// swap that starts exactly on a watched tick takes a zero-length first
    /// step across it. The contract has no such tick, so its tick can end
    /// one away from the replica's, across a watched tick that holds no
    /// liquidity. Moving the tick back across that tick changes no liquidity
    /// and no contract state; it flips the tick's watched record, so that
    /// `fee_growth_inside` keeps agreeing with the steps the observer saw.
    fn settle_tick(&mut self, zero_for_one: bool, record: &mut SwapRecord) -> PoolResult<()> {
        let tick =
            self.contract_tick_after_swap(zero_for_one, record.sqrt_price_x96, record.tick)?;
        if tick == self.tick {
            return Ok(());
        }
        // Ticks that hold liquidity and word edges are boundaries for both,
        // so the two can only differ by one, across a watched-only tick. If
        // that ever fails, refuse the swap rather than diverge from the
        // contract (the debug assertion makes the tests catch it).
        let across = tick.max(self.tick);
        let state = self.ticks.get(&across).copied();
        let explained = (tick - self.tick).abs() == 1 && state.is_some_and(|s| s.is_watched_only());
        debug_assert!(
            explained,
            "the walk ended at tick {} but the contract's tick is {tick}",
            self.tick
        );
        let Some(mut state) = state.filter(|_| explained) else {
            return Err(PoolError::ReplayMismatch(format!(
                "the walk ended at tick {} but the contract's tick is {tick}",
                self.tick
            )));
        };
        record.ticks.push((across, state));
        state.flip_watched(self.fee_growth_global0, self.fee_growth_global1);
        self.ticks.insert(across, state);
        self.tick = tick;
        Ok(())
    }

    /// The tick the contract holds after a swap in `zero_for_one` direction
    /// that started at `start_price` and `start_tick` and ended at the
    /// current price. It depends only on contract state:
    ///
    /// * If the price moved, the tick is the one of the new price, except
    ///   that a swap down that stopped exactly on one of its own step
    ///   boundaries leaves the tick one below it.
    /// * If the price did not move, the tick only changes when the price sat
    ///   exactly on a boundary in the swap direction: the zero-length first
    ///   step crosses it.
    fn contract_tick_after_swap(
        &self,
        zero_for_one: bool,
        start_price: U256,
        start_tick: i32,
    ) -> PoolResult<i32> {
        let price = self.sqrt_price_x96;
        if price != start_price {
            let tick = get_tick_at_sqrt_ratio(price)?;
            let on_boundary = zero_for_one
                && self.contract_stops_at(tick, true)
                && price == get_sqrt_ratio_at_tick(tick)?;
            return Ok(if on_boundary { tick - 1 } else { tick });
        }
        if zero_for_one {
            let crossed = start_tick >= MIN_TICK
                && self.contract_stops_at(start_tick, true)
                && start_price == get_sqrt_ratio_at_tick(start_tick)?;
            Ok(if crossed { start_tick - 1 } else { start_tick })
        } else {
            let above = start_tick + 1;
            let crossed = above <= MAX_TICK
                && self.contract_stops_at(above, false)
                && start_price == get_sqrt_ratio_at_tick(above)?;
            Ok(if crossed { above } else { start_tick })
        }
    }

    /// Whether the contract's tick search ends a step at `tick` when it
    /// reaches the tick going in the given direction: initialized ticks,
    /// the first tick of each 256-tick word going down, the last one going
    /// up, and the tick limits. Watched ticks do not count.
    fn contract_stops_at(&self, tick: i32, zero_for_one: bool) -> bool {
        let limit = if zero_for_one { MIN_TICK } else { MAX_TICK };
        if tick == limit {
            return true;
        }
        if tick % self.tick_spacing != 0 {
            return false;
        }
        let initialized = self
            .ticks
            .get(&tick)
            .is_some_and(|state| state.info.liquidity_gross > 0);
        let position = tick.div_euclid(self.tick_spacing).rem_euclid(256);
        initialized || position == if zero_for_one { 0 } else { 255 }
    }

    /// The next step boundary in the swap direction: its tick and price.
    fn next_boundary(&self, zero_for_one: bool) -> PoolResult<(i32, U256)> {
        let (tick_next, _) = self.next_initialized_tick_within_one_word(self.tick, zero_for_one);
        let tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
        Ok((tick_next, get_sqrt_ratio_at_tick(tick_next)?))
    }

    /// The contract walks its tick bitmap one 256-tick word at a time. The
    /// word boundaries are step boundaries even when no tick is initialized
    /// there, so they have to be reproduced for the rounding to match.
    /// Watched ticks count as initialized here.
    pub fn next_initialized_tick_within_one_word(&self, tick: i32, lte: bool) -> (i32, bool) {
        let spacing = self.tick_spacing;
        let compressed = tick.div_euclid(spacing);
        if lte {
            let word_start = (compressed - compressed.rem_euclid(256)) * spacing;
            let here = compressed * spacing;
            match self
                .ticks
                .range(word_start..=here)
                .rev()
                .find(|(_, state)| state.info.is_boundary())
            {
                Some((t, _)) => (*t, true),
                None => (word_start, false),
            }
        } else {
            let next = compressed + 1;
            let word_end = (next + (255 - next.rem_euclid(256))) * spacing;
            let from = next * spacing;
            match self
                .ticks
                .range(from..=word_end)
                .find(|(_, state)| state.info.is_boundary())
            {
                Some((t, _)) => (*t, true),
                None => (word_end, false),
            }
        }
    }

    /// Books a step's fee, moves the price, and crosses the boundary tick if
    /// the step reached it.
    #[allow(clippy::too_many_arguments)]
    fn finish_step(
        &mut self,
        zero_for_one: bool,
        tick_next: i32,
        sqrt_price_next: U256,
        sqrt_price_after: U256,
        amount_in: U256,
        amount_out: U256,
        fee_amount: U256,
        record: &mut SwapRecord,
    ) -> PoolResult<()> {
        let sqrt_price_start = self.sqrt_price_x96;
        self.account_step(
            zero_for_one,
            sqrt_price_start,
            sqrt_price_after,
            amount_in,
            amount_out,
            fee_amount,
            record,
        )?;

        self.sqrt_price_x96 = sqrt_price_after;
        if sqrt_price_after == sqrt_price_next {
            self.cross(tick_next, zero_for_one, record)?;
            self.tick = if zero_for_one {
                tick_next - 1
            } else {
                tick_next
            };
        } else if sqrt_price_after != sqrt_price_start {
            self.tick = get_tick_at_sqrt_ratio(sqrt_price_after)?;
        }
        Ok(())
    }

    /// Crosses a step boundary: the contract's `Tick.cross` for a tick that
    /// holds liquidity, and a flip of the watched record for a watched one.
    /// A word boundary with neither is not in the map and changes nothing.
    fn cross(&mut self, tick: i32, zero_for_one: bool, record: &mut SwapRecord) -> PoolResult<()> {
        let (g0, g1) = (self.fee_growth_global0, self.fee_growth_global1);
        let Some(state) = self.ticks.get_mut(&tick) else {
            return Ok(());
        };
        record.ticks.push((tick, *state));
        state.flip_watched(g0, g1);
        if state.info.liquidity_gross == 0 {
            return Ok(());
        }
        let info = &mut state.info;
        info.fee_growth_outside0 = g0.wrapping_sub(info.fee_growth_outside0);
        info.fee_growth_outside1 = g1.wrapping_sub(info.fee_growth_outside1);
        let net = if zero_for_one {
            info.liquidity_net
                .checked_neg()
                .ok_or(MathError::LiquidityDelta)?
        } else {
            info.liquidity_net
        };
        self.liquidity = add_liquidity_delta(self.liquidity, net)?;
        Ok(())
    }

    /// Splits a step's fee between protocol and liquidity providers, adds the
    /// providers' part to fee growth, and records the step for the observer.
    #[allow(clippy::too_many_arguments)]
    fn account_step(
        &mut self,
        zero_for_one: bool,
        sqrt_price_start: U256,
        sqrt_price_end: U256,
        amount_in: U256,
        amount_out: U256,
        fee_amount: U256,
        record: &mut SwapRecord,
    ) -> PoolResult<()> {
        let fee_protocol = if zero_for_one {
            self.fee_protocol0
        } else {
            self.fee_protocol1
        };
        let protocol_fee = protocol_cut(fee_amount, fee_protocol);
        let lp_fee = fee_amount - protocol_fee;

        if self.liquidity > 0 {
            let growth = mul_div(lp_fee, Q128, U256::from(self.liquidity))?;
            if zero_for_one {
                self.fee_growth_global0 = self.fee_growth_global0.wrapping_add(growth);
            } else {
                self.fee_growth_global1 = self.fee_growth_global1.wrapping_add(growth);
            }
        }

        record.steps.push(StepInfo {
            zero_for_one,
            tick_start: self.tick,
            sqrt_price_start,
            sqrt_price_end,
            liquidity: self.liquidity,
            amount_in,
            amount_out,
            lp_fee,
            protocol_fee,
        });
        Ok(())
    }

    // --- checks -----------------------------------------------------------

    fn require_initialized(&self) -> PoolResult<()> {
        if self.initialized {
            Ok(())
        } else {
            Err(PoolError::NotInitialized)
        }
    }

    fn check_tick(&self, tick: i32) -> PoolResult<()> {
        if !(MIN_TICK..=MAX_TICK).contains(&tick) {
            return Err(PoolError::BadTickRange);
        }
        if tick % self.tick_spacing != 0 {
            return Err(PoolError::TickNotSpaced(tick));
        }
        Ok(())
    }

    fn check_ticks(&self, tick_lower: i32, tick_upper: i32) -> PoolResult<()> {
        if tick_lower >= tick_upper {
            return Err(PoolError::BadTickRange);
        }
        self.check_tick(tick_lower)?;
        self.check_tick(tick_upper)
    }
}

/// The protocol's cut of a fee: `fee / denominator`, or nothing when off.
fn protocol_cut(fee_amount: U256, fee_protocol: u8) -> U256 {
    if fee_protocol == 0 {
        U256::ZERO
    } else {
        fee_amount / U256::from(fee_protocol)
    }
}

/// A step never goes past the swap's price limit.
fn cap_target(zero_for_one: bool, sqrt_price_next: U256, limit: U256) -> U256 {
    let beyond = if zero_for_one {
        sqrt_price_next < limit
    } else {
        sqrt_price_next > limit
    };
    if beyond {
        limit
    } else {
        sqrt_price_next
    }
}

fn to_i256(x: U256) -> PoolResult<I256> {
    I256::try_from(x).map_err(|_| PoolError::Math(MathError::Overflow256))
}

/// The most liquidity any single tick may reference for a tick spacing.
pub fn tick_spacing_to_max_liquidity_per_tick(tick_spacing: i32) -> u128 {
    let min_tick = (MIN_TICK / tick_spacing) * tick_spacing;
    let max_tick = (MAX_TICK / tick_spacing) * tick_spacing;
    let num_ticks = ((max_tick - min_tick) / tick_spacing) as u128 + 1;
    u128::MAX / num_ticks
}

/// Fee a position earned for one token between two fee-growth readings.
pub fn fees_earned(
    fee_growth_inside_now: U256,
    fee_growth_inside_then: U256,
    liquidity: u128,
) -> U256 {
    // Cannot fail: the divisor is a non-zero constant and the quotient is
    // below the 128-bit liquidity times 2^128 over 2^128.
    mul_div(
        fee_growth_inside_now.wrapping_sub(fee_growth_inside_then),
        U256::from(liquidity),
        Q128,
    )
    .unwrap_or(U256::MAX)
}

/// Fee the pool charges on a flash loan of `amount`, rounded up.
pub fn flash_fee(amount: U256, fee_pips: u32) -> Result<U256, MathError> {
    mul_div_rounding_up(amount, U256::from(fee_pips), U256::from(1_000_000u32))
}

#[cfg(test)]
#[path = "pool_tests.rs"]
mod tests;
