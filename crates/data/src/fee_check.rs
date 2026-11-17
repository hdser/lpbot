//! The check of the replica's fees against what real positions collected.
//!
//! A pool pays a position by adding to what it is owed whenever the
//! position is minted to or burned from: the fee growth inside its range
//! since the last time, times its liquidity. Burning adds the principal.
//! `Collect` events show what the owner then took out, and the pool keeps
//! the rest as the position's `tokensOwed`.
//!
//! This module replays a stored history and computes what every position
//! was owed from the replica's own fee growth. For a position that was
//! emptied, what the replica says it was owed, less what it collected,
//! must be what the pool still owes it at the end of the history. A
//! replica whose fee accounting is off either way fails here.
//!
//! Only positions that started the history empty can be followed. With a
//! [`PositionReader`] the pool on the chain says which ones did (no
//! liquidity and nothing owed at the start) and what each still owes at the
//! end. Without one, a position counts as starting empty when a bound of
//! it was not an initialized tick in the starting snapshot. That misses
//! fees left uncollected from before the history, and an emptied position
//! that collected less than the replica says may simply not have collected
//! yet: such a check confirms nothing, and its agreements are reported as
//! unverified.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{keccak256, Address, B256, I256, U256};
use lpbot_chain::uniswap_v3::{fetch_positions, ChainError};
use lpbot_chain::RpcClient;
use lpbot_venues::uniswap_v3::events::{decode_log, topics, PoolEvent};
use lpbot_venues::uniswap_v3::pool::{fees_earned, Pool, Replay};

use crate::snapshot::StoredSnapshot;
use crate::store::{StoreError, StoreResult, StoredLog};

/// Signature hash of the pool's `Collect` event.
pub fn collect_topic() -> B256 {
    keccak256(b"Collect(address,address,int24,int24,uint128,uint128)")
}

/// A position as the pool knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PositionKey {
    pub owner: Address,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

/// What the pool on the chain holds for a position after a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PositionState {
    pub liquidity: u128,
    /// Fees and principal credited and not yet collected, per token.
    pub tokens_owed: (u128, u128),
}

/// Reads positions from the pool on the chain (`positions(bytes32)`).
pub trait PositionReader {
    /// The positions of `pool` after `block`, in the order asked. A
    /// position the pool never saw is all zeros.
    fn positions(
        &self,
        pool: Address,
        keys: &[PositionKey],
        block: u64,
    ) -> Result<Vec<PositionState>, ChainError>;
}

/// A node reads positions with `eth_call` at the block, which needs the
/// state of that block: an archive node has it.
impl PositionReader for RpcClient {
    fn positions(
        &self,
        pool: Address,
        keys: &[PositionKey],
        block: u64,
    ) -> Result<Vec<PositionState>, ChainError> {
        let asked: Vec<(Address, i32, i32)> = keys
            .iter()
            .map(|key| (key.owner, key.tick_lower, key.tick_upper))
            .collect();
        Ok(fetch_positions(self, pool, &asked, block)?
            .into_iter()
            .map(|info| PositionState {
                liquidity: info.liquidity,
                tokens_owed: (info.tokens_owed0, info.tokens_owed1),
            })
            .collect())
    }
}

/// What the replica says a position was owed, against what it collected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionCheck {
    pub key: PositionKey,
    /// Liquidity still in the position at the end of the history.
    pub liquidity: u128,
    /// Fees the replica computed, per token.
    pub fees: (U256, U256),
    /// Principal returned by burns, per token.
    pub principal: (U256, U256),
    /// What `Collect` events took out, per token.
    pub collected: (U256, U256),
    /// What the pool on the chain still owed the position at the end of
    /// the history, when a [`PositionReader`] was asked. Its presence also
    /// means the pool confirmed the position started empty.
    pub owed_on_chain: Option<(U256, U256)>,
}

/// How a position's collections compare with what the replica computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Emptied, and what it collected plus what the pool still owes it is
    /// exactly what the replica computed. Without the pool's records:
    /// collected exactly that.
    Exact,
    /// As `Exact`, within the tolerance.
    Close,
    /// Not comparable yet: it still holds liquidity, or (without the
    /// pool's records) collected less than it was owed so far.
    Open,
    /// Contradicts the replica: collected more than the replica says it
    /// was ever owed, or an emptied position's collections and what the
    /// pool still owes it differ from the replica beyond the tolerance.
    Mismatch,
    /// Would be `Exact` or `Close`, but without the pool's records nothing
    /// proves the position started without fees owed, so it confirms
    /// nothing.
    Unverified,
}

impl PositionCheck {
    pub fn owed(&self) -> (U256, U256) {
        (
            self.fees.0 + self.principal.0,
            self.fees.1 + self.principal.1,
        )
    }

