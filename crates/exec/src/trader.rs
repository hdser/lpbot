//! The live trader's executing half (`docs/PLAN.md`, pieces 1, 4 and 5):
//! what the engine did to its position, carried out on the chain.
//!
//! The engine decides as in a backtest and records each action as steps
//! (`lpbot_core::StepRecord`). The trader does not send those amounts as
//! they are: it carries the action out on what the chain holds now
//! ([`anchor`]). A mint puts in what the Safe has of the token, at most what
//! the engine put in. A burn takes out the position's real liquidity at the
//! pool's price now. A swap sells what the Safe holds of the token, bounded
//! by the engine's own exchange rate. Then the risk checks, the pause flag,
//! the planner's bounds, the journal before every step, and reading the
//! position back. The engine's simulated position is not changed by any of
//! this: the gap between the live run and its simulated twin is measured,
//! not hidden (`docs/PLAN.md`, piece 3, the tracking gate).
//!
//! Supported: a one-sided entry, a close, a withdrawal and a flip. An entry
//! that holds both tokens (around the price) is refused with a message,
//! for now.

use alloy_primitives::{Address, Bytes, B256, U256};
use lpbot_core::{Step, StepRecord};
use lpbot_risk::checks::{self, ActionLog, Kind, Verdict};
use lpbot_risk::pause::State;
use lpbot_signer::{Eip1559Tx, SignedTx};
use lpbot_venues::uniswap_v3::liquidity_amounts::{
    get_amounts_for_liquidity, get_liquidity_for_amount0, get_liquidity_for_amount1,
};
use lpbot_venues::uniswap_v3::sqrt_price_math::{get_amount0_delta, get_amount1_delta};
use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

use crate::abi::selector;
use crate::calls::Call;
use crate::journal::{Entry, Journal};
use crate::planner::{less_slippage, plan, Allowances, OpenPosition, PlanContext, Planned};
use crate::reconcile::{self, Expected, Outcome};
use crate::submit::{submit, FeePolicy, Landed, Node, Order};

/// How the trader's transactions reach the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Custody {
    /// The Safe, through the Roles modifier: each call on its own, as a
    /// plain call, sent by the keeper.
    Roles {
        safe: Address,
        roles: Address,
        role_key: B256,
        keeper: Address,
    },
    /// A plain account standing in for the Safe, on a local fork only
    /// (chain id 31337): what the fork tests use.
    TestAccount { account: Address },
}

/// The chain id of a local Anvil fork: the only chain a test account is
/// accepted on.
pub const LOCAL_CHAIN_ID: u64 = 31_337;

impl Custody {
    /// The address that holds the funds and receives every token.
    pub fn holder(&self) -> Address {
        match self {
            Custody::Roles { safe, .. } => *safe,
            Custody::TestAccount { account } => *account,
        }
    }

    /// The address that sends the transactions.
    pub fn sender(&self) -> Address {
        match self {
            Custody::Roles { keeper, .. } => *keeper,
            Custody::TestAccount { account } => *account,
        }
    }
}

/// The pool a run trades.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInfo {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    /// Tokens whose allowance must go to zero before a new one (USDT).
    pub zero_first: Vec<Address>,
}

/// The contracts the calls go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Contracts {
    pub position_manager: Address,
    pub router: Address,
    pub multisend: Address,
}

/// The limits the trader holds every action to (the risk engine's
/// pre-trade checks, `lpbot_risk::checks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub min_width_ticks: i32,
    /// The planner's tolerance on every amount, in parts per million.
    pub slippage_ppm: u32,
    /// The most that tolerance may be.
    pub max_slippage_ppm: u32,
    /// How far a swap may move the price, for its price limit.
    pub price_limit_ppm: u32,
    pub max_swap_impact_ppm: u32,
    pub exit_max_swap_impact_ppm: u32,
    pub max_actions_per_day: usize,
}

