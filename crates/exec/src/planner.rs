//! The planner: from what the engine did to the calls that do it on chain.
//!
//! One action of the engine (an open, a close, a withdrawal, a flip, a
//! recentre) is a short list of [`StepRecord`]s: liquidity burnt, swaps,
//! liquidity minted. The planner turns them, in order, into the position
//! manager's and the router's calls, with the approvals they need, batches
//! them in one `multiSend` that the Safe makes, and wraps that in the roles
//! module's call the keeper sends. Every amount is bounded by what the
//! engine recorded, less the slippage tolerance, so the transaction reverts
//! when the pool has moved: integer maths, rounded toward the safe side.
//!
//! The Safe is the only recipient of anything. [`verify`] reads every call
//! of a plan back and refuses one that pays anyone else or calls a contract
//! the plan has no business with; [`plan`] runs it on its own output.

use alloy_primitives::{Address, B256, U256};
use lpbot_core::{Step, StepRecord};
use lpbot_venues::uniswap_v3::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};

use crate::abi::AbiError;
use crate::calls::{
    self, approve, burn, collect, decrease_liquidity, exact_input_single,
    exec_transaction_with_role, mint, multi_send, BatchError, Call, CollectParams, DecreaseParams,
    ExactInputSingleParams, MintParams, Operation,
};

/// Parts per million, the unit of the tolerances.
pub const PPM: u32 = 1_000_000;

/// What the Safe currently lets the position manager and the router take of
/// each of the pool's tokens, as read from the chain before planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Allowances {
    pub manager0: U256,
    pub manager1: U256,
    pub router0: U256,
    pub router1: U256,
}

/// The position NFT the Safe holds in the pool, as read from the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenPosition {
    pub token_id: U256,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub liquidity: u128,
}

/// Everything the planner needs besides the records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanContext {
    pub chain_id: u64,
    /// The Safe: the only recipient of any token, ever.
    pub safe: Address,
    pub position_manager: Address,
    pub router: Address,
    /// Safe's MultiSendCallOnly.
    pub multisend: Address,
    /// The Zodiac Roles v2 modifier enabled on the Safe, and the keeper's role.
    pub roles: Address,
    pub role_key: B256,
    /// The pool, in its own order (`token0` below `token1` by address).
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    /// The position the action works on; needed for a burn.
    pub position: Option<OpenPosition>,
    pub allowances: Allowances,
    /// Tokens whose allowance must be set to zero before a new non-zero one
    /// (USDT reverts otherwise).
    pub zero_first: Vec<Address>,
    /// How far an amount may fall short of the one recorded, in parts per
    /// million: the minimums are the recorded amounts less this.
    pub slippage_ppm: u32,
    /// How far a swap may move the pool's price, in parts per million of
    /// the price recorded: the swap's price limit.
    pub price_limit_ppm: u32,
    /// The time after which every call reverts, in seconds since 1970.
    pub deadline: u64,
}

/// What a plan holds an amount to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    /// The record the bound comes from, by index.
    pub record: usize,
    /// The parameter it is written into: `amount0_min`, `amount1_min`,
    /// `amount_out_minimum` or `sqrt_price_limit_x96`.
    pub field: &'static str,
    /// What the engine recorded (for a price limit, the price it recorded).
    pub recorded: U256,
    /// What the call holds it to.
    pub bound: U256,
}

/// One action, ready to sign.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub chain_id: u64,
    /// The calls the Safe makes, in order.
    pub inner: Vec<Call>,
    /// The inner calls as one `multiSend`, a delegate call from the Safe.
    pub batch: Call,
    /// The batch through the roles module: one transaction, but a delegate
    /// call from the Safe, which the policy must then allow.
    pub outer: Call,
    /// The transactions the keeper sends by default: each inner call on its
    /// own through the roles module, with operation `Call`, so the policy
    /// never has to allow a delegate call (`docs/DESIGN.md`, "What 2026
    /// taught about Roles"). Sent in order, each after the one before has
    /// landed.
    pub separate: Vec<Call>,
    pub bounds: Vec<Bound>,
    /// The allowances once the plan has run, if every call takes what it
    /// asks for.
    pub allowances_after: Allowances,
    /// Whether the plan burns the position NFT, leaving no position.
    pub burns_position: bool,
}