    /// Compares collections with what was owed. `tolerance` is a fraction,
    /// 0.001 for the 0.1% of the gate.
    pub fn verdict(&self, tolerance: f64) -> Verdict {
        match self.owed_on_chain {
            Some(on_chain) if self.liquidity == 0 => self.verdict_on_chain(on_chain, tolerance),
            Some(_) => self.verdict_from_collections(tolerance),
            None => match self.verdict_from_collections(tolerance) {
                Verdict::Exact | Verdict::Close => Verdict::Unverified,
                other => other,
            },
        }
    }

    /// An emptied position whose start and end the pool confirmed: the
    /// replica's owed must equal what was collected plus what the pool
    /// still owes. The tolerance applies to the fees, the only part the
    /// replica computes; the principal comes from the Burn events.
    fn verdict_on_chain(&self, on_chain: (U256, U256), tolerance: f64) -> Verdict {
        let owed = self.owed();
        let gap = |owed: U256, accounted: U256| owed.abs_diff(accounted);
        let gaps = (
            gap(owed.0, self.collected.0 + on_chain.0),
            gap(owed.1, self.collected.1 + on_chain.1),
        );
        if owed.0.is_zero() && owed.1.is_zero() {
            return if gaps.0.is_zero() && gaps.1.is_zero() {
                Verdict::Open
            } else {
                Verdict::Mismatch
            };
        }
        if gaps.0.is_zero() && gaps.1.is_zero() {
            return Verdict::Exact;
        }
        let within = |gap: U256, fees: U256| to_f64(gap) <= to_f64(fees) * tolerance;
        if within(gaps.0, self.fees.0) && within(gaps.1, self.fees.1) {
            Verdict::Close
        } else {
            Verdict::Mismatch
        }
    }

    /// Collections alone against what was owed.
    fn verdict_from_collections(&self, tolerance: f64) -> Verdict {
        let owed = self.owed();
        let over = self.collected.0 > owed.0 || self.collected.1 > owed.1;
        let exact = self.collected == owed;
        let nothing_collected = self.collected.0.is_zero() && self.collected.1.is_zero();
        if exact && !nothing_collected {
            return Verdict::Exact;
        }
        let within = |collected: U256, owed: U256| {
            let (c, o) = (to_f64(collected), to_f64(owed));
            (c - o).abs() <= o * tolerance
        };
        let close = within(self.collected.0, owed.0) && within(self.collected.1, owed.1);
        if over {
            // A unit or two over is rounding in the comparison, not a fault.
            if close {
                Verdict::Close
            } else {
                Verdict::Mismatch
            }
        } else if self.liquidity == 0 && close && !nothing_collected {
            Verdict::Close
        } else {
            Verdict::Open
        }
    }
}

fn to_f64(value: U256) -> f64 {
    value.to_string().parse().unwrap_or(f64::INFINITY)
}

#[derive(Debug, Clone, Default)]
struct Tracked {
    liquidity: u128,
    last_inside: (U256, U256),
    fees: (U256, U256),
    principal: (U256, U256),
    collected: (U256, U256),
}

fn bad(log: &StoredLog, reason: impl Into<String>) -> StoreError {
    StoreError::BadFile {
        path: Default::default(),
        reason: format!(
            "block {}, log {}: {}",
            log.block_number,
            log.log_index,
            reason.into()
        ),
    }
}

fn key_of(log: &StoredLog) -> StoreResult<PositionKey> {
    if log.topics.len() != 4 {
        return Err(bad(log, "a position event needs four topics"));
    }
    let tick = |topic: &B256| {
        i32::try_from(I256::from_raw(U256::from_be_bytes(topic.0)))
            .map_err(|_| bad(log, "a tick does not fit"))
    };
    Ok(PositionKey {
        owner: Address::from_word(log.topics[1]),
        tick_lower: tick(&log.topics[2])?,
        tick_upper: tick(&log.topics[3])?,
    })
}

fn word(log: &StoredLog, index: usize) -> StoreResult<U256> {
    log.data
        .get(index * 32..(index + 1) * 32)
        .map(U256::from_be_slice)
        .ok_or_else(|| bad(log, "the log data is too short"))
}

/// Replays `logs` on top of `snapshot` and returns every position that
/// could be followed from empty, in key order. Which ones started empty
/// is told by the ticks of the snapshot alone, so the checks' agreements
/// are unverified; [`check_fees_on_chain`] asks the pool instead.
pub fn check_fees(snapshot: StoredSnapshot, logs: &[StoredLog]) -> StoreResult<Vec<PositionCheck>> {
    let initialized: BTreeSet<i32> = snapshot.state.ticks.iter().map(|(tick, _)| *tick).collect();
    // One fresh bound means the position held no liquidity at the start.
    follow(snapshot, logs, |key| {
        !initialized.contains(&key.tick_lower) || !initialized.contains(&key.tick_upper)
    })
}