/// The pool's price now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Market {
    pub sqrt_price_x96: U256,
    pub tick: i32,
}

/// What the holder has now: both tokens, and its open position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Book {
    pub balance0: U256,
    pub balance1: U256,
    pub position: Option<OpenPosition>,
}

/// Why an action is not carried out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("{0}")]
    Check(String),
    #[error(
        "the engine's action `{action}` holds both tokens in [{lower}, {upper}); entries around \
         the price are not carried out live yet. The run's entries stay held"
    )]
    Mixed {
        action: String,
        lower: i32,
        upper: i32,
    },
    #[error(
        "the price moved into [{lower}, {upper}) since the engine decided; a one-sided entry \
         would now need both tokens. Not sent; the engine's next decision is waited for"
    )]
    PriceInRange { lower: i32, upper: i32 },
    #[error(
        "the engine takes out [{lower}, {upper}) but the holder's position is {held}; read the \
         chain back (`reconcile`) before anything else is sent"
    )]
    NoSuchPosition {
        lower: i32,
        upper: i32,
        held: String,
    },
    #[error("nothing to do: {0}")]
    Nothing(String),
    #[error("the pool maths failed: {0}")]
    Math(String),
}

fn math<E: std::fmt::Debug>(e: E) -> Refusal {
    Refusal::Math(format!("{e:?}"))
}

/// The engine's action carried out on what the chain holds: the same
/// changes, with amounts from the holder's balances, the position's real
/// liquidity and the price now. Pure: everything it needs is passed in.
pub fn anchor(
    action: &str,
    sim: &[StepRecord],
    market: &Market,
    book: &Book,
) -> Result<Vec<StepRecord>, Refusal> {
    let mut holdings = [book.balance0, book.balance1];
    let mut position = book.position;
    let mut live = Vec::new();
    for record in sim {
        let step = match &record.step {
            Step::Mint {
                tick_lower,
                tick_upper,
                amount0,
                amount1,
                ..
            } => {
                if !amount0.is_zero() && !amount1.is_zero() {
                    return Err(Refusal::Mixed {
                        action: action.to_string(),
                        lower: *tick_lower,
                        upper: *tick_upper,
                    });
                }
                let sqrt_a = get_sqrt_ratio_at_tick(*tick_lower).map_err(math)?;
                let sqrt_b = get_sqrt_ratio_at_tick(*tick_upper).map_err(math)?;
                let price = market.sqrt_price_x96;
                // Below the range it takes token0 only, above it token1.
                let token = if price <= sqrt_a {
                    0
                } else if price >= sqrt_b {
                    1
                } else {
                    return Err(Refusal::PriceInRange {
                        lower: *tick_lower,
                        upper: *tick_upper,
                    });
                };
                let wanted = if token == 0 { *amount0 } else { *amount1 };
                let amount = wanted.min(holdings[token]);
                let liquidity = if token == 0 {
                    get_liquidity_for_amount0(sqrt_a, sqrt_b, amount).map_err(math)?
                } else {
                    get_liquidity_for_amount1(sqrt_a, sqrt_b, amount).map_err(math)?
                };
                if liquidity == 0 {
                    return Err(Refusal::Nothing(format!(
                        "the holder has none of the token [{tick_lower}, {tick_upper}) takes"
                    )));
                }
                let used = if token == 0 {
                    get_amount0_delta(sqrt_a, sqrt_b, liquidity, true).map_err(math)?
                } else {
                    get_amount1_delta(sqrt_a, sqrt_b, liquidity, true).map_err(math)?
                };
                holdings[token] -= used.min(holdings[token]);
                position = Some(OpenPosition {
                    token_id: U256::ZERO,
                    tick_lower: *tick_lower,
                    tick_upper: *tick_upper,
                    liquidity,
                });
                let (amount0, amount1) = if token == 0 {
                    (used, U256::ZERO)
                } else {
                    (U256::ZERO, used)
                };
                Step::Mint {
                    tick_lower: *tick_lower,
                    tick_upper: *tick_upper,
                    liquidity,
                    amount0,
                    amount1,
                }
            }
            Step::Burn {
                tick_lower,
                tick_upper,
                ..
            } => {
                let held = match position {
                    Some(p) if (p.tick_lower, p.tick_upper) == (*tick_lower, *tick_upper) => p,
                    other => {
                        return Err(Refusal::NoSuchPosition {
                            lower: *tick_lower,
                            upper: *tick_upper,
                            held: other.map_or("none".into(), |p| {
                                format!("[{}, {})", p.tick_lower, p.tick_upper)
                            }),
                        })
                    }
                };
                let sqrt_a = get_sqrt_ratio_at_tick(*tick_lower).map_err(math)?;
                let sqrt_b = get_sqrt_ratio_at_tick(*tick_upper).map_err(math)?;
                let (amount0, amount1) = get_amounts_for_liquidity(
                    market.sqrt_price_x96,
                    sqrt_a,
                    sqrt_b,
                    held.liquidity,
                )
                .map_err(math)?;
                holdings[0] += amount0;
                holdings[1] += amount1;
                position = None;
                Step::Burn {
                    tick_lower: *tick_lower,
                    tick_upper: *tick_upper,
                    liquidity: held.liquidity,
                    amount0,
                    amount1,
                    fees0: U256::ZERO,
                    fees1: U256::ZERO,
                }
            }
            Step::Swap {
                zero_for_one,
                amount_in,
                amount_out,
            } => {
                let (from, to) = if *zero_for_one { (0, 1) } else { (1, 0) };
                let sell = holdings[from];
                if sell.is_zero() || amount_in.is_zero() {
                    continue;
                }
                // The engine's own rate, which its replica had at the block.
                let out = sell * *amount_out / *amount_in;
                holdings[from] = U256::ZERO;
                holdings[to] += out;
                Step::Swap {
                    zero_for_one: *zero_for_one,
                    amount_in: sell,
                    amount_out: out,
                }
            }
        };
        live.push(StepRecord {
            sqrt_price_x96: market.sqrt_price_x96,
            step,
            ..record.clone()
        });
    }
    if live.is_empty() {
        return Err(Refusal::Nothing(format!(
            "`{action}` changes nothing the holder has"
        )));
    }
    Ok(live)
}

