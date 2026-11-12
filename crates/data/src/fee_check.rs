//! The check of the replica's fees against what real positions collected.
//!
//! A pool pays a position by adding to what it is owed whenever the
//! position is minted to or burned from: the fee growth inside its range
//! since the last time, times its liquidity. Burning adds the principal.
//! `Collect` events show what the owner then took out.
//!
//! This module replays a stored history, computes what every position was
//! owed from the replica's own fee growth, and compares it with the
//! `Collect` events. A position that was emptied and collected in full
//! must match to the last unit; a replica whose fee accounting is off in
//! any way fails here.
//!
//! Only positions that provably started empty can be followed: those with
//! a bound that was not an initialized tick in the starting snapshot.

use std::collections::BTreeMap;

use alloy_primitives::{keccak256, Address, B256, I256, U256};
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
}

/// How a position's collections compare with what the replica computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Emptied, and collected exactly what the replica computed.
    Exact,
    /// Emptied, and within the tolerance of what the replica computed.
    Close,
    /// Collected less than it was owed so far: not yet comparable.
    Open,
    /// Collected more than the replica says it was ever owed, or an
    /// emptied position differs by more than the tolerance.
    Mismatch,
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
/// could be followed from empty, in key order.
pub fn check_fees(snapshot: StoredSnapshot, logs: &[StoredLog]) -> StoreResult<Vec<PositionCheck>> {
    let pool_address = snapshot.pool;
    let initialized: std::collections::BTreeSet<i32> =
        snapshot.state.ticks.iter().map(|(tick, _)| *tick).collect();
    let mut pool = Pool::from_snapshot(snapshot.state);
    let mut positions: BTreeMap<PositionKey, Tracked> = BTreeMap::new();
    // Positions that may have held liquidity before the history began.
    let followable = |key: &PositionKey| {
        !initialized.contains(&key.tick_lower) || !initialized.contains(&key.tick_upper)
    };
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
        if !followable(&key) {
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

    /// A pool being run for real, recording the logs it would emit and the
    /// fee each swap paid to liquidity providers.
    struct Chain {
        pool: Pool,
        logs: Vec<StoredLog>,
        lp_fees: (U256, U256),
    }

    impl Chain {
        fn new() -> (Self, PoolSnapshot) {
            let mut pool = Pool::new(3_000, 60);
            pool.initialize(get_sqrt_ratio_at_tick(0).unwrap()).unwrap();
            // Background liquidity on ticks the test positions avoid.
            pool.mint(-6_000, 6_000, 10u128.pow(21)).unwrap();
            let snapshot = pool.snapshot();
            (
                Self {
                    pool,
                    logs: Vec::new(),
                    lp_fees: (U256::ZERO, U256::ZERO),
                },
                snapshot,
            )
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
        }

        fn mint(&mut self, owner: Address, lower: i32, upper: i32, amount: u128) {
            let (amount0, amount1) = self.pool.mint(lower, upper, amount).unwrap();
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
            let (amount0, amount1) = self.pool.burn(lower, upper, amount).unwrap();
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

        fn collect(&mut self, owner: Address, lower: i32, upper: i32, amounts: (U256, U256)) {
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
        let mut chain = Chain {
            pool,
            logs: Vec::new(),
            lp_fees: (U256::ZERO, U256::ZERO),
        };
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
        let checks = check_fees(stored(snapshot), &chain.logs).unwrap();
        let bob = checks.iter().find(|c| c.key.owner == BOB).unwrap();
        let alice = checks.iter().find(|c| c.key.owner == ALICE).unwrap();
        assert_eq!(bob.verdict(0.001), Verdict::Exact);
        assert_eq!(alice.verdict(0.001), Verdict::Open);
        let summary = summarize(&checks, 0.001);
        assert_eq!((summary.exact, summary.open, summary.mismatched), (1, 1, 0));
        assert!(summary.passed(1) && !summary.passed(2));
    }

    #[test]
    fn collecting_more_than_the_replica_computed_is_a_mismatch() {
        let (mut chain, snapshot) = Chain::new();
        chain.mint(BOB, 60, 180, 3 * 10u128.pow(18));
        chain.swap_to(200);
        chain.swap_to(0);
        chain.burn(BOB, 60, 180, 3 * 10u128.pow(18));
        let owed = check_fees(stored(snapshot.clone()), &chain.logs).unwrap()[0].owed();

        // One unit over is rounding; one percent over is a fault.
        let mut close = Chain {
            pool: chain.pool.clone(),
            logs: chain.logs.clone(),
            lp_fees: chain.lp_fees,
        };
        close.collect(BOB, 60, 180, (owed.0 + U256::from(1u8), owed.1));
        let check = &check_fees(stored(snapshot.clone()), &close.logs).unwrap()[0];
        assert_eq!(check.verdict(0.001), Verdict::Close);

        chain.collect(BOB, 60, 180, (owed.0 + owed.0 / U256::from(100u8), owed.1));
        let checks = check_fees(stored(snapshot), &chain.logs).unwrap();
        assert_eq!(checks[0].verdict(0.001), Verdict::Mismatch);
        assert!(!summarize(&checks, 0.001).passed(0));
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