/// Why an action cannot be planned. Each says what to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("there are no steps to plan; the engine recorded nothing for this action")]
    Empty,
    #[error(
        "the steps come from more than one action (block {block} {action} campaign {campaign}, \
         then block {other_block} {other_action} campaign {other_campaign}); plan one action \
         at a time"
    )]
    Mixed {
        block: u64,
        action: String,
        campaign: u32,
        other_block: u64,
        other_action: String,
        other_campaign: u32,
    },
    #[error("{what} is not set (the zero address); set it in the chain's execution settings")]
    Unset { what: &'static str },
    #[error("chain id 0 is not a chain; set the chain's id in its settings")]
    ChainId,
    #[error(
        "the pool's tokens must be given in the pool's order, token0 below token1 by address; \
         got {token0} and {token1}"
    )]
    TokenOrder { token0: Address, token1: Address },
    #[error("zero_first names {token}, which is not one of the pool's tokens; remove it")]
    ForeignToken { token: Address },
    #[error("the tick spacing is {0}; it must be above 0, as the pool's is")]
    Spacing(i32),
    #[error("{what} is {value} parts per million; set it between 1 and 1000000")]
    Tolerance { what: &'static str, value: u32 },
    #[error(
        "step {record} is on [{lower}, {upper}), which is not a range of this pool (ticks in \
         order, within the pool's limits, on multiples of {spacing})"
    )]
    Ticks {
        record: usize,
        lower: i32,
        upper: i32,
        spacing: i32,
    },
    #[error("step {record} mints nothing (both amounts zero); the engine should not record it")]
    MintsNothing { record: usize },
    #[error("step {record} swaps nothing; the engine should not record it")]
    SwapsNothing { record: usize },
    #[error(
        "step {record} burns a position, but none is open in the pool; read the Safe's position \
         from the chain and give it to the planner"
    )]
    NoPosition { record: usize },
    #[error(
        "step {record} burns [{lower}, {upper}), but the open position is on [{open_lower}, \
         {open_upper}); the chain and the engine disagree: reconcile before planning"
    )]
    OtherPosition {
        record: usize,
        lower: i32,
        upper: i32,
        open_lower: i32,
        open_upper: i32,
    },
    #[error(
        "step {record} burns {wanted} liquidity, but the position holds {held}; the chain and \
         the engine disagree: reconcile before planning"
    )]
    BurnTooLarge {
        record: usize,
        wanted: u128,
        held: u128,
    },
    #[error("an amount of step {record} is too large to bound; it cannot be a real amount")]
    Overflow { record: usize },
    #[error("call {index} of the plan pays {found}, not the Safe {safe}; the plan is refused")]
    Recipient {
        index: usize,
        found: Address,
        safe: Address,
    },
    #[error(
        "call {index} of the plan goes to {found}, which is neither the position manager, the \
         router nor one of the pool's tokens; the plan is refused"
    )]
    Target { index: usize, found: Address },
    #[error("call {index} of the plan is not one the planner makes ({why}); the plan is refused")]
    Unexpected { index: usize, why: String },
    #[error(transparent)]
    Abi(#[from] AbiError),
    #[error(transparent)]
    Batch(#[from] BatchError),
}

/// `amount` less `ppm` parts per million, rounded down: the least a call
/// may get or give back. `None` only for a tolerance above a million.
pub fn less_slippage(amount: U256, ppm: u32) -> Option<U256> {
    if ppm > PPM {
        return None;
    }
    let keep = U256::from(PPM - ppm);
    let million = U256::from(PPM);
    Some(match amount.checked_mul(keep) {
        Some(product) => product / million,
        // Only past 2^236: split, which rounds down further still.
        None => amount / million * keep,
    })
}

/// The integer square root, rounded down.
fn isqrt(value: u128) -> u128 {
    if value < 2 {
        return value;
    }
    let mut x = 1u128 << (value.ilog2() / 2 + 1);
    loop {
        let y = (x + value / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// A swap's price limit: the square-root price at which the pool stops,
/// `ppm` parts per million of the price below (`zero_for_one`, the price
/// falls) or above `sqrt_price_x96`. Rounded toward the recorded price, so
/// the limit is never looser than asked, and kept strictly inside the
/// pool's limits. `None` for a tolerance of zero or above a million.
pub fn price_limit(sqrt_price_x96: U256, zero_for_one: bool, ppm: u32) -> Option<U256> {
    if ppm == 0 || ppm > PPM {
        return None;
    }
    // sqrt(1 ± ppm / 1e6), in billionths: sqrt((1e6 ± ppm) * 1e12).
    let scale = U256::from(1_000_000_000u64);
    let one = U256::from(1u8);
    let limit = if zero_for_one {
        let square = u128::from(PPM - ppm) * 1_000_000_000_000;
        let mut root = isqrt(square);
        if root * root < square {
            root += 1;
        }
        let product = sqrt_price_x96 * U256::from(root);
        product.div_ceil(scale).max(MIN_SQRT_RATIO + one)
    } else {
        let square = u128::from(PPM + ppm) * 1_000_000_000_000;
        let product = sqrt_price_x96 * U256::from(isqrt(square));
        (product / scale).min(MAX_SQRT_RATIO - one)
    };
    Some(limit)
}

fn check_context(ctx: &PlanContext) -> Result<(), PlanError> {
    if ctx.chain_id == 0 {
        return Err(PlanError::ChainId);
    }
    for (address, what) in [
        (ctx.safe, "the Safe"),
        (ctx.position_manager, "the position manager"),
        (ctx.router, "the router"),
        (ctx.multisend, "the MultiSend contract"),
        (ctx.roles, "the roles module"),
        (ctx.token0, "the pool's token0"),
        (ctx.token1, "the pool's token1"),
    ] {
        if address == Address::ZERO {
            return Err(PlanError::Unset { what });
        }
    }
    if ctx.token0 >= ctx.token1 {
        return Err(PlanError::TokenOrder {
            token0: ctx.token0,
            token1: ctx.token1,
        });
    }
    if let Some(token) = ctx
        .zero_first
        .iter()
        .find(|t| **t != ctx.token0 && **t != ctx.token1)
    {
        return Err(PlanError::ForeignToken { token: *token });
    }
    if ctx.tick_spacing <= 0 {
        return Err(PlanError::Spacing(ctx.tick_spacing));
    }
    for (value, what) in [
        (ctx.slippage_ppm, "the slippage tolerance"),
        (ctx.price_limit_ppm, "the swap's price limit"),
    ] {
        if value == 0 || value > PPM {
            return Err(PlanError::Tolerance { what, value });
        }
    }
    Ok(())
}

fn check_records(records: &[StepRecord]) -> Result<(), PlanError> {
    let first = records.first().ok_or(PlanError::Empty)?;
    if let Some(other) = records.iter().find(|r| {
        r.block != first.block || r.action != first.action || r.campaign != first.campaign
    }) {
        return Err(PlanError::Mixed {
            block: first.block,
            action: first.action.clone(),
            campaign: first.campaign,
            other_block: other.block,
            other_action: other.action.clone(),
            other_campaign: other.campaign,
        });
    }
    Ok(())
}

fn check_ticks(record: usize, lower: i32, upper: i32, spacing: i32) -> Result<(), PlanError> {
    let fits = lower < upper
        && lower >= MIN_TICK
        && upper <= MAX_TICK
        && lower % spacing == 0
        && upper % spacing == 0;
    if fits {
        Ok(())
    } else {
        Err(PlanError::Ticks {
            record,
            lower,
            upper,
            spacing,
        })
    }
}

/// Builds the plan as it goes, keeping the allowances in step.
struct Builder<'a> {
    ctx: &'a PlanContext,
    inner: Vec<Call>,
    bounds: Vec<Bound>,
    allowances: Allowances,
    /// Token and spender pairs already drawn on in this plan, whose
    /// allowance may be above what is tracked if a call took less.
    drawn: Vec<(Address, Address)>,
}

impl Builder<'_> {
    fn deadline(&self) -> U256 {
        U256::from(self.ctx.deadline)
    }

    fn min(&self, record: usize, amount: U256) -> Result<U256, PlanError> {
        less_slippage(amount, self.ctx.slippage_ppm).ok_or(PlanError::Overflow { record })
    }

    fn allowance_mut(&mut self, token: Address, spender: Address) -> &mut U256 {
        let to_manager = spender == self.ctx.position_manager;
        let a = &mut self.allowances;
        match (token == self.ctx.token0, to_manager) {
            (true, true) => &mut a.manager0,
            (false, true) => &mut a.manager1,
            (true, false) => &mut a.router0,
            (false, false) => &mut a.router1,
        }
    }

    /// Approves `spender` for `needed` of `token` unless it already may take
    /// that much, then counts the amount as taken.
    fn draw(&mut self, token: Address, spender: Address, needed: U256) {
        if needed.is_zero() {
            return;
        }
        let current = *self.allowance_mut(token, spender);
        if current < needed {
            let maybe_left = !current.is_zero() || self.drawn.contains(&(token, spender));
            if self.ctx.zero_first.contains(&token) && maybe_left {
                self.inner.push(approve(token, spender, U256::ZERO));
            }
            self.inner.push(approve(token, spender, needed));
            *self.allowance_mut(token, spender) = needed;
        }
        let left = self.allowance_mut(token, spender);
        *left -= needed;
        if !self.drawn.contains(&(token, spender)) {
            self.drawn.push((token, spender));
        }
    }

    fn bound(&mut self, record: usize, field: &'static str, recorded: U256, bound: U256) {
        self.bounds.push(Bound {
            record,
            field,
            recorded,
            bound,
        });
    }
}

/// The calls that carry out one action of the engine, from its records.
pub fn plan(records: &[StepRecord], ctx: &PlanContext) -> Result<Planned, PlanError> {
    check_context(ctx)?;
    check_records(records)?;
    let mut b = Builder {
        ctx,
        inner: Vec::new(),
        bounds: Vec::new(),
        allowances: ctx.allowances,
        drawn: Vec::new(),
    };
    let mut position = ctx.position;
    let mut burns_position = false;
    for (i, record) in records.iter().enumerate() {
        match &record.step {
            Step::Mint {
                tick_lower,
                tick_upper,
                amount0,
                amount1,
                ..
            } => {
                check_ticks(i, *tick_lower, *tick_upper, ctx.tick_spacing)?;
                if amount0.is_zero() && amount1.is_zero() {
                    return Err(PlanError::MintsNothing { record: i });
                }
                b.draw(ctx.token0, ctx.position_manager, *amount0);
                b.draw(ctx.token1, ctx.position_manager, *amount1);
                let amount0_min = b.min(i, *amount0)?;
                let amount1_min = b.min(i, *amount1)?;
                b.bound(i, "amount0_min", *amount0, amount0_min);
                b.bound(i, "amount1_min", *amount1, amount1_min);
                let call = mint(
                    ctx.position_manager,
                    &MintParams {
                        token0: ctx.token0,
                        token1: ctx.token1,
                        fee: ctx.fee,
                        tick_lower: *tick_lower,
                        tick_upper: *tick_upper,
                        amount0_desired: *amount0,
                        amount1_desired: *amount1,
                        amount0_min,
                        amount1_min,
                        recipient: ctx.safe,
                        deadline: b.deadline(),
                    },
                )?;
                b.inner.push(call);
            }
            Step::Burn {
                tick_lower,
                tick_upper,
                liquidity,
                amount0,
                amount1,
                ..
            } => {
                let open = position
                    .as_mut()
                    .ok_or(PlanError::NoPosition { record: i })?;
                if (open.tick_lower, open.tick_upper) != (*tick_lower, *tick_upper) {
                    return Err(PlanError::OtherPosition {
                        record: i,
                        lower: *tick_lower,
                        upper: *tick_upper,
                        open_lower: open.tick_lower,
                        open_upper: open.tick_upper,
                    });
                }
                if *liquidity > open.liquidity {
                    return Err(PlanError::BurnTooLarge {
                        record: i,
                        wanted: *liquidity,
                        held: open.liquidity,
                    });
                }
                let token_id = open.token_id;
                let amount0_min = b.min(i, *amount0)?;
                let amount1_min = b.min(i, *amount1)?;
                b.bound(i, "amount0_min", *amount0, amount0_min);
                b.bound(i, "amount1_min", *amount1, amount1_min);
                b.inner.push(decrease_liquidity(
                    ctx.position_manager,
                    &DecreaseParams {
                        token_id,
                        liquidity: *liquidity,
                        amount0_min,
                        amount1_min,
                        deadline: b.deadline(),
                    },
                ));
                // Everything owed: the principal just taken out and the fees.
                b.inner.push(collect(
                    ctx.position_manager,
                    &CollectParams {
                        token_id,
                        recipient: ctx.safe,
                        amount0_max: u128::MAX,
                        amount1_max: u128::MAX,
                    },
                ));
                open.liquidity -= liquidity;
                if open.liquidity == 0 {
                    b.inner.push(burn(ctx.position_manager, token_id));
                    position = None;
                    burns_position = true;
                }
            }
            Step::Swap {
                zero_for_one,
                amount_in,
                amount_out,
            } => {
                if amount_in.is_zero() {
                    return Err(PlanError::SwapsNothing { record: i });
                }
                let (token_in, token_out) = if *zero_for_one {
                    (ctx.token0, ctx.token1)
                } else {
                    (ctx.token1, ctx.token0)
                };
                b.draw(token_in, ctx.router, *amount_in);
                let amount_out_minimum = b.min(i, *amount_out)?;
                let limit = price_limit(record.sqrt_price_x96, *zero_for_one, ctx.price_limit_ppm)
                    .ok_or(PlanError::Tolerance {
                        what: "the swap's price limit",
                        value: ctx.price_limit_ppm,
                    })?;
                b.bound(i, "amount_out_minimum", *amount_out, amount_out_minimum);
                b.bound(i, "sqrt_price_limit_x96", record.sqrt_price_x96, limit);
                let call = exact_input_single(
                    ctx.router,
                    &ExactInputSingleParams {
                        token_in,
                        token_out,
                        fee: ctx.fee,
                        recipient: ctx.safe,
                        deadline: b.deadline(),
                        amount_in: *amount_in,
                        amount_out_minimum,
                        sqrt_price_limit_x96: limit,
                    },
                )?;
                b.inner.push(call);
            }
        }
    }
    let batch = multi_send(ctx.multisend, &b.inner)?;
    let outer = exec_transaction_with_role(ctx.roles, &batch, ctx.role_key, true);
    let separate = b
        .inner
        .iter()
        .map(|call| exec_transaction_with_role(ctx.roles, call, ctx.role_key, true))
        .collect();
    let planned = Planned {
        chain_id: ctx.chain_id,
        inner: b.inner,
        batch,
        outer,
        separate,
        bounds: b.bounds,
        allowances_after: b.allowances,
        burns_position,
    };
    verify(&planned, ctx)?;
    Ok(planned)
}

/// Reads every call of a plan back and refuses one that pays anyone but the
/// Safe, calls a contract other than the position manager, the router and
/// the pool's tokens, approves anyone else, or is not one the planner makes.
/// The batch and the outer call must be the inner calls, through the
/// MultiSend contract and the roles module.
pub fn verify(planned: &Planned, ctx: &PlanContext) -> Result<(), PlanError> {
    let safe = ctx.safe;
    let paid = |index: usize, found: Address| {
        if found == safe {
            Ok(())
        } else {
            Err(PlanError::Recipient { index, found, safe })
        }
    };
    let unexpected = |index: usize, why: String| PlanError::Unexpected { index, why };
    for (index, call) in planned.inner.iter().enumerate() {
        if call.operation != Operation::Call || !call.value.is_zero() {
            return Err(unexpected(
                index,
                "a delegate call or a call that sends ether".into(),
            ));
        }
        let selector: [u8; 4] = call
            .data
            .get(..4)
            .and_then(|s| s.try_into().ok())
            .ok_or_else(|| unexpected(index, "no selector".into()))?;
        let is = |signature: &str| crate::abi::selector(signature) == selector;
        if call.to == ctx.position_manager {
            if is(calls::MINT) {
                let p = calls::decode_mint(&call.data)?;
                paid(index, p.recipient)?;
                if (p.token0, p.token1, p.fee) != (ctx.token0, ctx.token1, ctx.fee) {
                    return Err(unexpected(index, "a mint into another pool".into()));
                }
            } else if is(calls::COLLECT) {
                paid(index, calls::decode_collect(&call.data)?.recipient)?;
            } else if is(calls::DECREASE_LIQUIDITY) {
                calls::decode_decrease_liquidity(&call.data)?;
            } else if is(calls::BURN) {
                calls::decode_burn(&call.data)?;
            } else {
                return Err(unexpected(
                    index,
                    format!("position manager selector 0x{}", hex::encode(selector)),
                ));
            }
        } else if call.to == ctx.router {
            if !is(calls::EXACT_INPUT_SINGLE) {
                return Err(unexpected(
                    index,
                    format!("router selector 0x{}", hex::encode(selector)),
                ));
            }
            let p = calls::decode_exact_input_single(&call.data)?;
            paid(index, p.recipient)?;
            let pair = (p.token_in, p.token_out);
            if p.fee != ctx.fee
                || (pair != (ctx.token0, ctx.token1) && pair != (ctx.token1, ctx.token0))
            {
                return Err(unexpected(index, "a swap through another pool".into()));
            }
        } else if call.to == ctx.token0 || call.to == ctx.token1 {
            if !is(calls::APPROVE) {
                return Err(unexpected(
                    index,
                    format!("token selector 0x{}", hex::encode(selector)),
                ));
            }
            let (spender, _) = calls::decode_approve(&call.data)?;
            if spender != ctx.position_manager && spender != ctx.router {
                return Err(unexpected(index, format!("an approval for {spender}")));
            }
        } else {
            return Err(PlanError::Target {
                index,
                found: call.to,
            });
        }
    }
    let expected_batch = multi_send(ctx.multisend, &planned.inner)?;
    if planned.batch != expected_batch {
        return Err(unexpected(
            planned.inner.len(),
            "the batch is not the inner calls".into(),
        ));
    }
    let roles = calls::decode_exec_transaction_with_role(&planned.outer.data)?;
    if planned.outer.to != ctx.roles
        || planned.outer.operation != Operation::Call
        || !planned.outer.value.is_zero()
        || roles.inner != planned.batch
        || roles.role_key != ctx.role_key
        || !roles.should_revert
    {
        return Err(unexpected(
            planned.inner.len() + 1,
            "the outer call is not the batch through the roles module".into(),
        ));
    }
    if planned.separate.len() != planned.inner.len() {
        return Err(unexpected(
            planned.inner.len() + 1,
            "the separate transactions are not one for each inner call".into(),
        ));
    }
    for (index, (each, inner)) in planned.separate.iter().zip(&planned.inner).enumerate() {
        let roles = calls::decode_exec_transaction_with_role(&each.data)?;
        if each.to != ctx.roles
            || each.operation != Operation::Call
            || !each.value.is_zero()
            || roles.inner != *inner
            || roles.inner.operation != Operation::Call
            || roles.role_key != ctx.role_key
            || !roles.should_revert
        {
            return Err(unexpected(
                index,
                "a separate transaction is not its inner call, as a plain call, through the \
                 roles module"
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calls::{
        decode_approve, decode_burn, decode_collect, decode_decrease_liquidity,
        decode_exact_input_single, decode_exec_transaction_with_role, decode_mint,
        decode_multi_send,
    };
    use alloy_primitives::address;

    // USDC below WETH by address, as on mainnet; the addresses are made up.
    const TOKEN0: Address = address!("0000000000000000000000000000000000000010");
    const TOKEN1: Address = address!("0000000000000000000000000000000000000020");
    const SAFE: Address = address!("00000000000000000000000000000000000000a3");
    const MANAGER: Address = address!("00000000000000000000000000000000000000a1");
    const ROUTER: Address = address!("00000000000000000000000000000000000000a2");
    const MULTISEND: Address = address!("00000000000000000000000000000000000000a4");
    const ROLES: Address = address!("00000000000000000000000000000000000000a5");
    const SPACING: i32 = 10;
    const TOKEN_ID: u64 = 987_654;
    const SQRT_PRICE: u128 = 1_234_567_890_123_456_789_012_345;

    fn ctx() -> PlanContext {
        PlanContext {
            chain_id: 1,
            safe: SAFE,
            position_manager: MANAGER,
            router: ROUTER,
            multisend: MULTISEND,
            roles: ROLES,
            role_key: B256::repeat_byte(0x4b),
            token0: TOKEN0,
            token1: TOKEN1,
            fee: 500,
            tick_spacing: SPACING,
            position: None,
            allowances: Allowances::default(),
            zero_first: Vec::new(),
            slippage_ppm: 5_000,
            price_limit_ppm: 20_000,
            deadline: 1_800_000_000,
        }
    }

    fn open(lower: i32, upper: i32, liquidity: u128) -> Option<OpenPosition> {
        Some(OpenPosition {
            token_id: U256::from(TOKEN_ID),
            tick_lower: lower,
            tick_upper: upper,
            liquidity,
        })
    }

    fn record(action: &str, step: Step) -> StepRecord {
        StepRecord {
            block: 21_000_000,
            time: 1_730_000_000,
            campaign: 3,
            action: action.into(),
            sqrt_price_x96: U256::from(SQRT_PRICE),
            step,
        }
    }

    fn u(n: u128) -> U256 {
        U256::from(n)
    }

    fn mint_step(lower: i32, upper: i32, amount0: u128, amount1: u128) -> Step {
        Step::Mint {
            tick_lower: lower,
            tick_upper: upper,
            liquidity: 1_000_000,
            amount0: u(amount0),
            amount1: u(amount1),
        }
    }

    fn burn_step(lower: i32, upper: i32, liquidity: u128, amount0: u128, amount1: u128) -> Step {
        Step::Burn {
            tick_lower: lower,
            tick_upper: upper,
            liquidity,
            amount0: u(amount0),
            amount1: u(amount1),
            fees0: u(11),
            fees1: u(22),
        }
    }

    fn swap_step(zero_for_one: bool, amount_in: u128, amount_out: u128) -> Step {
        Step::Swap {
            zero_for_one,
            amount_in: u(amount_in),
            amount_out: u(amount_out),
        }
    }

    fn min(amount: u128) -> U256 {
        less_slippage(u(amount), ctx().slippage_ppm).unwrap()
    }

    /// The selector names of the inner calls, in order.
    fn shape(planned: &Planned) -> Vec<&'static str> {
        planned
            .inner
            .iter()
            .map(|call| {
                let s: [u8; 4] = call.data[..4].try_into().unwrap();
                [
                    (calls::MINT, "mint"),
                    (calls::DECREASE_LIQUIDITY, "decrease"),
                    (calls::COLLECT, "collect"),
                    (calls::BURN, "burn"),
                    (calls::EXACT_INPUT_SINGLE, "swap"),
                    (calls::APPROVE, "approve"),
                ]
                .iter()
                .find(|(sig, _)| crate::abi::selector(sig) == s)
                .map(|(_, name)| *name)
                .unwrap()
            })
            .collect()
    }

    /// The batch and the roles call carry exactly the inner calls.
    fn check_wrapping(planned: &Planned, ctx: &PlanContext) {
        assert_eq!(
            decode_multi_send(&planned.batch.data).unwrap(),
            planned.inner
        );
        assert_eq!(planned.batch.to, MULTISEND);
        assert_eq!(planned.batch.operation, Operation::DelegateCall);
        let roles = decode_exec_transaction_with_role(&planned.outer.data).unwrap();
        assert_eq!(planned.outer.to, ROLES);
        assert_eq!(roles.inner, planned.batch);
        assert_eq!(roles.role_key, ctx.role_key);
        assert!(roles.should_revert);
        assert_eq!(planned.chain_id, 1);
    }

    #[test]
    fn less_slippage_rounds_down_and_refuses_a_tolerance_above_a_million() {
        assert_eq!(less_slippage(u(1_000_000), 5_000), Some(u(995_000)));
        assert_eq!(less_slippage(u(999), 5_000), Some(u(994)), "994.005 down");
        assert_eq!(less_slippage(u(7), 1_000_000), Some(U256::ZERO));
        assert_eq!(less_slippage(u(7), 0), Some(u(7)));
        assert_eq!(less_slippage(u(7), 1_000_001), None);
        // Past 2^236 the product overflows; the split still rounds down.
        let huge = U256::MAX;
        let bounded = less_slippage(huge, 5_000).unwrap();
        assert!(bounded < huge);
        assert!(bounded >= huge / u(1_000_000) * u(994_999));
    }

    #[test]
    fn a_price_limit_is_the_price_moved_by_the_tolerance_and_never_looser() {
        let price = u(SQRT_PRICE);
        let as_f64 = |v: U256| v.to_string().parse::<f64>().unwrap();
        let down = price_limit(price, true, 10_000).unwrap();
        let up = price_limit(price, false, 10_000).unwrap();
        assert!(down < price && up > price);
        // The price is the square of the square root: about 1% each way.
        let ratio_down = (as_f64(down) / as_f64(price)).powi(2);
        let ratio_up = (as_f64(up) / as_f64(price)).powi(2);
        assert!((ratio_down - 0.99).abs() < 1e-8, "{ratio_down}");
        assert!((ratio_up - 1.01).abs() < 1e-8, "{ratio_up}");
        // Never past the tolerance, even by a unit of rounding.
        let floor_down = price * U256::from(isqrt(990_000_000_000_000_000)) / u(1_000_000_000);
        assert!(down >= floor_down);

        // Kept inside the pool's own limits.
        let one = U256::from(1u8);
        assert_eq!(price_limit(price, true, PPM).unwrap(), MIN_SQRT_RATIO + one);
        assert_eq!(
            price_limit(MAX_SQRT_RATIO - one, false, 500_000).unwrap(),
            MAX_SQRT_RATIO - one
        );
        assert_eq!(price_limit(price, true, 0), None);
        assert_eq!(price_limit(price, false, PPM + 1), None);
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(15), 3);
        assert_eq!(isqrt(16), 4);
        assert_eq!(isqrt(u128::MAX), u64::MAX as u128);
    }

    #[test]
    fn a_one_sided_open_approves_the_manager_and_mints_to_the_safe() {
        let ctx = ctx();
        // Dollars below the price: token0 only.
        let records = [record(
            "open",
            mint_step(-200_100, -199_500, 10_000_000_000, 0),
        )];
        let planned = plan(&records, &ctx).unwrap();
        assert_eq!(shape(&planned), ["approve", "mint"]);
        let (spender, amount) = decode_approve(&planned.inner[0].data).unwrap();
        assert_eq!(planned.inner[0].to, TOKEN0);
        assert_eq!((spender, amount), (MANAGER, u(10_000_000_000)));
        let m = decode_mint(&planned.inner[1].data).unwrap();
        assert_eq!((m.token0, m.token1, m.fee), (TOKEN0, TOKEN1, 500));
        assert_eq!((m.tick_lower, m.tick_upper), (-200_100, -199_500));
        assert_eq!(m.amount0_desired, u(10_000_000_000));
        assert_eq!(m.amount0_min, min(10_000_000_000));
        assert_eq!(m.amount0_min, u(9_950_000_000));
        assert_eq!((m.amount1_desired, m.amount1_min), (U256::ZERO, U256::ZERO));
        assert_eq!(m.recipient, SAFE);
        assert_eq!(m.deadline, U256::from(ctx.deadline));
        assert_eq!(planned.bounds.len(), 2);
        assert_eq!(planned.bounds[0].field, "amount0_min");
        assert_eq!(planned.bounds[0].bound, m.amount0_min);
        assert_eq!(planned.allowances_after, Allowances::default());
        assert!(!planned.burns_position);
        check_wrapping(&planned, &ctx);

        // With the allowance already in place, no approval.
        let allowed = PlanContext {
            allowances: Allowances {
                manager0: u(20_000_000_000),
                ..Allowances::default()
            },
            ..ctx
        };
        let planned = plan(&records, &allowed).unwrap();
        assert_eq!(shape(&planned), ["mint"]);
        assert_eq!(planned.allowances_after.manager0, u(10_000_000_000));
    }

    #[test]
    fn a_close_burns_collects_to_the_safe_and_swaps_the_other_token_back() {
        let ctx = PlanContext {
            position: open(-200_100, -199_500, 5_000_000),
            ..ctx()
        };
        let records = [
            record(
                "close",
                burn_step(
                    -200_100,
                    -199_500,
                    5_000_000,
                    4_000_000_000,
                    2_000_000_000_000_000_000,
                ),
            ),
            record(
                "close",
                swap_step(false, 2_000_000_000_000_000_022, 6_000_000_000),
            ),
        ];
        let planned = plan(&records, &ctx).unwrap();
        assert_eq!(
            shape(&planned),
            ["decrease", "collect", "burn", "approve", "swap"]
        );
        let d = decode_decrease_liquidity(&planned.inner[0].data).unwrap();
        assert_eq!(d.token_id, U256::from(TOKEN_ID));
        assert_eq!(d.liquidity, 5_000_000);
        assert_eq!(d.amount0_min, min(4_000_000_000));
        assert_eq!(d.amount1_min, min(2_000_000_000_000_000_000));
        let c = decode_collect(&planned.inner[1].data).unwrap();
        assert_eq!(c.recipient, SAFE);
        assert_eq!((c.amount0_max, c.amount1_max), (u128::MAX, u128::MAX));
        assert_eq!(
            decode_burn(&planned.inner[2].data).unwrap(),
            U256::from(TOKEN_ID)
        );
        assert_eq!(planned.inner[3].to, TOKEN1);
        assert_eq!(
            decode_approve(&planned.inner[3].data).unwrap(),
            (ROUTER, u(2_000_000_000_000_000_022))
        );
        let s = decode_exact_input_single(&planned.inner[4].data).unwrap();
        assert_eq!((s.token_in, s.token_out, s.fee), (TOKEN1, TOKEN0, 500));
        assert_eq!(s.recipient, SAFE);
        assert_eq!(s.amount_in, u(2_000_000_000_000_000_022));
        assert_eq!(s.amount_out_minimum, min(6_000_000_000));
        assert_eq!(
            s.sqrt_price_limit_x96,
            price_limit(u(SQRT_PRICE), false, ctx.price_limit_ppm).unwrap()
        );
        assert!(
            s.sqrt_price_limit_x96 > u(SQRT_PRICE),
            "token1 in: the price rises"
        );
        assert!(planned.burns_position);
        let fields: Vec<_> = planned.bounds.iter().map(|b| b.field).collect();
        assert_eq!(
            fields,
            [
                "amount0_min",
                "amount1_min",
                "amount_out_minimum",
                "sqrt_price_limit_x96"
            ]
        );
        assert_eq!(planned.bounds[3].recorded, u(SQRT_PRICE));
        check_wrapping(&planned, &ctx);
    }

    #[test]
    fn a_withdrawal_burns_the_nft_only_when_it_empties_the_position() {
        let records = [record(
            "withdraw",
            burn_step(-200_100, -199_500, 5_000_000, 9_000_000_000, 0),
        )];
        let whole = PlanContext {
            position: open(-200_100, -199_500, 5_000_000),
            ..ctx()
        };
        let planned = plan(&records, &whole).unwrap();
        assert_eq!(shape(&planned), ["decrease", "collect", "burn"]);
        assert!(planned.burns_position);
        let d = decode_decrease_liquidity(&planned.inner[0].data).unwrap();
        assert_eq!(
            (d.amount0_min, d.amount1_min),
            (min(9_000_000_000), U256::ZERO)
        );

        // Part of a larger position: no burn of the NFT.
        let larger = PlanContext {
            position: open(-200_100, -199_500, 8_000_000),
            ..ctx()
        };
        let planned = plan(&records, &larger).unwrap();
        assert_eq!(shape(&planned), ["decrease", "collect"]);
        assert!(!planned.burns_position);
        check_wrapping(&planned, &larger);
    }

    #[test]
    fn a_flip_burns_and_mints_the_other_token_on_the_other_side() {
        let ctx = PlanContext {
            position: open(-200_100, -199_500, 5_000_000),
            allowances: Allowances {
                manager1: u(1),
                ..Allowances::default()
            },
            ..ctx()
        };
        let records = [
            record(
                "flip",
                burn_step(-200_100, -199_500, 5_000_000, 0, 3_000_000_000_000_000_000),
            ),
            record(
                "flip",
                mint_step(-200_700, -200_100, 0, 2_999_999_999_999_999_990),
            ),
        ];
        let planned = plan(&records, &ctx).unwrap();
        assert_eq!(
            shape(&planned),
            ["decrease", "collect", "burn", "approve", "mint"]
        );
        let m = decode_mint(&planned.inner[4].data).unwrap();
        assert_eq!((m.tick_lower, m.tick_upper), (-200_700, -200_100));
        assert_eq!(m.amount1_desired, u(2_999_999_999_999_999_990));
        assert_eq!(m.amount1_min, min(2_999_999_999_999_999_990));
        assert_eq!(m.recipient, SAFE);
        assert!(planned.burns_position);
        check_wrapping(&planned, &ctx);
    }

    #[test]
    fn a_recentre_and_an_open_around_the_price_swap_before_they_mint() {
        let generous = Allowances {
            manager0: U256::MAX,
            manager1: U256::MAX,
            router0: U256::MAX,
            router1: U256::MAX,
        };
        let ctx = PlanContext {
            position: open(-200_100, -199_500, 5_000_000),
            allowances: generous,
            ..ctx()
        };
        let records = [
            record(
                "recenter",
                burn_step(-200_100, -199_500, 5_000_000, 0, 3_000_000_000_000_000_000),
            ),
            record(
                "recenter",
                swap_step(false, 1_500_000_000_000_000_000, 4_500_000_000),
            ),
            record(
                "recenter",
                mint_step(-198_900, -198_300, 4_400_000_000, 1_500_000_000_000_000_000),
            ),
        ];
        let planned = plan(&records, &ctx).unwrap();
        assert_eq!(
            shape(&planned),
            ["decrease", "collect", "burn", "swap", "mint"]
        );
        let m = decode_mint(&planned.inner[4].data).unwrap();
        assert_eq!(
            (m.amount0_min, m.amount1_min),
            (min(4_400_000_000), min(1_500_000_000_000_000_000))
        );
        check_wrapping(&planned, &ctx);

        // An open around the price, from dollars: swap some, then mint both.
        let fresh = super::tests::ctx();
        let records = [
            record(
                "open",
                swap_step(true, 5_000_000_000, 1_600_000_000_000_000_000),
            ),
            record(
                "open",
                mint_step(-200_100, -199_500, 4_990_000_000, 1_600_000_000_000_000_000),
            ),
        ];
        let planned = plan(&records, &fresh).unwrap();
        assert_eq!(
            shape(&planned),
            ["approve", "swap", "approve", "approve", "mint"]
        );
        let spenders: Vec<_> = [0, 2, 3]
            .iter()
            .map(|i| {
                (
                    planned.inner[*i].to,
                    decode_approve(&planned.inner[*i].data).unwrap().0,
                )
            })
            .collect();
        assert_eq!(
            spenders,
            [(TOKEN0, ROUTER), (TOKEN0, MANAGER), (TOKEN1, MANAGER)]
        );
        let s = decode_exact_input_single(&planned.inner[1].data).unwrap();
        assert!(
            s.sqrt_price_limit_x96 < u(SQRT_PRICE),
            "token0 in: the price falls"
        );
        check_wrapping(&planned, &fresh);
    }

    #[test]
    fn a_token_that_must_go_to_zero_first_gets_two_approvals() {
        let ctx = PlanContext {
            allowances: Allowances {
                manager0: u(5),
                ..Allowances::default()
            },
            zero_first: vec![TOKEN0],
            ..ctx()
        };
        let records = [record(
            "open",
            mint_step(-200_100, -199_500, 10_000_000_000, 0),
        )];
        let planned = plan(&records, &ctx).unwrap();
        assert_eq!(shape(&planned), ["approve", "approve", "mint"]);
        assert_eq!(
            decode_approve(&planned.inner[0].data).unwrap(),
            (MANAGER, U256::ZERO)
        );
        assert_eq!(
            decode_approve(&planned.inner[1].data).unwrap(),
            (MANAGER, u(10_000_000_000))
        );
        // From zero, one approval is enough.
        let from_zero = PlanContext {
            allowances: Allowances::default(),
            ..ctx
        };
        assert_eq!(
            shape(&plan(&records, &from_zero).unwrap()),
            ["approve", "mint"]
        );
    }

    #[test]
    fn what_cannot_be_planned_safely_is_refused_with_what_to_do() {
        let base = ctx();
        let mint_rec = record("open", mint_step(-200_100, -199_500, 10, 0));
        assert_eq!(plan(&[], &base), Err(PlanError::Empty));

        let other_block = StepRecord {
            block: 21_000_001,
            ..mint_rec.clone()
        };
        let mixed = plan(&[mint_rec.clone(), other_block], &base).unwrap_err();
        assert!(matches!(mixed, PlanError::Mixed { .. }));
        assert!(mixed.to_string().contains("one action at a time"));
        let other_action = StepRecord {
            action: "close".into(),
            ..mint_rec.clone()
        };
        assert!(matches!(
            plan(&[mint_rec.clone(), other_action], &base),
            Err(PlanError::Mixed { .. })
        ));

        let burn_rec = record("withdraw", burn_step(-200_100, -199_500, 5, 10, 0));
        let none = plan(std::slice::from_ref(&burn_rec), &base).unwrap_err();
        assert_eq!(none, PlanError::NoPosition { record: 0 });
        assert!(none.to_string().contains("read the Safe's position"));
        let elsewhere = PlanContext {
            position: open(-100, 500, 5),
            ..base.clone()
        };
        assert!(matches!(
            plan(std::slice::from_ref(&burn_rec), &elsewhere),
            Err(PlanError::OtherPosition { .. })
        ));
        let smaller = PlanContext {
            position: open(-200_100, -199_500, 4),
            ..base.clone()
        };
        assert_eq!(
            plan(std::slice::from_ref(&burn_rec), &smaller),
            Err(PlanError::BurnTooLarge {
                record: 0,
                wanted: 5,
                held: 4
            })
        );

        for ppm in [0, PPM + 1] {
            let loose = PlanContext {
                slippage_ppm: ppm,
                ..base.clone()
            };
            let refused = plan(std::slice::from_ref(&mint_rec), &loose).unwrap_err();
            assert!(
                refused.to_string().contains("between 1 and 1000000"),
                "{refused}"
            );
            let limit = PlanContext {
                price_limit_ppm: ppm,
                ..base.clone()
            };
            assert!(plan(std::slice::from_ref(&mint_rec), &limit).is_err());
        }
        let swapped = PlanContext {
            token0: TOKEN1,
            token1: TOKEN0,
            ..base.clone()
        };
        assert!(matches!(
            plan(std::slice::from_ref(&mint_rec), &swapped),
            Err(PlanError::TokenOrder { .. })
        ));
        let foreign = address!("00000000000000000000000000000000000000ff");
        let usdt_elsewhere = PlanContext {
            zero_first: vec![foreign],
            ..base.clone()
        };
        assert_eq!(
            plan(std::slice::from_ref(&mint_rec), &usdt_elsewhere),
            Err(PlanError::ForeignToken { token: foreign })
        );
        let no_safe = PlanContext {
            safe: Address::ZERO,
            ..base.clone()
        };
        assert_eq!(
            plan(std::slice::from_ref(&mint_rec), &no_safe),
            Err(PlanError::Unset { what: "the Safe" })
        );
        let no_chain = PlanContext {
            chain_id: 0,
            ..base.clone()
        };
        assert_eq!(
            plan(std::slice::from_ref(&mint_rec), &no_chain),
            Err(PlanError::ChainId)
        );

        let off_grid = record("open", mint_step(-200_105, -199_500, 10, 0));
        assert!(matches!(
            plan(&[off_grid], &base),
            Err(PlanError::Ticks { .. })
        ));
        let backwards = record("open", mint_step(-199_500, -200_100, 10, 0));
        assert!(matches!(
            plan(&[backwards], &base),
            Err(PlanError::Ticks { .. })
        ));
        let nothing = record("open", mint_step(-200_100, -199_500, 0, 0));
        assert_eq!(
            plan(&[nothing], &base),
            Err(PlanError::MintsNothing { record: 0 })
        );
        let empty_swap = record("close", swap_step(true, 0, 5));
        assert_eq!(
            plan(&[empty_swap], &base),
            Err(PlanError::SwapsNothing { record: 0 })
        );
    }

    #[test]
    fn verify_refuses_a_plan_that_pays_anyone_else_or_calls_elsewhere() {
        let ctx = ctx();
        let records = [record("open", mint_step(-200_100, -199_500, 10_000, 0))];
        let planned = plan(&records, &ctx).unwrap();
        assert_eq!(verify(&planned, &ctx), Ok(()));

        // The mint's recipient changed after planning.
        let thief = address!("00000000000000000000000000000000000000ee");
        let mut params = decode_mint(&planned.inner[1].data).unwrap();
        params.recipient = thief;
        let mut tampered = planned.clone();
        tampered.inner[1] = mint(MANAGER, &params).unwrap();
        assert_eq!(
            verify(&tampered, &ctx),
            Err(PlanError::Recipient {
                index: 1,
                found: thief,
                safe: SAFE
            })
        );

        // A call to a contract the plan has no business with.
        let mut elsewhere = planned.clone();
        elsewhere.inner.push(approve(thief, MANAGER, u(1)));
        assert_eq!(
            verify(&elsewhere, &ctx),
            Err(PlanError::Target {
                index: 2,
                found: thief
            })
        );

        // An approval for someone else.
        let mut spender = planned.clone();
        spender.inner[0] = approve(TOKEN0, thief, u(1));
        assert!(matches!(
            verify(&spender, &ctx),
            Err(PlanError::Unexpected { index: 0, .. })
        ));

        // A batch that is not the inner calls.
        let mut batch = planned.clone();
        batch.inner.truncate(1);
        assert!(matches!(
            verify(&batch, &ctx),
            Err(PlanError::Unexpected { .. })
        ));

        // A roles call with another role.
        let mut role = planned.clone();
        role.outer = exec_transaction_with_role(ROLES, &planned.batch, B256::ZERO, true);
        assert!(matches!(
            verify(&role, &ctx),
            Err(PlanError::Unexpected { .. })
        ));

        // Each call on its own, with no delegate call: one transaction per
        // inner call, each a plain call through the roles module.
        assert_eq!(planned.separate.len(), planned.inner.len());
        for (each, inner) in planned.separate.iter().zip(&planned.inner) {
            let roles = decode_exec_transaction_with_role(&each.data).unwrap();
            assert_eq!((each.to, each.operation), (ROLES, Operation::Call));
            assert_eq!(&roles.inner, inner);
            assert_eq!(roles.inner.operation, Operation::Call);
        }
        // Two of them swapped, one missing, or one made a delegate call:
        // refused.
        let mut swapped = planned.clone();
        swapped.separate.swap(0, 1);
        assert!(matches!(
            verify(&swapped, &ctx),
            Err(PlanError::Unexpected { .. })
        ));
        let mut short = planned.clone();
        short.separate.pop();
        assert!(matches!(
            verify(&short, &ctx),
            Err(PlanError::Unexpected { .. })
        ));
        let mut delegated = planned.clone();
        let mut inner = planned.inner[0].clone();
        inner.operation = Operation::DelegateCall;
        delegated.separate[0] = exec_transaction_with_role(ROLES, &inner, ctx.role_key, true);
        assert!(matches!(
            verify(&delegated, &ctx),
            Err(PlanError::Unexpected { .. })
        ));

        // A delegate call inside.
        let mut delegate = planned;
        delegate.inner[0].operation = Operation::DelegateCall;
        assert!(matches!(
            verify(&delegate, &ctx),
            Err(PlanError::Unexpected { index: 0, .. })
        ));
    }
}