/// Why a check against the chain could not be made.
#[derive(Debug, thiserror::Error)]
pub enum FeeCheckError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("cannot read positions from the pool on the chain: {0}")]
    Reader(#[from] ChainError),
}

/// Like [`check_fees`], with the pool's own records: a position is
/// followed only if the pool held no liquidity and owed nothing for it at
/// the snapshot's block, and each followed position gets what the pool
/// still owed it after the last stored log's block.
pub fn check_fees_on_chain(
    snapshot: StoredSnapshot,
    logs: &[StoredLog],
    reader: &dyn PositionReader,
) -> Result<Vec<PositionCheck>, FeeCheckError> {
    let pool = snapshot.pool;
    let mut touched = BTreeSet::new();
    for log in logs {
        let topic0 = log.topics.first();
        if topic0 == Some(&topics::mint()) || topic0 == Some(&topics::burn()) {
            touched.insert(key_of(log)?);
        }
    }
    let touched: Vec<PositionKey> = touched.into_iter().collect();
    if touched.is_empty() {
        // Still replayed, so a damaged history is reported all the same.
        return Ok(follow(snapshot, logs, |_| false)?);
    }
    let at_start = reader.positions(pool, &touched, snapshot.block)?;
    if at_start.len() != touched.len() {
        return Err(FeeCheckError::Reader(ChainError::Other(format!(
            "asked for {} positions, got {}",
            touched.len(),
            at_start.len()
        ))));
    }
    let empty: BTreeSet<PositionKey> = touched
        .iter()
        .zip(&at_start)
        .filter(|(_, state)| **state == PositionState::default())
        .map(|(key, _)| *key)
        .collect();

    let mut checks = follow(snapshot, logs, |key| empty.contains(key))?;
    let end = logs.iter().map(|log| log.block_number).max();
    let (Some(end), false) = (end, checks.is_empty()) else {
        return Ok(checks);
    };
    let keys: Vec<PositionKey> = checks.iter().map(|check| check.key).collect();
    let at_end = reader.positions(pool, &keys, end)?;
    if at_end.len() != keys.len() {
        return Err(FeeCheckError::Reader(ChainError::Other(format!(
            "asked for {} positions, got {}",
            keys.len(),
            at_end.len()
        ))));
    }
    for (check, state) in checks.iter_mut().zip(at_end) {
        check.owed_on_chain = Some((
            U256::from(state.tokens_owed.0),
            U256::from(state.tokens_owed.1),
        ));
    }
    Ok(checks)
}

/// Replays `logs` on top of `snapshot`, following the positions that
/// `started_empty` admits.
fn follow(
    snapshot: StoredSnapshot,
    logs: &[StoredLog],
    started_empty: impl Fn(&PositionKey) -> bool,
) -> StoreResult<Vec<PositionCheck>> {
    let pool_address = snapshot.pool;
    let mut pool = Pool::from_snapshot(snapshot.state);
    let mut positions: BTreeMap<PositionKey, Tracked> = BTreeMap::new();
    let replay_error = |log: &StoredLog, reason: String| StoreError::Replay {
        pool: pool_address,
        block: log.block_number,
        log_index: log.log_index,
        reason,
    };

    for log in logs {
        let Some(topic0) = log.topics.first() else {
            continue;
        };
        if *topic0 == collect_topic() {
            let key = key_of(log)?;
            if let Some(position) = positions.get_mut(&key) {
                position.collected.0 += word(log, 1)?;
                position.collected.1 += word(log, 2)?;
            }
            continue;
        }
        let Some(event) =
            decode_log(&log.topics, &log.data).map_err(|e| bad(log, e.to_string()))?
        else {
            continue;
        };

        let is_mint = *topic0 == topics::mint();
        let is_burn = *topic0 == topics::burn();
        if !(is_mint || is_burn) {
            pool.apply_event(&event, Replay::Strict, &mut ())
                .map_err(|e| replay_error(log, e.to_string()))?;
            continue;
        }

        let key = key_of(log)?;
        // The contract reads the fee growth inside after a mint has
        // initialized its ticks, and before a burn clears them.
        let inside = if is_mint {
            pool.apply_event(&event, Replay::Strict, &mut ())
                .map_err(|e| replay_error(log, e.to_string()))?;
            pool.fee_growth_inside(key.tick_lower, key.tick_upper)
        } else {
            let inside = pool.fee_growth_inside(key.tick_lower, key.tick_upper);
            pool.apply_event(&event, Replay::Strict, &mut ())
                .map_err(|e| replay_error(log, e.to_string()))?;
            inside
        };
        if !positions.contains_key(&key) && !started_empty(&key) {
            continue;
        }

        let position = positions.entry(key).or_default();
        position.fees.0 += fees_earned(inside.0, position.last_inside.0, position.liquidity);
        position.fees.1 += fees_earned(inside.1, position.last_inside.1, position.liquidity);
        position.last_inside = inside;
        match event {
            PoolEvent::Mint { amount, .. } => {
                position.liquidity = position
                    .liquidity
                    .checked_add(amount)
                    .ok_or_else(|| bad(log, "position liquidity overflows"))?;
            }
            PoolEvent::Burn {
                amount,
                amount0,
                amount1,
                ..
            } => {
                position.liquidity = position
                    .liquidity
                    .checked_sub(amount)
                    .ok_or_else(|| bad(log, "a burn takes more than the position holds"))?;
                position.principal.0 += amount0;
                position.principal.1 += amount1;
            }
            _ => {}
        }
    }

    Ok(positions
        .into_iter()
        .map(|(key, p)| PositionCheck {
            key,
            liquidity: p.liquidity,
            fees: p.fees,
            principal: p.principal,
            collected: p.collected,
            owed_on_chain: None,
        })
        .collect())
}