/// Whether an action adds exposure (an entry) or takes it away.
pub fn kind_of(action: &str) -> Kind {
    match action {
        "close" | "withdraw" => Kind::Exit,
        _ => Kind::Entry,
    }
}

/// The pre-trade checks on an anchored action. `base_fee` and `gas_cap`
/// in wei a gas.
#[allow(clippy::too_many_arguments)]
pub fn check(
    action: &str,
    live: &[StepRecord],
    market: &Market,
    limits: &Limits,
    pause: &State,
    base_fee: u128,
    gas_cap: u128,
    log: &mut ActionLog,
    pool: &str,
    now: u64,
) -> Verdict {
    let kind = kind_of(action);
    let paused = match (pause, kind) {
        (State::Running, _) | (_, Kind::Exit) => Verdict::Allow,
        (State::Paused(why) | State::Exiting(why), Kind::Entry) => {
            Verdict::Defer(format!("the pause flag holds entries: {why}"))
        }
    };
    let mut verdicts = vec![
        paused,
        checks::check_slippage(limits.slippage_ppm, limits.max_slippage_ppm),
        checks::check_gas(base_fee.saturating_mul(2), gas_cap, kind),
        log.check(pool, now, limits.max_actions_per_day, kind),
    ];
    let price = {
        let p = f64::from(market.sqrt_price_x96) / 2f64.powi(96);
        p * p
    };
    for record in live {
        match &record.step {
            Step::Mint {
                tick_lower,
                tick_upper,
                ..
            } => verdicts.push(checks::check_width(
                *tick_lower,
                *tick_upper,
                limits.min_width_ticks,
            )),
            Step::Swap {
                zero_for_one,
                amount_in,
                amount_out,
            } => {
                let impact = checks::swap_impact_ppm(
                    *zero_for_one,
                    f64::from(*amount_in),
                    f64::from(*amount_out),
                    price,
                );
                verdicts.push(checks::check_swap(
                    impact,
                    kind,
                    limits.max_swap_impact_ppm,
                    limits.exit_max_swap_impact_ppm,
                ));
            }
            Step::Burn { .. } => {}
        }
    }
    Verdict::all(verdicts)
}

/// The calls a custody sends for a plan, in order.
pub fn transactions(planned: &Planned, custody: &Custody) -> Vec<Call> {
    match custody {
        Custody::Roles { .. } => planned.separate.clone(),
        Custody::TestAccount { .. } => planned.inner.clone(),
    }
}

/// What a carried-out action came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acted {
    pub live: Vec<StepRecord>,
    pub landed: Vec<Landed>,
    pub outcome: Outcome,
}

/// The trader: one run's pool, one custody, one journal.
pub struct Trader<'a> {
    pub node: &'a dyn Node,
    pub chain_id: u64,
    pub custody: Custody,
    pub contracts: Contracts,
    pub pool: PoolInfo,
    pub limits: Limits,
    pub fees: FeePolicy,
    /// The highest fee a gas an entry pays.
    pub gas_cap: u128,
    pub journal: Journal,
    pub actions: ActionLog,
}

fn word(data: &[u8], i: usize) -> anyhow::Result<U256> {
    data.get(i * 32..(i + 1) * 32)
        .map(U256::from_be_slice)
        .ok_or_else(|| anyhow::anyhow!("the answer is too short for word {i}"))
}

fn call_data(sig: &str, args: &[U256]) -> Vec<u8> {
    let mut data = selector(sig).to_vec();
    for a in args {
        data.extend_from_slice(&a.to_be_bytes::<32>());
    }
    data
}

fn address_arg(a: Address) -> U256 {
    U256::from_be_slice(a.as_slice())
}