/// The outcome of a fee check over many positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeeCheckSummary {
    pub exact: usize,
    pub close: usize,
    pub open: usize,
    pub mismatched: usize,
    /// Agreements that confirm nothing, as the pool's own records were
    /// not read.
    pub unverified: usize,
}

impl FeeCheckSummary {
    /// The gate: no position contradicts the replica, and at least
    /// `needed` emptied positions confirm it.
    pub fn passed(&self, needed: usize) -> bool {
        self.mismatched == 0 && self.exact + self.close >= needed
    }
}

pub fn summarize(checks: &[PositionCheck], tolerance: f64) -> FeeCheckSummary {
    let mut summary = FeeCheckSummary::default();
    for check in checks {
        match check.verdict(tolerance) {
            Verdict::Exact => summary.exact += 1,
            Verdict::Close => summary.close += 1,
            Verdict::Open => summary.open += 1,
            Verdict::Mismatch => summary.mismatched += 1,
            Verdict::Unverified => summary.unverified += 1,
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::day::Day;
    use lpbot_venues::uniswap_v3::events::encode_log;
    use lpbot_venues::uniswap_v3::pool::{PoolSnapshot, StepInfo, SwapEvent};
    use lpbot_venues::uniswap_v3::tick_math::get_sqrt_ratio_at_tick;

    const ALICE: Address = Address::repeat_byte(0xA1);
    const BOB: Address = Address::repeat_byte(0xB0);

    /// A position as the contract books it.
    #[derive(Debug, Clone, Copy, Default)]
    struct Booked {
        liquidity: u128,
        last_inside: (U256, U256),
        owed: (U256, U256),
    }

    /// A pool being run for real, recording the logs it would emit, the
    /// fee each swap paid to liquidity providers, and its positions as
    /// `positions(key)` shows them after every block.
    #[derive(Clone)]
    struct Chain {
        pool: Pool,
        logs: Vec<StoredLog>,
        lp_fees: (U256, U256),
        book: BTreeMap<PositionKey, Booked>,
        /// The book after each block; block 99 is the start.
        books: BTreeMap<u64, BTreeMap<PositionKey, Booked>>,
    }

    impl Chain {
        fn with_pool(pool: Pool) -> Self {
            Self {
                pool,
                logs: Vec::new(),
                lp_fees: (U256::ZERO, U256::ZERO),
                book: BTreeMap::new(),
                books: BTreeMap::from([(99, BTreeMap::new())]),
            }
        }

        fn new() -> (Self, PoolSnapshot) {
            let mut pool = Pool::new(3_000, 60);
            pool.initialize(get_sqrt_ratio_at_tick(0).unwrap()).unwrap();
            // Background liquidity on ticks the test positions avoid.
            pool.mint(-6_000, 6_000, 10u128.pow(21)).unwrap();
            let snapshot = pool.snapshot();
            (Self::with_pool(pool), snapshot)
        }

        /// Books a position as owed something before the history starts.
        fn owed_before(&mut self, owner: Address, lower: i32, upper: i32, owed: (U256, U256)) {
            let key = PositionKey {
                owner,
                tick_lower: lower,
                tick_upper: upper,
            };
            self.book.entry(key).or_default().owed = owed;
            self.books.insert(99, self.book.clone());
        }

        fn push(&mut self, mut topics: Vec<B256>, data: Vec<u8>, owner: Option<Address>) {
            if let Some(owner) = owner {
                topics[1] = owner.into_word();
            }
            let block = 100 + self.logs.len() as u64;
            self.logs.push(StoredLog {
                block_number: block,
                block_time: 1_700_000_000 + block * 12,
                log_index: 0,
                tx_hash: B256::ZERO,
                topics,
                data,
            });
            self.books.insert(block, self.book.clone());
        }

        /// What the contract credits a position when it is touched.
        fn credit(&mut self, key: PositionKey, inside: (U256, U256), delta: i128) {
            let booked = self.book.entry(key).or_default();
            booked.owed.0 += fees_earned(inside.0, booked.last_inside.0, booked.liquidity);
            booked.owed.1 += fees_earned(inside.1, booked.last_inside.1, booked.liquidity);
            booked.last_inside = inside;
            booked.liquidity = booked.liquidity.checked_add_signed(delta).unwrap();
        }

        fn mint(&mut self, owner: Address, lower: i32, upper: i32, amount: u128) {
            let (amount0, amount1) = self.pool.mint(lower, upper, amount).unwrap();
            let inside = self.pool.fee_growth_inside(lower, upper);
            let key = PositionKey {
                owner,
                tick_lower: lower,
                tick_upper: upper,
            };
            self.credit(key, inside, amount as i128);
            let (topics, data) = encode_log(&PoolEvent::Mint {
                tick_lower: lower,
                tick_upper: upper,
                amount,
                amount0,
                amount1,
            });
            self.push(topics, data, Some(owner));
        }

        fn burn(&mut self, owner: Address, lower: i32, upper: i32, amount: u128) -> (U256, U256) {
            let inside = self.pool.fee_growth_inside(lower, upper);
            let (amount0, amount1) = self.pool.burn(lower, upper, amount).unwrap();
            let key = PositionKey {
                owner,
                tick_lower: lower,
                tick_upper: upper,
            };
            self.credit(key, inside, -(amount as i128));
            let booked = self.book.get_mut(&key).unwrap();
            booked.owed.0 += amount0;
            booked.owed.1 += amount1;
            let (topics, data) = encode_log(&PoolEvent::Burn {
                tick_lower: lower,
                tick_upper: upper,
                amount,
                amount0,
                amount1,
            });
            self.push(topics, data, Some(owner));
            (amount0, amount1)
        }

        /// What the position is owed now, as the contract books it.
        fn owed(&self, owner: Address, lower: i32, upper: i32) -> (U256, U256) {
            let key = PositionKey {
                owner,
                tick_lower: lower,
                tick_upper: upper,
            };
            self.book
                .get(&key)
                .map_or((U256::ZERO, U256::ZERO), |b| b.owed)
        }

        fn collect(&mut self, owner: Address, lower: i32, upper: i32, amounts: (U256, U256)) {
            let key = PositionKey {
                owner,
                tick_lower: lower,
                tick_upper: upper,
            };
            if let Some(booked) = self.book.get_mut(&key) {
                booked.owed.0 = booked.owed.0.saturating_sub(amounts.0);
                booked.owed.1 = booked.owed.1.saturating_sub(amounts.1);
            }
            let signed = |tick: i32| {
                B256::from(I256::try_from(tick).unwrap().into_raw().to_be_bytes::<32>())
            };
            let mut data = vec![0u8; 32];
            data.extend(amounts.0.to_be_bytes::<32>());
            data.extend(amounts.1.to_be_bytes::<32>());
            self.push(
                vec![
                    collect_topic(),
                    owner.into_word(),
                    signed(lower),
                    signed(upper),
                ],
                data,
                None,
            );
        }

        fn swap_to(&mut self, tick: i32) {
            let limit = get_sqrt_ratio_at_tick(tick).unwrap();
            let down = limit < self.pool.sqrt_price_x96();
            let mut fees = (U256::ZERO, U256::ZERO);
            let mut observer = |step: &StepInfo| {
                if step.zero_for_one {
                    fees.0 += step.lp_fee;
                } else {
                    fees.1 += step.lp_fee;
                }
            };
            let amounts = self
                .pool
                .swap(
                    down,
                    I256::try_from(10u128.pow(30)).unwrap(),
                    limit,
                    &mut observer,
                )
                .unwrap();
            self.lp_fees.0 += fees.0;
            self.lp_fees.1 += fees.1;
            let (topics, data) = encode_log(&PoolEvent::Swap(SwapEvent {
                amount0: amounts.amount0,
                amount1: amounts.amount1,
                sqrt_price_x96: self.pool.sqrt_price_x96(),
                liquidity: self.pool.liquidity(),
                tick: self.pool.tick(),
            }));
            self.push(topics, data, None);
        }
    }

    /// The chain's positions, as a node reads them with `eth_call`.
    impl PositionReader for Chain {
        fn positions(
            &self,
            _pool: Address,
            keys: &[PositionKey],
            block: u64,
        ) -> Result<Vec<PositionState>, ChainError> {
            let (_, book) = self
                .books
                .range(..=block)
                .next_back()
                .ok_or_else(|| ChainError::Other(format!("no state for block {block}")))?;
            Ok(keys
                .iter()
                .map(|key| {
                    let booked = book.get(key).copied().unwrap_or_default();
                    PositionState {
                        liquidity: booked.liquidity,
                        tokens_owed: (booked.owed.0.to::<u128>(), booked.owed.1.to::<u128>()),
                    }
                })
                .collect())
        }
    }

    /// A pool that paid its positions less than its own fee growth says:
    /// what a replica that overstates fees by `percent` looks like.
    struct PaysLess<'a> {
        chain: &'a Chain,
        /// Fees per token the pool kept back from each position.
        short: (u128, u128),
    }

    impl PositionReader for PaysLess<'_> {
        fn positions(
            &self,
            pool: Address,
            keys: &[PositionKey],
            block: u64,
        ) -> Result<Vec<PositionState>, ChainError> {
            let mut states = self.chain.positions(pool, keys, block)?;
            if block > 99 {
                for state in &mut states {
                    state.tokens_owed.0 = state.tokens_owed.0.saturating_sub(self.short.0);
                    state.tokens_owed.1 = state.tokens_owed.1.saturating_sub(self.short.1);
                }
            }
            Ok(states)
        }
    }

    fn stored(state: PoolSnapshot) -> StoredSnapshot {
        StoredSnapshot {
            pool: Address::repeat_byte(1),
            token0: Address::repeat_byte(2),
            token1: Address::repeat_byte(3),
            block: 99,
            day: Day(20_000),
            state,
        }
    }

    #[test]
    fn a_position_alone_in_its_pool_is_owed_every_fee_paid() {
        // No background liquidity here: the position is the whole pool, so
        // what it is owed can be counted from the swaps themselves.
        let mut pool = Pool::new(3_000, 60);
        pool.initialize(get_sqrt_ratio_at_tick(0).unwrap()).unwrap();
        let snapshot = pool.snapshot();
        let mut chain = Chain::with_pool(pool);
        chain.mint(ALICE, -600, 600, 10u128.pow(20));
        for tick in [300, -300, 120, -480, 0] {
            chain.swap_to(tick);
        }
        let principal = chain.burn(ALICE, -600, 600, 10u128.pow(20));

        let checks = check_fees(stored(snapshot), &chain.logs).unwrap();
        assert_eq!(checks.len(), 1);
        let check = &checks[0];
        assert_eq!(check.key.owner, ALICE);
        assert_eq!((check.liquidity, check.principal), (0, principal));
        // Fee growth is kept per unit of liquidity and rounded down, so
        // the position is owed the fees paid, less a few units of dust.
        for (owed, paid) in [
            (check.fees.0, chain.lp_fees.0),
            (check.fees.1, chain.lp_fees.1),
        ] {
            assert!(
                owed <= paid && paid - owed < U256::from(10u8),
                "{owed} vs {paid}"
            );
            assert!(owed > U256::from(10u128.pow(14)));
        }
    }

    #[test]
    fn positions_are_followed_through_mints_pokes_burns_and_collects() {
        let (mut chain, snapshot) = Chain::new();
        chain.mint(ALICE, -120, 120, 5 * 10u128.pow(18));
        chain.swap_to(90);
        chain.mint(BOB, 60, 180, 3 * 10u128.pow(18));
        chain.swap_to(150);
        chain.swap_to(-200);
        // Alice adds to her position, then pokes it with an empty burn.
        chain.mint(ALICE, -120, 120, 10u128.pow(18));
        chain.swap_to(100);
        chain.burn(ALICE, -120, 120, 0);
        chain.swap_to(-30);
        // Bob leaves in two steps and collects everything.
        chain.burn(BOB, 60, 180, 10u128.pow(18));
        chain.swap_to(170);
        chain.burn(BOB, 60, 180, 2 * 10u128.pow(18));

        let before_collect = check_fees(stored(snapshot.clone()), &chain.logs).unwrap();
        let bob = before_collect.iter().find(|c| c.key.owner == BOB).unwrap();
        let alice = before_collect
            .iter()
            .find(|c| c.key.owner == ALICE)
            .unwrap();
        assert_eq!(bob.liquidity, 0);
        assert_eq!(alice.liquidity, 6 * 10u128.pow(18));
        assert!(bob.fees.0 > U256::ZERO && bob.fees.1 > U256::ZERO);
        assert!(alice.fees.0 > bob.fees.0);
        // Nothing collected yet: neither can be judged.
        assert_eq!(bob.verdict(0.001), Verdict::Open);
        assert_eq!(alice.verdict(0.001), Verdict::Open);

        // Bob collects all he is owed; Alice collects part of her fees.
        chain.collect(BOB, 60, 180, bob.owed());
        chain.collect(
            ALICE,
            -120,
            120,
            (alice.fees.0 / U256::from(2u8), U256::ZERO),
        );
        // Without the pool's records Bob's agreement proves nothing: he may
        // have been owed fees from before the history.
        let checks = check_fees(stored(snapshot.clone()), &chain.logs).unwrap();
        let bob = checks.iter().find(|c| c.key.owner == BOB).unwrap();
        let alice = checks.iter().find(|c| c.key.owner == ALICE).unwrap();
        assert_eq!(bob.owed_on_chain, None);
        assert_eq!(bob.verdict(0.001), Verdict::Unverified);
        assert_eq!(alice.verdict(0.001), Verdict::Open);
        let summary = summarize(&checks, 0.001);
        assert_eq!(
            (
                summary.exact,
                summary.unverified,
                summary.open,
                summary.mismatched
            ),
            (0, 1, 1, 0)
        );
        assert!(!summary.passed(1) && summary.passed(0));

        // With them, he confirms the replica.
        let checks = check_fees_on_chain(stored(snapshot), &chain.logs, &chain).unwrap();
        let bob = checks.iter().find(|c| c.key.owner == BOB).unwrap();
        let alice = checks.iter().find(|c| c.key.owner == ALICE).unwrap();
        assert_eq!(bob.owed_on_chain, Some((U256::ZERO, U256::ZERO)));
        assert_eq!(bob.verdict(0.001), Verdict::Exact);
        assert_eq!(alice.verdict(0.001), Verdict::Open);
        let summary = summarize(&checks, 0.001);
        assert_eq!((summary.exact, summary.open, summary.mismatched), (1, 1, 0));
        assert!(summary.passed(1) && !summary.passed(2));
    }

    #[test]
    fn an_emptied_position_that_kept_fees_in_the_pool_is_judged_by_what_the_pool_still_owes() {
        let (mut chain, snapshot) = Chain::new();
        chain.mint(BOB, 60, 180, 3 * 10u128.pow(18));
        chain.swap_to(200);
        chain.swap_to(-100);
        chain.swap_to(150);
        chain.burn(BOB, 60, 180, 3 * 10u128.pow(18));
        // Bob takes out his principal and only half of his fees.
        let owed = chain.owed(BOB, 60, 180);
        let fees = check_fees(stored(snapshot.clone()), &chain.logs).unwrap()[0].fees;
        let half = (
            owed.0 - fees.0 / U256::from(2u8),
            owed.1 - fees.1 / U256::from(2u8),
        );
        chain.collect(BOB, 60, 180, half);

        // From his collections alone this cannot be judged.
        let alone = &check_fees(stored(snapshot.clone()), &chain.logs).unwrap()[0];
        assert_eq!(alone.verdict(0.001), Verdict::Open);

        // The pool still owes him the other half: the replica is right.
        let checks = check_fees_on_chain(stored(snapshot.clone()), &chain.logs, &chain).unwrap();
        assert_eq!(checks[0].verdict(0.001), Verdict::Exact);

        // A pool that owes him 1% of his fees less than the replica says:
        // the replica overstates the fees, and the check says so.
        let short = |fees: U256| (fees / U256::from(100u8)).to::<u128>();
        let less = PaysLess {
            chain: &chain,
            short: (short(fees.0), short(fees.1)),
        };
        let checks = check_fees_on_chain(stored(snapshot.clone()), &chain.logs, &less).unwrap();
        assert_eq!(checks[0].verdict(0.001), Verdict::Mismatch);
        assert!(!summarize(&checks, 0.001).passed(0));
        // A few units short is within the tolerance.
        let less = PaysLess {
            chain: &chain,
            short: (3, 0),
        };
        let checks = check_fees_on_chain(stored(snapshot), &chain.logs, &less).unwrap();
        assert_eq!(checks[0].verdict(0.001), Verdict::Close);
    }

    #[test]
    fn a_position_owed_fees_from_before_the_history_is_left_out_when_the_pool_says_so() {
        let (mut chain, snapshot) = Chain::new();
        // Bob burned before the history and left fees in the pool; the
        // ticks of his range were cleared, so the snapshot cannot show it.
        let before = (U256::from(10u128.pow(17)), U256::from(10u128.pow(17)));
        chain.owed_before(BOB, 60, 180, before);
        chain.mint(BOB, 60, 180, 3 * 10u128.pow(18));
        chain.swap_to(200);
        chain.swap_to(0);
        chain.burn(BOB, 60, 180, 3 * 10u128.pow(18));
        let owed = chain.owed(BOB, 60, 180);
        chain.collect(BOB, 60, 180, owed);
        // Alice's range has both bounds initialized in the snapshot, but
        // the pool says her position there was empty.
        chain.mint(ALICE, -6_000, 6_000, 10u128.pow(18));
        chain.swap_to(300);
        chain.burn(ALICE, -6_000, 6_000, 10u128.pow(18));
        let owed = chain.owed(ALICE, -6_000, 6_000);
        chain.collect(ALICE, -6_000, 6_000, owed);

        // Ticks alone: Bob is followed and seems to collect more than he
        // was ever owed, which the old check called a fault; Alice is not
        // followed.
        let checks = check_fees(stored(snapshot.clone()), &chain.logs).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].key.owner, BOB);
        assert_eq!(checks[0].verdict(0.001), Verdict::Mismatch);

        // The pool's records: Bob did not start empty, Alice did.
        let checks = check_fees_on_chain(stored(snapshot), &chain.logs, &chain).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].key.owner, ALICE);
        assert_eq!(checks[0].verdict(0.001), Verdict::Exact);
    }

    #[test]
    fn a_reader_that_cannot_answer_is_an_error_of_its_own() {
        struct Down;
        impl PositionReader for Down {
            fn positions(
                &self,
                _: Address,
                _: &[PositionKey],
                _: u64,
            ) -> Result<Vec<PositionState>, ChainError> {
                Err(ChainError::Other("missing trie node".into()))
            }
        }
        let (mut chain, snapshot) = Chain::new();
        chain.mint(BOB, 60, 180, 10u128.pow(18));
        let error = check_fees_on_chain(stored(snapshot.clone()), &chain.logs, &Down).unwrap_err();
        assert!(matches!(error, FeeCheckError::Reader(_)), "{error}");
        assert!(error.to_string().contains("missing trie node"), "{error}");
        // No history, nothing to ask.
        assert_eq!(
            check_fees_on_chain(stored(snapshot), &[], &Down).unwrap(),
            vec![]
        );
    }

    #[test]
    fn collecting_more_than_the_replica_computed_is_a_mismatch() {
        let (mut chain, snapshot) = Chain::new();
        chain.mint(BOB, 60, 180, 3 * 10u128.pow(18));
        chain.swap_to(200);
        chain.swap_to(0);
        chain.burn(BOB, 60, 180, 3 * 10u128.pow(18));
        let owed = check_fees(stored(snapshot.clone()), &chain.logs).unwrap()[0].owed();

        // One unit over is rounding; one percent over is a fault. (A pool
        // pays out no more than it owes; these collections stand for a
        // replica that computes too little.)
        let mut close = chain.clone();
        close.collect(BOB, 60, 180, (owed.0 + U256::from(1u8), owed.1));
        let check = &check_fees(stored(snapshot.clone()), &close.logs).unwrap()[0];
        // Without the pool's records it confirms nothing.
        assert_eq!(check.verdict(0.001), Verdict::Unverified);
        let check = &check_fees_on_chain(stored(snapshot.clone()), &close.logs, &close).unwrap()[0];
        assert_eq!(check.verdict(0.001), Verdict::Close);

        chain.collect(BOB, 60, 180, (owed.0 + owed.0 / U256::from(100u8), owed.1));
        let checks = check_fees(stored(snapshot.clone()), &chain.logs).unwrap();
        assert_eq!(checks[0].verdict(0.001), Verdict::Mismatch);
        assert!(!summarize(&checks, 0.001).passed(0));
        let checks = check_fees_on_chain(stored(snapshot), &chain.logs, &chain).unwrap();
        assert_eq!(checks[0].verdict(0.001), Verdict::Mismatch);
    }

    #[test]
    fn positions_that_may_predate_the_history_are_left_out() {
        let (mut chain, snapshot) = Chain::new();
        // Both bounds were initialized in the snapshot: this owner may
        // have held liquidity there before, so the position is not followed.
        chain.mint(ALICE, -6_000, 6_000, 10u128.pow(18));
        // One fresh bound is enough to know a position starts empty.
        chain.mint(BOB, -6_000, 240, 10u128.pow(18));
        chain.swap_to(100);
        let checks = check_fees(stored(snapshot), &chain.logs).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].key.owner, BOB);
    }

    #[test]
    fn damaged_logs_are_reported_with_their_block() {
        let (mut chain, snapshot) = Chain::new();
        chain.mint(BOB, 60, 180, 10u128.pow(18));
        chain.collect(BOB, 60, 180, (U256::ZERO, U256::ZERO));
        chain.logs[1].data.truncate(40);
        let error = check_fees(stored(snapshot.clone()), &chain.logs).unwrap_err();
        assert!(error.to_string().contains("block 101"), "{error}");

        // A history with a hole does not replay.
        let (mut chain, _) = Chain::new();
        chain.mint(BOB, 60, 180, 10u128.pow(18));
        chain.swap_to(100);
        chain.swap_to(-100);
        chain.logs.remove(1);
        assert!(matches!(
            check_fees(stored(snapshot), &chain.logs),
            Err(StoreError::Replay { .. })
        ));
    }
}