impl Trader<'_> {
    /// Refuses a custody the chain does not allow: a test account anywhere
    /// but a local fork.
    pub fn check_custody(&self) -> anyhow::Result<()> {
        if matches!(self.custody, Custody::TestAccount { .. }) && self.chain_id != LOCAL_CHAIN_ID {
            anyhow::bail!(
                "a test account stands in for the Safe only on a local fork (chain id \
                 {LOCAL_CHAIN_ID}); this node answers chain id {}",
                self.chain_id
            );
        }
        Ok(())
    }

    /// The pool's price now.
    pub fn market(&self) -> anyhow::Result<Market> {
        let data = self.node.call(
            self.custody.sender(),
            self.pool.address,
            &selector("slot0()"),
        )?;
        let sqrt_price_x96 = word(&data, 0)?;
        let tick_word = data
            .get(32..64)
            .ok_or_else(|| anyhow::anyhow!("slot0 answered too few bytes"))?;
        let tick = i32::from_be_bytes(tick_word[28..32].try_into().expect("four bytes"));
        Ok(Market {
            sqrt_price_x96,
            tick,
        })
    }

    /// What the holder has now: both tokens and its newest position in this
    /// pool, if it holds liquidity.
    pub fn book(&self) -> anyhow::Result<Book> {
        let holder = self.custody.holder();
        let balance0 = reconcile::balance(self.node, self.pool.token0, holder)?;
        let balance1 = reconcile::balance(self.node, self.pool.token1, holder)?;
        let manager = self.contracts.position_manager;
        let position = match reconcile::newest_token(self.node, manager, holder)? {
            None => None,
            Some(token_id) => {
                let p = reconcile::read_position(self.node, manager, holder, token_id)?;
                (p.liquidity > 0
                    && (p.token0, p.token1, p.fee)
                        == (self.pool.token0, self.pool.token1, self.pool.fee))
                    .then_some(OpenPosition {
                        token_id,
                        tick_lower: p.tick_lower,
                        tick_upper: p.tick_upper,
                        liquidity: p.liquidity,
                    })
            }
        };
        Ok(Book {
            balance0,
            balance1,
            position,
        })
    }

    fn allowance(&self, token: Address, spender: Address) -> anyhow::Result<U256> {
        let data = self.node.call(
            self.custody.sender(),
            token,
            &call_data(
                "allowance(address,address)",
                &[address_arg(self.custody.holder()), address_arg(spender)],
            ),
        )?;
        word(&data, 0)
    }

    fn context(&self, position: Option<OpenPosition>, now: u64) -> anyhow::Result<PlanContext> {
        let (roles, role_key) = match &self.custody {
            Custody::Roles {
                roles, role_key, ..
            } => (*roles, *role_key),
            // The planner always builds the roles transactions too; a test
            // account sends the inner calls and never these.
            Custody::TestAccount { .. } => (Address::repeat_byte(0x01), B256::repeat_byte(0x01)),
        };
        let manager = self.contracts.position_manager;
        let router = self.contracts.router;
        Ok(PlanContext {
            chain_id: self.chain_id,
            safe: self.custody.holder(),
            position_manager: manager,
            router,
            multisend: self.contracts.multisend,
            roles,
            role_key,
            token0: self.pool.token0,
            token1: self.pool.token1,
            fee: self.pool.fee,
            tick_spacing: self.pool.tick_spacing,
            position,
            allowances: Allowances {
                manager0: self.allowance(self.pool.token0, manager)?,
                manager1: self.allowance(self.pool.token1, manager)?,
                router0: self.allowance(self.pool.token0, router)?,
                router1: self.allowance(self.pool.token1, router)?,
            },
            zero_first: self.pool.zero_first.clone(),
            slippage_ppm: self.limits.slippage_ppm,
            price_limit_ppm: self.limits.price_limit_ppm,
            deadline: now + 600,
        })
    }

    /// Carries out one action of the engine: anchors it on the chain,
    /// checks it, plans it, sends each transaction in order, and reads the
    /// position back. Every step is journaled under `id` before it is
    /// taken. A refusal sends nothing and says why.
    #[allow(clippy::too_many_arguments)]
    pub fn act(
        &mut self,
        id: &str,
        run: &str,
        action: &str,
        sim: &[StepRecord],
        pause: &State,
        now: u64,
        sign: &mut dyn FnMut(&Eip1559Tx) -> anyhow::Result<SignedTx>,
        wait: &mut dyn FnMut(),
    ) -> anyhow::Result<Result<Acted, Refusal>> {
        self.check_custody()?;
        let market = self.market()?;
        let book = self.book()?;
        let live = match anchor(action, sim, &market, &book) {
            Ok(live) => live,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let pool = format!("{:#x}", self.pool.address);
        match check(
            action,
            &live,
            &market,
            &self.limits,
            pause,
            self.node.base_fee()?,
            self.gas_cap,
            &mut self.actions,
            &pool,
            now,
        ) {
            Verdict::Allow => {}
            Verdict::Defer(why) | Verdict::Veto(why) => return Ok(Err(Refusal::Check(why))),
        }
        self.journal.append(&Entry::Intent {
            id: id.into(),
            run: run.into(),
            campaign: live.first().map_or(0, |r| r.campaign),
            action: action.into(),
            steps: live.clone(),
        })?;
        let planned =
            plan(&live, &self.context(book.position, now)?).map_err(|e| anyhow::anyhow!("{e}"))?;
        let calls = transactions(&planned, &self.custody);
        let mut landed = Vec::new();
        for (index, call) in calls.iter().enumerate() {
            self.journal.append(&Entry::Planned {
                id: format!("{id}/{index}"),
                to: format!("{:#x}", call.to),
                calldata_sha256: hex::encode(alloy_primitives::keccak256(&call.data)),
            })?;
            let order = Order {
                id: format!("{id}/{index}"),
                chain_id: self.chain_id,
                from: self.custody.sender(),
                to: call.to,
                data: Bytes::from(call.data.clone()),
                exit: kind_of(action) == Kind::Exit,
            };
            let done = submit(self.node, sign, &order, &self.fees, &self.journal, wait, 60)?;
            if !done.success {
                self.journal.append(&Entry::Abandoned {
                    id: id.into(),
                    reason: format!("transaction {index} reverted ({})", done.hash),
                })?;
                anyhow::bail!(
                    "transaction {index} of `{action}` reverted ({}); the rest was not sent. \
                     Read the chain back before the next action",
                    done.hash
                );
            }
            landed.push(done);
        }
        self.actions.record(&pool, now);
        // Read back: the range it made, or nothing once taken out.
        let expected = match live.last().map(|r| &r.step) {
            Some(Step::Mint {
                tick_lower,
                tick_upper,
                liquidity,
                ..
            }) => Expected::Position {
                tick_lower: *tick_lower,
                tick_upper: *tick_upper,
                min_liquidity: less_slippage(U256::from(*liquidity), self.limits.slippage_ppm)
                    .map_or(0, |l| l.to::<u128>()),
            },
            _ => Expected::None,
        };
        let after = self.book()?;
        let found = match (&expected, after.position) {
            (Expected::Position { .. }, Some(p)) => Some(reconcile::read_position(
                self.node,
                self.contracts.position_manager,
                self.custody.holder(),
                p.token_id,
            )?),
            _ => None,
        };
        let outcome = reconcile::compare(found, &expected);
        let (token_id, liquidity) = match &outcome {
            Outcome::Adopt(Some(p)) => (Some(p.token_id), p.liquidity),
            _ => (None, 0),
        };
        self.journal.append(&Entry::Reconciled {
            id: id.into(),
            token_id,
            liquidity,
            note: match &outcome {
                Outcome::Adopt(_) => "as planned".into(),
                Outcome::Differs(why) => why.clone(),
            },
        })?;
        Ok(Ok(Acted {
            live,
            landed,
            outcome,
        }))
    }

    /// The operator's exit (`lpbot exit-all`): the open position taken out
    /// and the other token swapped back, built from the chain alone.
    pub fn exit_records(
        &self,
        value_is_token0: bool,
        campaign: u32,
    ) -> anyhow::Result<Vec<StepRecord>> {
        let book = self.book()?;
        let market = self.market()?;
        let mut sim = Vec::new();
        let base = |step| StepRecord {
            block: 0,
            time: 0,
            campaign,
            action: "close".into(),
            sqrt_price_x96: market.sqrt_price_x96,
            step,
        };
        if let Some(p) = book.position {
            sim.push(base(Step::Burn {
                tick_lower: p.tick_lower,
                tick_upper: p.tick_upper,
                liquidity: p.liquidity,
                amount0: U256::ZERO,
                amount1: U256::ZERO,
                fees0: U256::ZERO,
                fees1: U256::ZERO,
            }));
        }
        // Sell the other token at the pool's price, as the bound's rate.
        let price = market.sqrt_price_x96;
        let (amount_in, amount_out) = if value_is_token0 {
            // token1 -> token0: out = in * 2^192 / price^2
            let q = U256::from(1u8) << 96;
            (q, q * q / price * q / price)
        } else {
            let q = U256::from(1u8) << 96;
            (q, q * price / q * price / q)
        };
        sim.push(base(Step::Swap {
            zero_for_one: !value_is_token0,
            amount_in,
            amount_out,
        }));
        Ok(sim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q96: u128 = 1 << 96;

    fn market_at(tick: i32) -> Market {
        Market {
            sqrt_price_x96: get_sqrt_ratio_at_tick(tick).unwrap(),
            tick,
        }
    }

    fn record(action: &str, step: Step) -> StepRecord {
        StepRecord {
            block: 10,
            time: 100,
            campaign: 1,
            action: action.into(),
            sqrt_price_x96: U256::from(Q96),
            step,
        }
    }

    fn mint(lower: i32, upper: i32, amount0: u64, amount1: u64) -> Step {
        Step::Mint {
            tick_lower: lower,
            tick_upper: upper,
            liquidity: 1,
            amount0: U256::from(amount0),
            amount1: U256::from(amount1),
        }
    }

    fn burn(lower: i32, upper: i32) -> Step {
        Step::Burn {
            tick_lower: lower,
            tick_upper: upper,
            liquidity: 1,
            amount0: U256::ZERO,
            amount1: U256::ZERO,
            fees0: U256::ZERO,
            fees1: U256::ZERO,
        }
    }

    fn book(balance0: u64, balance1: u64, position: Option<(i32, i32, u128)>) -> Book {
        Book {
            balance0: U256::from(balance0),
            balance1: U256::from(balance1),
            position: position.map(|(l, u, liquidity)| OpenPosition {
                token_id: U256::from(7u8),
                tick_lower: l,
                tick_upper: u,
                liquidity,
            }),
        }
    }

    #[test]
    fn an_entry_puts_in_what_the_holder_has_at_most_what_the_engine_did() {
        // Price at tick 0; a range above takes token0 only.
        let sim = [record("open", mint(600, 1_200, 1_000_000, 0))];
        let live = anchor("open", &sim, &market_at(0), &book(400_000, 0, None)).unwrap();
        let Step::Mint {
            amount0,
            amount1,
            liquidity,
            ..
        } = live[0].step
        else {
            panic!()
        };
        assert!(amount1.is_zero());
        assert!(
            amount0 <= U256::from(400_000u64),
            "no more than the holder has"
        );
        assert!(amount0 > U256::from(399_990u64), "{amount0}");
        assert!(liquidity > 0);
        // With more than the engine used, the engine's amount is the cap.
        let live = anchor("open", &sim, &market_at(0), &book(5_000_000, 0, None)).unwrap();
        let Step::Mint { amount0, .. } = live[0].step else {
            panic!()
        };
        assert!(amount0 <= U256::from(1_000_000u64) && amount0 > U256::from(999_990u64));
        assert_eq!(live[0].sqrt_price_x96, market_at(0).sqrt_price_x96);
    }

    #[test]
    fn an_entry_is_refused_when_price_moved_into_its_range_or_it_holds_both() {
        let sim = [record("open", mint(-600, 600, 1_000, 0))];
        assert_eq!(
            anchor("open", &sim, &market_at(0), &book(1_000, 0, None)),
            Err(Refusal::PriceInRange {
                lower: -600,
                upper: 600
            })
        );
        let mixed = [record("open", mint(-600, 600, 1_000, 1_000))];
        assert!(matches!(
            anchor("open", &mixed, &market_at(0), &book(1_000, 1_000, None)),
            Err(Refusal::Mixed { .. })
        ));
        let none = [record("open", mint(600, 1_200, 1_000, 0))];
        assert!(matches!(
            anchor("open", &none, &market_at(0), &book(0, 0, None)),
            Err(Refusal::Nothing(_))
        ));
    }

    #[test]
    fn a_close_takes_out_the_real_liquidity_and_sells_what_came_out_at_the_engines_rate() {
        // A range below the price (tick 0), full of token1 after a fall.
        let sim = [
            record("close", burn(-1_200, -600)),
            record(
                "close",
                Step::Swap {
                    zero_for_one: false,
                    amount_in: U256::from(1_000u64),
                    amount_out: U256::from(990u64),
                },
            ),
        ];
        // The engine thought the range sat below; on the chain it is.
        let held = book(50, 7, Some((-1_200, -600, 1_000_000_000)));
        let live = anchor("close", &sim, &market_at(0), &held).unwrap();
        let Step::Burn {
            liquidity,
            amount0,
            amount1,
            ..
        } = live[0].step
        else {
            panic!()
        };
        assert_eq!(
            liquidity, 1_000_000_000,
            "the chain's liquidity, not the engine's"
        );
        assert!(
            amount0.is_zero() && !amount1.is_zero(),
            "all token1 below the price"
        );
        let Step::Swap {
            amount_in,
            amount_out,
            ..
        } = live[1].step
        else {
            panic!()
        };
        assert_eq!(
            amount_in,
            amount1 + U256::from(7u8),
            "everything held of token1"
        );
        assert_eq!(
            amount_out,
            amount_in * U256::from(990u64) / U256::from(1_000u64)
        );
        // A position the chain does not have is refused.
        assert!(matches!(
            anchor("close", &sim, &market_at(0), &book(0, 0, None)),
            Err(Refusal::NoSuchPosition { .. })
        ));
        assert!(matches!(
            anchor("close", &sim, &market_at(0), &book(0, 0, Some((0, 600, 5)))),
            Err(Refusal::NoSuchPosition { .. })
        ));
    }

    #[test]
    fn a_flip_puts_what_the_burn_gave_into_the_other_side() {
        // Price at 0; the old range was above (all token0 now? no: price
        // went through it, so it holds token1 if price is above). Use a
        // range below the price that holds token1, flipped above the price
        // with token0 bought back is not a flip; a flip keeps the token.
        let sim = [
            record("flip", burn(-1_200, -600)),
            record("flip", mint(-600, 0, 0, 1_000_000)),
        ];
        let held = book(0, 0, Some((-1_200, -600, 1_000_000_000)));
        let live = anchor("flip", &sim, &market_at(60), &held).unwrap();
        let (Step::Burn { amount1: out, .. }, Step::Mint { amount1: put, .. }) =
            (&live[0].step, &live[1].step)
        else {
            panic!()
        };
        assert!(put <= out, "no more than the burn gave");
    }

    #[test]
    fn entries_wait_for_the_pause_flag_gas_and_the_daily_count_and_exits_do_not() {
        let limits = Limits {
            min_width_ticks: 60,
            slippage_ppm: 3_000,
            max_slippage_ppm: 5_000,
            price_limit_ppm: 10_000,
            max_swap_impact_ppm: 5_000,
            exit_max_swap_impact_ppm: 50_000,
            max_actions_per_day: 2,
        };
        let live = [record("open", mint(600, 1_200, 1_000, 0))];
        let market = market_at(0);
        let mut log = ActionLog::new();
        let run = |action: &str, pause: &State, base: u128, log: &mut ActionLog| {
            check(
                action,
                &live,
                &market,
                &limits,
                pause,
                base,
                20_000_000_000,
                log,
                "p",
                1_000,
            )
        };
        assert_eq!(run("open", &State::Running, 1, &mut log), Verdict::Allow);
        assert!(!run("open", &State::Paused("x".into()), 1, &mut log).allowed());
        assert!(run("close", &State::Exiting("x".into()), 1, &mut log).allowed());
        assert!(!run("open", &State::Running, 30_000_000_000, &mut log).allowed());
        assert!(run("close", &State::Running, 30_000_000_000, &mut log).allowed());
        log.record("p", 900);
        log.record("p", 950);
        assert!(!run("open", &State::Running, 1, &mut log).allowed());
        assert!(run("withdraw", &State::Running, 1, &mut log).allowed());
        let narrow = [record("open", mint(600, 630, 1_000, 0))];
        assert!(!check(
            "open",
            &narrow,
            &market,
            &limits,
            &State::Running,
            1,
            20_000_000_000,
            &mut ActionLog::new(),
            "p",
            1_000
        )
        .allowed());
    }
}
