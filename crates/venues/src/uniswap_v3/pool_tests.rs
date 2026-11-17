use std::collections::BTreeMap;

use alloy_primitives::{I256, U256};

use super::super::events::PoolEvent;
use super::super::liquidity_amounts::get_amounts_for_liquidity;
use super::super::swap_math::compute_swap_step;
use super::super::tick_math::{get_sqrt_ratio_at_tick, MAX_SQRT_RATIO, MIN_SQRT_RATIO};
use super::super::{Q128, Q96};
use super::*;

fn pos(x: u128) -> I256 {
    I256::try_from(x).unwrap()
}

fn neg(x: u128) -> I256 {
    -pos(x)
}

fn one() -> U256 {
    U256::from(1u8)
}

/// A 0.05% pool at a price of one with wide background liquidity.
fn pool_with_background() -> Pool {
    let mut pool = Pool::new(500, 10);
    pool.initialize(Q96).unwrap();
    pool.mint(-20_000, 20_000, 1_000_000_000_000_000_000_000)
        .unwrap();
    pool
}

/// Small deterministic generator, so the scenario tests need no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

// --- tick walking -------------------------------------------------------

/// Reference vectors from v3-core's TickBitmap tests.
fn bitmap_pool(extra: &[i32]) -> Pool {
    let mut pool = Pool::new(500, 1);
    pool.initialize(Q96).unwrap();
    for tick in [-200, -55, -4, 70, 78, 84, 139, 240, 535] {
        pool.watch_tick(tick).unwrap();
    }
    for tick in extra {
        pool.watch_tick(*tick).unwrap();
    }
    pool
}

#[test]
fn next_tick_to_the_right_matches_the_reference() {
    let pool = bitmap_pool(&[]);
    let next = |tick| pool.next_initialized_tick_within_one_word(tick, false);
    assert_eq!(next(78), (84, true));
    assert_eq!(next(-55), (-4, true));
    assert_eq!(next(77), (78, true));
    assert_eq!(next(-56), (-55, true));
    assert_eq!(next(255), (511, false));
    assert_eq!(next(-257), (-200, true));
    assert_eq!(next(508), (511, false));
    assert_eq!(next(383), (511, false));

    let pool = bitmap_pool(&[340]);
    assert_eq!(
        pool.next_initialized_tick_within_one_word(328, false),
        (340, true)
    );
}

#[test]
fn next_tick_to_the_left_matches_the_reference() {
    let pool = bitmap_pool(&[]);
    let next = |tick| pool.next_initialized_tick_within_one_word(tick, true);
    assert_eq!(next(78), (78, true));
    assert_eq!(next(79), (78, true));
    assert_eq!(next(258), (256, false));
    assert_eq!(next(256), (256, false));
    assert_eq!(next(72), (70, true));
    assert_eq!(next(-257), (-512, false));
    assert_eq!(next(1023), (768, false));
    assert_eq!(next(900), (768, false));

    let pool = bitmap_pool(&[329]);
    assert_eq!(
        pool.next_initialized_tick_within_one_word(456, true),
        (329, true)
    );
}

#[test]
fn tick_walking_respects_the_tick_spacing() {
    let mut pool = Pool::new(3000, 60);
    pool.initialize(Q96).unwrap();
    pool.mint(-120, 180, 1_000).unwrap();
    // One word covers 256 spacings.
    assert_eq!(
        pool.next_initialized_tick_within_one_word(0, false),
        (180, true)
    );
    assert_eq!(
        pool.next_initialized_tick_within_one_word(0, true),
        (0, false)
    );
    assert_eq!(
        pool.next_initialized_tick_within_one_word(-1, true),
        (-120, true)
    );
    assert_eq!(
        pool.next_initialized_tick_within_one_word(180, false),
        (255 * 60, false)
    );
}

// --- positions ----------------------------------------------------------

#[test]
fn mint_and_burn_validate_their_ticks() {
    let mut pool = Pool::new(500, 10);
    assert_eq!(pool.mint(-10, 10, 1), Err(PoolError::NotInitialized));
    pool.initialize(Q96).unwrap();
    assert_eq!(pool.initialize(Q96), Err(PoolError::AlreadyInitialized));
    assert_eq!(pool.mint(10, 10, 1), Err(PoolError::BadTickRange));
    assert_eq!(pool.mint(20, 10, 1), Err(PoolError::BadTickRange));
    assert_eq!(pool.mint(-15, 10, 1), Err(PoolError::TickNotSpaced(-15)));
    assert_eq!(pool.mint(-887_280, 10, 1), Err(PoolError::BadTickRange));
    assert!(pool.burn(-10, 10, 1).is_err(), "nothing to burn");
}

#[test]
fn a_position_is_one_sided_outside_its_range_and_two_sided_inside() {
    let mut pool = Pool::new(500, 10);
    pool.initialize(Q96).unwrap();

    let (above0, above1) = pool.mint(100, 200, 1_000_000_000).unwrap();
    assert!(above0 > U256::ZERO);
    assert_eq!(above1, U256::ZERO);
    assert_eq!(pool.liquidity(), 0, "out of range liquidity is not active");

    let (below0, below1) = pool.mint(-200, -100, 1_000_000_000).unwrap();
    assert_eq!(below0, U256::ZERO);
    assert!(below1 > U256::ZERO);

    let (in0, in1) = pool.mint(-100, 100, 1_000_000_000).unwrap();
    assert!(in0 > U256::ZERO && in1 > U256::ZERO);
    assert_eq!(pool.liquidity(), 1_000_000_000);
}

#[test]
fn burning_everything_removes_the_ticks() {
    let mut pool = Pool::new(500, 10);
    pool.initialize(Q96).unwrap();
    let (in0, in1) = pool.mint(-100, 100, 5_000_000).unwrap();
    assert_eq!(pool.ticks().count(), 2);
    let (out0, out1) = pool.burn(-100, 100, 5_000_000).unwrap();
    assert_eq!(pool.ticks().count(), 0);
    assert_eq!(pool.liquidity(), 0);
    // Minting rounds up and burning rounds down, by at most one unit each.
    assert!(out0 <= in0 && in0 - out0 <= one());
    assert!(out1 <= in1 && in1 - out1 <= one());
}

#[test]
fn liquidity_per_tick_is_capped() {
    assert_eq!(
        tick_spacing_to_max_liquidity_per_tick(10),
        1917569901783203986719870431555990u128
    );
    assert_eq!(
        tick_spacing_to_max_liquidity_per_tick(60),
        11505743598341114571880798222544994u128
    );
    assert_eq!(
        tick_spacing_to_max_liquidity_per_tick(200),
        38350317471085141830651933667504588u128
    );
}

// --- swaps --------------------------------------------------------------

#[test]
fn a_swap_inside_one_range_equals_a_single_swap_step() {
    let mut pool = pool_with_background();
    let liquidity = pool.liquidity();
    let amount = 1_000_000_000_000_000u128;

    let expected =
        compute_swap_step(Q96, MIN_SQRT_RATIO + one(), liquidity, pos(amount), 500).unwrap();

    let result = pool
        .swap(true, pos(amount), MIN_SQRT_RATIO + one(), &mut ())
        .unwrap();

    assert_eq!(result.amount0, pos(amount));
    assert_eq!(
        result.amount1,
        -I256::try_from(expected.amount_out).unwrap()
    );
    assert_eq!(pool.sqrt_price_x96(), expected.sqrt_ratio_next);
    assert_eq!(
        pool.fee_growth_global().0,
        expected.fee_amount * Q128 / U256::from(liquidity)
    );
    assert_eq!(pool.fee_growth_global().1, U256::ZERO);
}

#[test]
fn swaps_reject_bad_inputs() {
    let mut pool = pool_with_background();
    assert_eq!(
        pool.swap(true, I256::ZERO, MIN_SQRT_RATIO + one(), &mut ()),
        Err(PoolError::ZeroAmount)
    );
    // The limit must be on the side the price moves to, and inside the bounds.
    assert_eq!(
        pool.swap(true, pos(1), Q96 + one(), &mut ()),
        Err(PoolError::BadPriceLimit)
    );
    assert_eq!(
        pool.swap(true, pos(1), MIN_SQRT_RATIO, &mut ()),
        Err(PoolError::BadPriceLimit)
    );
    assert_eq!(
        pool.swap(false, pos(1), Q96 - one(), &mut ()),
        Err(PoolError::BadPriceLimit)
    );
    assert_eq!(
        pool.swap(false, pos(1), MAX_SQRT_RATIO, &mut ()),
        Err(PoolError::BadPriceLimit)
    );
}

#[test]
fn crossing_a_tick_changes_active_liquidity() {
    let mut pool = pool_with_background();
    let background = pool.liquidity();
    pool.mint(100, 200, 7_000_000_000_000_000_000).unwrap();
    assert_eq!(pool.liquidity(), background);

    // Push the price up into the position's range.
    let inside = get_sqrt_ratio_at_tick(150).unwrap();
    pool.swap(false, pos(u128::MAX >> 2), inside, &mut ())
        .unwrap();
    assert_eq!(pool.tick(), 150);
    assert_eq!(pool.liquidity(), background + 7_000_000_000_000_000_000);

    // And beyond it.
    let beyond = get_sqrt_ratio_at_tick(250).unwrap();
    pool.swap(false, pos(u128::MAX >> 2), beyond, &mut ())
        .unwrap();
    assert_eq!(pool.liquidity(), background);

    // Stopping exactly on the lower tick while going down leaves the range.
    let lower = get_sqrt_ratio_at_tick(100).unwrap();
    pool.swap(true, pos(u128::MAX >> 2), lower, &mut ())
        .unwrap();
    assert_eq!(pool.sqrt_price_x96(), lower);
    assert_eq!(pool.tick(), 99);
    assert_eq!(pool.liquidity(), background);
}

#[test]
fn the_protocol_takes_its_share_of_the_fee() {
    let mut with_protocol = pool_with_background();
    with_protocol.set_fee_protocol(4, 4);
    let mut without = pool_with_background();

    let mut protocol_total = U256::ZERO;
    let mut lp_total = U256::ZERO;
    let mut observer = |step: &StepInfo| {
        protocol_total += step.protocol_fee;
        lp_total += step.lp_fee;
    };
    with_protocol
        .swap(
            true,
            pos(1_000_000_000_000_000_000),
            MIN_SQRT_RATIO + one(),
            &mut observer,
        )
        .unwrap();
    without
        .swap(
            true,
            pos(1_000_000_000_000_000_000),
            MIN_SQRT_RATIO + one(),
            &mut (),
        )
        .unwrap();

    // The trader pays the same either way; a quarter of the fee is diverted.
    assert_eq!(with_protocol.sqrt_price_x96(), without.sqrt_price_x96());
    let total = protocol_total + lp_total;
    assert_eq!(protocol_total, total / U256::from(4u8));
    assert!(with_protocol.fee_growth_global().0 < without.fee_growth_global().0);
}

#[test]
fn flash_fees_go_to_fee_growth() {
    let mut pool = pool_with_background();
    let liquidity = U256::from(pool.liquidity());
    pool.flash(U256::from(1_000_000u32), U256::from(3_000_000u32))
        .unwrap();
    assert_eq!(
        pool.fee_growth_global(),
        (
            U256::from(1_000_000u32) * Q128 / liquidity,
            U256::from(3_000_000u32) * Q128 / liquidity
        )
    );
    assert_eq!(
        flash_fee(U256::from(1_000_001u32), 500),
        Ok(U256::from(501u16))
    );
}

// --- the core play ------------------------------------------------------

#[test]
fn a_one_sided_position_returns_its_token_plus_fees_after_a_round_trip() {
    let mut pool = pool_with_background();
    let position_liquidity = 50_000_000_000_000_000_000u128;
    let (lower, upper) = (100, 200);

    // Enter above the price: token0 only.
    let (in0, in1) = pool.mint(lower, upper, position_liquidity).unwrap();
    assert!(in0 > U256::ZERO);
    assert_eq!(in1, U256::ZERO);
    let (inside0_start, inside1_start) = pool.fee_growth_inside(lower, upper);

    // What an observer attributes to the position, step by step.
    let mut share0 = U256::ZERO;
    let mut share1 = U256::ZERO;
    let mut observer = |step: &StepInfo| {
        if lower <= step.tick_start && step.tick_start < upper && step.liquidity > 0 {
            let share = step.lp_fee * U256::from(position_liquidity) / U256::from(step.liquidity);
            if step.zero_for_one {
                share0 += share;
            } else {
                share1 += share;
            }
        }
    };

    // Price runs up through the whole range...
    let above = get_sqrt_ratio_at_tick(300).unwrap();
    pool.swap(false, pos(u128::MAX >> 2), above, &mut observer)
        .unwrap();
    let (mid0, mid1) = get_amounts_for_liquidity(
        pool.sqrt_price_x96(),
        get_sqrt_ratio_at_tick(lower).unwrap(),
        get_sqrt_ratio_at_tick(upper).unwrap(),
        position_liquidity,
    )
    .unwrap();
    assert_eq!(
        mid0,
        U256::ZERO,
        "fully converted to token1 beyond the range"
    );
    assert!(mid1 > U256::ZERO);

    // ...and comes all the way back.
    pool.swap(true, pos(u128::MAX >> 2), Q96, &mut observer)
        .unwrap();
    assert_eq!(pool.sqrt_price_x96(), Q96);

    // Fees were earned in token1 on the way up and token0 on the way down.
    let (inside0_end, inside1_end) = pool.fee_growth_inside(lower, upper);
    let fees0 = fees_earned(inside0_end, inside0_start, position_liquidity);
    let fees1 = fees_earned(inside1_end, inside1_start, position_liquidity);
    assert!(fees0 > U256::ZERO && fees1 > U256::ZERO);

    // The observer's attribution agrees with the contract's fee growth.
    let close = |a: U256, b: U256| {
        let diff = if a > b { a - b } else { b - a };
        diff <= U256::from(4u8)
    };
    assert!(close(share0, fees0), "{share0} vs {fees0}");
    assert!(close(share1, fees1), "{share1} vs {fees1}");

    // A full pass trades the whole position once, so each pass pays about
    // the fee tier on the position's value.
    let fee_rate_up = fees1 * U256::from(1_000_000u32) / mid1;
    assert!(
        fee_rate_up >= U256::from(499u16) && fee_rate_up <= U256::from(501u16),
        "one pass paid {fee_rate_up} millionths"
    );

    // Leave: the same token0 comes back, give or take rounding.
    let (out0, out1) = pool.burn(lower, upper, position_liquidity).unwrap();
    assert_eq!(out1, U256::ZERO);
    assert!(out0 <= in0 && in0 - out0 <= one());
}

// --- replaying events ---------------------------------------------------

/// What the contract would emit for a swap that just ran on `pool`.
fn event_of(pool: &Pool, amounts: SwapAmounts) -> SwapEvent {
    SwapEvent {
        amount0: amounts.amount0,
        amount1: amounts.amount1,
        sqrt_price_x96: pool.sqrt_price_x96(),
        liquidity: pool.liquidity(),
        tick: pool.tick(),
    }
}

/// A random swap on `pool`: direction, exact input or output, an amount on
/// any scale from one unit up, and a price limit near or far.
fn random_swap(pool: &mut Pool, rng: &mut Rng) -> Option<SwapEvent> {
    let zero_for_one = rng.chance(50);
    let magnitude = match rng.below(6) {
        0 => 1 + rng.below(50) as u128,
        1 => 1 + rng.below(1_000_000) as u128,
        2 => 1 + rng.below(1_000_000_000_000) as u128,
        3 => (1 + rng.below(1_000_000) as u128) * 1_000_000_000_000,
        4 => (1 + rng.below(1_000_000) as u128) * 1_000_000_000_000_000,
        _ => (1 + rng.below(1_000) as u128) * 1_000_000_000_000_000_000_000,
    };
    let amount = if rng.chance(60) {
        pos(magnitude)
    } else {
        neg(magnitude)
    };

    let limit = if rng.chance(8) {
        if zero_for_one {
            MIN_SQRT_RATIO + one()
        } else {
            MAX_SQRT_RATIO - one()
        }
    } else {
        let distance = 1 + rng.below(3_000) as i32;
        let tick = if zero_for_one {
            pool.tick() - distance
        } else {
            pool.tick() + distance
        }
        .clamp(MIN_TICK + 1, MAX_TICK - 1);
        let price = get_sqrt_ratio_at_tick(tick).unwrap();
        // Sometimes stop between ticks, sometimes exactly on one.
        if rng.chance(50) {
            price
        } else if zero_for_one {
            price + U256::from(rng.below(1_000_000))
        } else {
            price - U256::from(rng.below(1_000_000))
        }
    };

    let amounts = pool.swap(zero_for_one, amount, limit, &mut ()).ok()?;
    Some(event_of(pool, amounts))
}

/// Random mints, burns and swaps applied to two pools: one executes the
/// swaps, the other only sees the resulting events.
fn replay_scenario(seed: u64, fee_pips: u32, spacing: i32, fee_protocol: (u8, u8), rounds: usize) {
    let mut rng = Rng(seed);
    let mut executed = Pool::new(fee_pips, spacing);
    let mut replayed = Pool::new(fee_pips, spacing);
    executed.initialize(Q96).unwrap();
    replayed.initialize(Q96).unwrap();
    executed.set_fee_protocol(fee_protocol.0, fee_protocol.1);
    replayed.set_fee_protocol(fee_protocol.0, fee_protocol.1);

    let mut positions: Vec<(i32, i32, u128)> = Vec::new();
    let mut swaps = 0;
    let mut swaps_with_output = 0;
    let mut swaps_crossing_ticks = 0;

    for round in 0..rounds {
        match rng.below(10) {
            0..=2 => {
                let centre = executed.tick() + rng.below(4_000) as i32 - 2_000;
                let half_width = (1 + rng.below(150) as i32) * spacing;
                let lower = (centre - half_width).div_euclid(spacing) * spacing;
                let upper = lower + 2 * half_width;
                let liquidity = (1 + rng.below(1_000_000) as u128) * 1_000_000_000_000;
                // Near the price bounds a range can fall outside the tick limits.
                let a = executed.mint(lower, upper, liquidity);
                let b = replayed.mint(lower, upper, liquidity);
                assert_eq!(a, b);
                if a.is_ok() {
                    positions.push((lower, upper, liquidity));
                }
            }
            3 if !positions.is_empty() => {
                let index = rng.below(positions.len() as u64) as usize;
                let (lower, upper, liquidity) = positions.swap_remove(index);
                let a = executed.burn(lower, upper, liquidity).unwrap();
                let b = replayed.burn(lower, upper, liquidity).unwrap();
                assert_eq!(a, b);
            }
            _ => {
                if let Some(event) = random_swap(&mut executed, &mut rng) {
                    let mut steps_with_trades = 0;
                    let mut count = |step: &StepInfo| {
                        if !step.amount_in.is_zero() {
                            steps_with_trades += 1;
                        }
                    };
                    replayed
                        .apply_swap_event(&event, Replay::Strict, &mut count)
                        .unwrap_or_else(|e| panic!("seed {seed} round {round}: {e}"));
                    swaps += 1;
                    if event.amount0.is_negative() || event.amount1.is_negative() {
                        swaps_with_output += 1;
                    }
                    if steps_with_trades > 1 {
                        swaps_crossing_ticks += 1;
                    }
                }
            }
        }
        assert_eq!(executed, replayed, "seed {seed} round {round}");
    }
    // The scenario must exercise real trading, not just walks through empty ranges.
    assert!(swaps > rounds / 3, "seed {seed}: only {swaps} swaps");
    assert!(
        swaps_with_output > rounds / 5,
        "seed {seed}: only {swaps_with_output} swaps paid anything out"
    );
    assert!(
        swaps_crossing_ticks > rounds / 20,
        "seed {seed}: only {swaps_crossing_ticks} swaps traded across a tick"
    );
}

#[test]
fn replaying_events_reproduces_the_executed_pool_exactly() {
    for seed in 1..=12u64 {
        replay_scenario(
            seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
            500,
            10,
            (0, 0),
            400,
        );
    }
}

#[test]
fn replaying_events_is_exact_with_a_protocol_fee_and_other_fee_tiers() {
    replay_scenario(0xA5A5_5A5A_1234_5678, 3000, 60, (4, 6), 500);
    replay_scenario(0x0123_4567_89AB_CDEF, 100, 1, (10, 0), 500);
    replay_scenario(0xDEAD_BEEF_CAFE_F00D, 10_000, 200, (0, 5), 500);
}

#[test]
fn a_swap_whose_whole_input_is_fee_is_replayed() {
    let mut executed = pool_with_background();
    let mut replayed = executed.clone();
    // One unit of input cannot move the price of a deep pool.
    let amounts = executed
        .swap(true, pos(1), MIN_SQRT_RATIO + one(), &mut ())
        .unwrap();
    assert_eq!(executed.sqrt_price_x96(), Q96);
    assert_eq!(amounts.amount1, I256::ZERO);
    let event = event_of(&executed, amounts);
    replayed
        .apply_swap_event(&event, Replay::Strict, &mut ())
        .unwrap();
    assert_eq!(executed, replayed);
    assert!(replayed.fee_growth_global().0 > U256::ZERO);
}

#[test]
fn a_replay_that_does_not_fit_the_event_is_refused() {
    let mut executed = pool_with_background();
    let pristine = executed.clone();
    let amounts = executed
        .swap(
            true,
            pos(1_000_000_000_000_000_000),
            MIN_SQRT_RATIO + one(),
            &mut (),
        )
        .unwrap();
    let good = event_of(&executed, amounts);

    let mut wrong_tick = good;
    wrong_tick.tick -= 1;
    assert!(matches!(
        pristine
            .clone()
            .apply_swap_event(&wrong_tick, Replay::Strict, &mut ()),
        Err(PoolError::ReplayMismatch(_))
    ));

    let mut wrong_liquidity = good;
    wrong_liquidity.liquidity += 1;
    assert!(matches!(
        pristine
            .clone()
            .apply_swap_event(&wrong_liquidity, Replay::Strict, &mut ()),
        Err(PoolError::ReplayMismatch(_))
    ));

    let mut wrong_output = good;
    wrong_output.amount1 -= I256::ONE;
    assert!(matches!(
        pristine
            .clone()
            .apply_swap_event(&wrong_output, Replay::Strict, &mut ()),
        Err(PoolError::ReplayMismatch(_))
    ));

    let mut too_little_input = good;
    too_little_input.amount0 = pos(1);
    assert!(matches!(
        pristine
            .clone()
            .apply_swap_event(&too_little_input, Replay::Strict, &mut ()),
        Err(PoolError::ReplayMismatch(_))
    ));

    let mut wrong_side = good;
    wrong_side.sqrt_price_x96 = Q96 + one();
    assert!(matches!(
        pristine
            .clone()
            .apply_swap_event(&wrong_side, Replay::Strict, &mut ()),
        Err(PoolError::ReplayMismatch(_))
    ));
}

// --- watched ticks ------------------------------------------------------

#[test]
fn watched_ticks_split_steps_without_changing_the_price_path() {
    let mut rng = Rng(0x5EED_0000_0000_0001);
    let mut executed = pool_with_background();
    let mut watched = executed.clone();
    let (lower, upper) = (-130, 170);
    watched.watch_tick(lower).unwrap();
    watched.watch_tick(upper).unwrap();
    let sqrt_lower = get_sqrt_ratio_at_tick(lower).unwrap();
    let sqrt_upper = get_sqrt_ratio_at_tick(upper).unwrap();

    let mut strict_fees = U256::ZERO;
    let mut watched_fees = U256::ZERO;
    let mut swaps = 0;

    for _ in 0..300 {
        let Some(event) = random_swap(&mut executed, &mut rng) else {
            continue;
        };
        swaps += 1;

        let mut strict_copy = watched.clone();
        strict_copy.unwatch_tick(lower);
        strict_copy.unwatch_tick(upper);
        // Totals of the strict replay, for comparison.
        let mut count_strict = |step: &StepInfo| strict_fees += step.lp_fee + step.protocol_fee;
        strict_copy
            .apply_swap_event(&event, Replay::Strict, &mut count_strict)
            .unwrap();

        let mut check = |step: &StepInfo| {
            watched_fees += step.lp_fee + step.protocol_fee;
            let (low, high) = if step.sqrt_price_start < step.sqrt_price_end {
                (step.sqrt_price_start, step.sqrt_price_end)
            } else {
                (step.sqrt_price_end, step.sqrt_price_start)
            };
            for boundary in [sqrt_lower, sqrt_upper] {
                assert!(
                    !(low < boundary && boundary < high),
                    "a step straddles a watched tick"
                );
            }
        };
        watched
            .apply_swap_event(&event, Replay::Tolerant, &mut check)
            .unwrap();

        assert_eq!(watched.sqrt_price_x96(), executed.sqrt_price_x96());
        assert_eq!(watched.tick(), executed.tick());
        assert_eq!(watched.liquidity(), executed.liquidity());
    }

    assert!(swaps > 100);
    // Splitting steps moves at most a few units of rounding per swap.
    let diff = if strict_fees > watched_fees {
        strict_fees - watched_fees
    } else {
        watched_fees - strict_fees
    };
    assert!(
        diff <= U256::from(4 * swaps as u64),
        "fee totals differ by {diff}"
    );
}

#[test]
fn a_swap_that_ends_exactly_on_a_watched_tick_leaves_the_contracts_tick() {
    // The executed pool has no boundary at tick 130: a swap down to that
    // exact price ends inside a range, and the tick is 130.
    let mut executed = pool_with_background();
    executed
        .swap(
            false,
            pos(u128::MAX >> 2),
            get_sqrt_ratio_at_tick(455).unwrap(),
            &mut (),
        )
        .unwrap();
    let mut watching = executed.clone();
    watching.watch_tick(130).unwrap();

    fn swap_to(pool: &mut Pool, tick: i32) -> SwapEvent {
        let limit = get_sqrt_ratio_at_tick(tick).unwrap();
        let down = limit < pool.sqrt_price_x96();
        let amounts = pool
            .swap(down, pos(u128::MAX >> 2), limit, &mut ())
            .unwrap();
        event_of(pool, amounts)
    }
    let mut events = vec![swap_to(&mut executed, 130)];
    assert_eq!(executed.tick(), 130);
    // Further down, back up onto the same price from below, away upward,
    // and down onto it again.
    for target in [70, 130, 300, 130] {
        events.push(swap_to(&mut executed, target));
    }

    for (i, event) in events.iter().enumerate() {
        watching
            .apply_swap_event(event, Replay::Tolerant, &mut ())
            .unwrap_or_else(|e| panic!("event {i}: {e}"));
        assert_eq!(watching.tick(), event.tick, "event {i}");
    }

    // A position minted with its lower bound on that tick is in range in
    // both pools, and the watched tick starts with the contract's values.
    let minted = executed.mint(130, 200, 5_000_000).unwrap();
    assert_eq!(watching.mint(130, 200, 5_000_000).unwrap(), minted);
    assert_eq!(watching.liquidity(), executed.liquidity());
    let last = swap_to(&mut executed, 40);
    watching
        .apply_swap_event(&last, Replay::Tolerant, &mut ())
        .unwrap();
    watching.unwatch_tick(130);
    assert!(
        watching.differences(&executed).is_empty(),
        "{:?}",
        watching.differences(&executed)
    );
}

#[test]
fn a_tick_first_watched_then_minted_starts_with_the_contracts_fee_growth() {
    let mut executed = pool_with_background();
    let mut watching = executed.clone();
    watching.watch_tick(100).unwrap();
    // Cross the watched tick both ways, so it has been flipped.
    for target in [150, 50] {
        let limit = get_sqrt_ratio_at_tick(target).unwrap();
        let down = limit < executed.sqrt_price_x96();
        let amounts = executed
            .swap(down, pos(u128::MAX >> 2), limit, &mut ())
            .unwrap();
        watching
            .apply_swap_event(&event_of(&executed, amounts), Replay::Tolerant, &mut ())
            .unwrap();
    }
    // Price is below 100 now: a tick initialized above the price starts at zero.
    executed.mint(100, 160, 7_000_000).unwrap();
    watching.mint(100, 160, 7_000_000).unwrap();
    watching.unwatch_tick(100);
    // Fee growth totals differ by rounding once steps are split, so the
    // tick itself is compared.
    let tick_100 = |pool: &Pool| *pool.ticks().find(|(tick, _)| *tick == 100).unwrap().1;
    assert_eq!(tick_100(&watching), tick_100(&executed));
    assert_eq!(tick_100(&watching).fee_growth_outside0, U256::ZERO);
    assert_eq!(tick_100(&watching).fee_growth_outside1, U256::ZERO);
}

#[test]
fn watching_and_unwatching_leaves_no_trace() {
    let mut pool = pool_with_background();
    let before = pool.clone();
    pool.watch_tick(50).unwrap();
    pool.watch_tick(50).unwrap();
    assert_eq!(pool.tick_info(50).unwrap().watchers, 2);
    pool.unwatch_tick(50);
    pool.unwatch_tick(50);
    assert_eq!(pool, before);
    assert_eq!(pool.watch_tick(55), Err(PoolError::TickNotSpaced(55)));

    // A watch on a tick that also holds liquidity survives the burn.
    pool.mint(50, 150, 1_000).unwrap();
    pool.watch_tick(50).unwrap();
    pool.burn(50, 150, 1_000).unwrap();
    assert_eq!(pool.tick_info(50).unwrap().watchers, 1);
    assert_eq!(pool.tick_info(50).unwrap().liquidity_gross, 0);
    assert!(pool.tick_info(150).is_none());
}

// --- watched ticks: the contract's tick ---------------------------------

/// A swap with unlimited exact input that stops exactly on `tick`.
fn swap_exactly_to(pool: &mut Pool, tick: i32) -> Option<SwapEvent> {
    let limit = get_sqrt_ratio_at_tick(tick).unwrap();
    let down = limit < pool.sqrt_price_x96();
    let amounts = pool.swap(down, pos(u128::MAX >> 2), limit, &mut ()).ok()?;
    Some(event_of(pool, amounts))
}

/// A swap of `amount` (positive: exact input, negative: exact output) with
/// no price limit to speak of.
fn swap_amount(pool: &mut Pool, zero_for_one: bool, amount: I256) -> Option<SwapEvent> {
    let limit = if zero_for_one {
        MIN_SQRT_RATIO + one()
    } else {
        MAX_SQRT_RATIO - one()
    };
    let amounts = pool.swap(zero_for_one, amount, limit, &mut ()).ok()?;
    Some(event_of(pool, amounts))
}

/// Panics unless `watching` holds what the contract holds in `executed`:
/// price, tick, active liquidity and every tick's liquidity. Fee growth can
/// differ by rounding once steps are split.
fn assert_contract_state(watching: &Pool, executed: &Pool, context: &str) {
    assert_eq!(
        watching.sqrt_price_x96(),
        executed.sqrt_price_x96(),
        "{context}: price"
    );
    assert_eq!(watching.tick(), executed.tick(), "{context}: tick");
    assert_eq!(
        watching.liquidity(),
        executed.liquidity(),
        "{context}: liquidity"
    );
    let liquidity_by_tick = |pool: &Pool| -> Vec<(i32, u128, i128)> {
        pool.snapshot()
            .ticks
            .iter()
            .map(|(tick, info)| (*tick, info.liquidity_gross, info.liquidity_net))
            .collect()
    };
    assert_eq!(
        liquidity_by_tick(watching),
        liquidity_by_tick(executed),
        "{context}: ticks"
    );
}

#[derive(Debug, Clone, Copy)]
enum Move {
    /// A swap with unlimited input to exactly this tick's price.
    To(i32),
    /// One unit in, down (`true`) or up: all of it is fee, the price stays.
    Dust(bool),
    /// One unit out, down or up: the price moves by the least it can.
    Out(bool),
    Mint(i32, i32),
    Burn(i32, i32),
}

/// Liquidity of the positions `Move::Mint` and `Move::Burn` add and remove.
const MOVE_LIQUIDITY: u128 = 7_000_000_000_000_000_000;

/// Runs a move on the executed pool and returns the event it emitted.
fn run_move(executed: &mut Pool, step: Move) -> Option<PoolEvent> {
    Some(match step {
        Move::To(tick) => PoolEvent::Swap(swap_exactly_to(executed, tick)?),
        Move::Dust(down) => PoolEvent::Swap(swap_amount(executed, down, pos(1))?),
        Move::Out(down) => PoolEvent::Swap(swap_amount(executed, down, neg(1))?),
        Move::Mint(tick_lower, tick_upper) => {
            let (amount0, amount1) = executed.mint(tick_lower, tick_upper, MOVE_LIQUIDITY).ok()?;
            PoolEvent::Mint {
                tick_lower,
                tick_upper,
                amount: MOVE_LIQUIDITY,
                amount0,
                amount1,
            }
        }
        Move::Burn(tick_lower, tick_upper) => {
            let (amount0, amount1) = executed.burn(tick_lower, tick_upper, MOVE_LIQUIDITY).ok()?;
            PoolEvent::Burn {
                tick_lower,
                tick_upper,
                amount: MOVE_LIQUIDITY,
                amount0,
                amount1,
            }
        }
    })
}

/// Replays an event the way the simulator does: mints and burns must match
/// exactly, swaps through watched ticks may differ by rounding.
fn replay(pool: &mut Pool, event: &PoolEvent, observer: &mut impl SwapObserver) -> PoolResult<()> {
    let mode = if matches!(event, PoolEvent::Swap(_)) {
        Replay::Tolerant
    } else {
        Replay::Strict
    };
    pool.apply_event(event, mode, observer)
}

/// Runs `moves` on `executed` and replays every event on `watching`,
/// checking after each one that `watching` holds the contract's state.
fn follow(
    executed: &mut Pool,
    watching: &mut Pool,
    moves: &[Move],
    observer: &mut impl SwapObserver,
) {
    for (i, step) in moves.iter().enumerate() {
        let context = format!("move {i} ({step:?})");
        let event = run_move(executed, *step)
            .unwrap_or_else(|| panic!("{context}: the executed pool refused it"));
        replay(watching, &event, observer).unwrap_or_else(|e| panic!("{context}: {e}"));
        assert_contract_state(watching, executed, &context);
    }
}

#[test]
fn the_contract_stops_at_initialized_ticks_word_edges_and_the_limits() {
    let mut pool = pool_with_background();
    pool.watch_tick(130).unwrap();
    // Spacing 10: a word covers 2560 ticks.
    assert!(pool.contract_stops_at(0, true) && !pool.contract_stops_at(0, false));
    assert!(pool.contract_stops_at(2550, false) && !pool.contract_stops_at(2550, true));
    assert!(pool.contract_stops_at(-10, false) && !pool.contract_stops_at(-10, true));
    // A watched tick is not the contract's, an initialized one is both ways.
    assert!(!pool.contract_stops_at(130, true) && !pool.contract_stops_at(130, false));
    assert!(pool.contract_stops_at(20_000, true) && pool.contract_stops_at(20_000, false));
    assert!(!pool.contract_stops_at(5, true), "not on the spacing");
    assert!(pool.contract_stops_at(MIN_TICK, true) && pool.contract_stops_at(MAX_TICK, false));
}

#[test]
fn an_upward_swap_that_stays_on_a_watched_word_start_keeps_the_contracts_tick() {
    // Tick 0 starts a bitmap word, so a swap down to its price stops there
    // in the contract and leaves the tick at -1. Going up it is not a
    // boundary: a swap that cannot move the price keeps the tick at -1,
    // while the replica's first, zero-length step crosses the watched tick.
    let mut executed = pool_with_background();
    swap_exactly_to(&mut executed, 455).unwrap();
    let mut watching = executed.clone();
    watching.watch_tick(0).unwrap();
    watching.watch_tick(100).unwrap();

    let down = swap_exactly_to(&mut executed, 0).unwrap();
    assert_eq!(executed.tick(), -1);
    watching
        .apply_swap_event(&down, Replay::Tolerant, &mut ())
        .unwrap();
    assert_eq!(watching.tick(), -1);

    let stay = swap_amount(&mut executed, false, pos(1)).unwrap();
    assert_eq!(executed.sqrt_price_x96(), Q96);
    assert_eq!(executed.tick(), -1);
    watching
        .apply_swap_event(&stay, Replay::Tolerant, &mut ())
        .unwrap();
    assert_contract_state(&watching, &executed, "after the swap that stayed");
}

#[test]
fn swaps_that_end_on_or_start_from_a_watched_tick_leave_the_contracts_state() {
    use Move::*;
    // A word covers 2560 ticks: 0 and 2560 start one (the contract stops
    // there going down), -10 and 2550 end one (it stops there going up),
    // 130 is neither.
    for w in [130, 0, 2560, -10, 2550] {
        let moves = [
            // Onto the tick from above and from below, and swaps that
            // start there, with and without moving the price.
            To(w + 300),
            To(w),
            Dust(false),
            Dust(true),
            Dust(false),
            Out(false),
            To(w),
            Dust(true),
            Out(true),
            To(w - 200),
            To(w),
            Dust(true),
            Dust(false),
            Dust(true),
            Out(true),
            To(w),
            Dust(false),
            Out(false),
            To(w + 40),
            To(w),
            // The same with a real position on the tick.
            Mint(w, w + 100),
            Dust(false),
            Dust(true),
            To(w + 50),
            To(w),
            Dust(false),
            Dust(true),
            To(w - 50),
            To(w),
            Dust(true),
            Dust(false),
            Out(false),
            // Arrive from above while the tick holds liquidity, so the
            // contract's tick is below it, then burn the liquidity.
            To(w + 50),
            To(w),
            Burn(w, w + 100),
            Dust(false),
            Dust(true),
            Dust(false),
            // Arrive from below onto a tick that holds liquidity, burn it.
            To(w - 30),
            Mint(w - 100, w),
            To(w),
            Dust(true),
            Dust(false),
            Burn(w - 100, w),
            Dust(true),
            Dust(false),
            To(w + 10),
            To(w),
            Dust(true),
        ];
        let mut executed = pool_with_background();
        let mut watching = executed.clone();
        watching.watch_tick(w).unwrap();
        follow(&mut executed, &mut watching, &moves, &mut ());
        watching.unwatch_tick(w);
        assert!(watching.tick_info(w).is_none(), "tick {w}");
    }
}

// --- watched ticks: fee growth inside -----------------------------------

/// What the simulator's observer credits a virtual position, step by step,
/// next to what `fee_growth_inside` says it earned since it started.
struct Tracker {
    lower: i32,
    upper: i32,
    liquidity: u128,
    start: (U256, U256),
    credited: (U256, U256),
    steps: u64,
}

impl Tracker {
    fn start(pool: &Pool, lower: i32, upper: i32, liquidity: u128) -> Self {
        Self {
            lower,
            upper,
            liquidity,
            start: pool.fee_growth_inside(lower, upper),
            credited: (U256::ZERO, U256::ZERO),
            steps: 0,
        }
    }

    fn on_step(&mut self, step: &StepInfo) {
        if step.liquidity == 0 || step.tick_start < self.lower || step.tick_start >= self.upper {
            return;
        }
        let share = mul_div(
            step.lp_fee,
            U256::from(self.liquidity),
            U256::from(step.liquidity),
        )
        .unwrap();
        if step.zero_for_one {
            self.credited.0 += share;
        } else {
            self.credited.1 += share;
        }
        self.steps += 1;
    }

    /// Both round down once per step, so they agree within one unit per
    /// credited step.
    fn check(&self, pool: &Pool, context: &str) {
        let now = pool.fee_growth_inside(self.lower, self.upper);
        let earned = (
            fees_earned(now.0, self.start.0, self.liquidity),
            fees_earned(now.1, self.start.1, self.liquidity),
        );
        let tolerance = U256::from(self.steps + 1);
        for (token, a, b) in [
            (0, earned.0, self.credited.0),
            (1, earned.1, self.credited.1),
        ] {
            let diff = if a > b { a - b } else { b - a };
            assert!(
                diff <= tolerance,
                "{context}: range [{}, {}) token {token}: fee growth inside says {a}, the observer {b}",
                self.lower,
                self.upper
            );
        }
    }
}

#[test]
fn fee_growth_inside_a_watched_range_survives_swaps_that_end_on_its_bounds() {
    let (lower, upper) = (130, 200);
    let paths: [&[i32]; 4] = [
        // Never exactly on a bound.
        &[131, 180, 50, 190, 100],
        // Down onto the lower bound, then on.
        &[130, 180, 50, 190, 100],
        // Onto both bounds from either side.
        &[200, 130, 200, 50, 200, 130, 300, 130],
        &[130, 200, 130, 200],
    ];
    for path in paths {
        let mut executed = pool_with_background();
        swap_exactly_to(&mut executed, 455).unwrap();
        let mut watching = executed.clone();
        watching.watch_tick(lower).unwrap();
        watching.watch_tick(upper).unwrap();
        let mut tracker = Tracker::start(&watching, lower, upper, 1_000_000_000_000_000_000);
        let moves: Vec<Move> = path
            .iter()
            .flat_map(|tick| [Move::To(*tick), Move::Dust(true), Move::Dust(false)])
            .collect();
        follow(
            &mut executed,
            &mut watching,
            &moves,
            &mut |step: &StepInfo| tracker.on_step(step),
        );
        tracker.check(&watching, &format!("path {path:?}"));
        assert!(tracker.credited.0 > U256::ZERO && tracker.credited.1 > U256::ZERO);
    }
}

#[test]
fn real_mints_and_burns_on_watched_ticks_do_not_shift_fee_growth_inside() {
    use Move::*;
    let (lower, upper) = (130, 200);
    let mut executed = pool_with_background();
    swap_exactly_to(&mut executed, 455).unwrap();
    let mut watching = executed.clone();
    watching.watch_tick(lower).unwrap();
    watching.watch_tick(upper).unwrap();
    // Cross the watched ticks first, so their records are not trivial.
    follow(&mut executed, &mut watching, &[To(50), To(455)], &mut ());

    let mut tracker = Tracker::start(&watching, lower, upper, 1_000_000_000_000_000_000);
    let moves = [
        To(180),
        // Real positions appear on both bounds and are crossed both ways...
        Mint(lower, 300),
        To(50),
        To(190),
        Mint(100, upper),
        To(250),
        To(160),
        // ...and go again while the bounds stay watched.
        Burn(lower, 300),
        To(130),
        Dust(true),
        To(220),
        Burn(100, upper),
        To(200),
        Dust(false),
        To(140),
        Mint(lower, upper),
        To(60),
        Burn(lower, upper),
        To(170),
    ];
    follow(
        &mut executed,
        &mut watching,
        &moves,
        &mut |step: &StepInfo| tracker.on_step(step),
    );
    tracker.check(&watching, "after mints and burns on the bounds");
}

#[test]
fn a_watched_tick_keeps_its_record_apart_from_the_contracts() {
    let mut pool = pool_with_background();
    pool.watch_tick(100).unwrap();
    for tick in [150, 50, 150] {
        swap_exactly_to(&mut pool, tick).unwrap();
    }
    // The contract has no tick 100, so neither does the pool's contract state.
    assert_eq!(
        *pool.tick_info(100).unwrap(),
        TickInfo {
            watchers: 1,
            ..TickInfo::default()
        }
    );
    let inside = pool.fee_growth_inside(100, 200);

    // A real position starts the contract's record by its convention: the
    // price is above the tick, so all growth so far counts as below it.
    pool.mint(100, 200, 1_000).unwrap();
    let info = *pool.tick_info(100).unwrap();
    assert_eq!(
        (info.fee_growth_outside0, info.fee_growth_outside1),
        pool.fee_growth_global()
    );
    // The watched record goes on as it was, and so does the range.
    assert_eq!(pool.fee_growth_inside(100, 200), inside);
    pool.burn(100, 200, 1_000).unwrap();
    assert_eq!(pool.fee_growth_inside(100, 200), inside);
}

/// For a tick that both holds liquidity and is watched, the contract's
/// record and the watched one flip together, so their difference only
/// changes sign, each time the tick is crossed. Checks every such tick
/// against the difference first seen for it.
fn check_watched_records(pool: &Pool, seen: &mut BTreeMap<i32, (U256, U256, bool)>, context: &str) {
    let both = |state: &TickState| state.info.liquidity_gross > 0 && state.info.watchers > 0;
    seen.retain(|tick, _| pool.ticks.get(tick).is_some_and(both));
    for (tick, state) in pool.ticks.iter().filter(|(_, state)| both(state)) {
        let diff = (
            state
                .info
                .fee_growth_outside0
                .wrapping_sub(state.watched_outside0),
            state
                .info
                .fee_growth_outside1
                .wrapping_sub(state.watched_outside1),
        );
        let above = pool.tick() >= *tick;
        match seen.get(tick) {
            None => {
                seen.insert(*tick, (diff.0, diff.1, above));
            }
            Some(&(d0, d1, above_then)) => {
                let expected = if above == above_then {
                    (d0, d1)
                } else {
                    (d0.wrapping_neg(), d1.wrapping_neg())
                };
                assert_eq!(diff, expected, "{context}: tick {tick}");
            }
        }
    }
}

/// Random swaps, mints, burns and extra watches near several watched
/// ranges, replayed on a pool that watches them. Checks after every event
/// that the replica holds the contract's state, and at the end that fee
/// growth inside each range agrees with the observer.
fn watched_fee_scenario(
    seed: u64,
    (fee_pips, spacing): (u32, i32),
    fee_protocol: (u8, u8),
    rounds: usize,
) {
    let s = spacing;
    // A bitmap word covers 256 spacings. Ranges on plain ticks, on word
    // starts (0, 256) and on word ends (-1, 255), some sharing a bound.
    let ranges = [
        (-13 * s, 17 * s),
        (0, 13 * s),
        (-s, 255 * s),
        (255 * s, 256 * s),
        (13 * s, 256 * s),
    ];
    let liquidity = 3_000_000_000_000_000_000u128;
    let mut special: Vec<i32> = ranges.iter().flat_map(|(l, u)| [*l, *u]).collect();
    special.extend([-256 * s, 512 * s, 254 * s, s, 257 * s]);
    special.sort_unstable();
    special.dedup();
    let bound_prices: Vec<U256> = special
        .iter()
        .map(|t| get_sqrt_ratio_at_tick(*t).unwrap())
        .collect();

    let mut rng = Rng(seed);
    let mut executed = Pool::new(fee_pips, s);
    executed.initialize(Q96).unwrap();
    executed
        .mint(-2_000 * s, 2_000 * s, 1_000_000_000_000_000_000_000)
        .unwrap();
    executed.set_fee_protocol(fee_protocol.0, fee_protocol.1);
    let mut watching = executed.clone();
    for (lower, upper) in ranges {
        watching.watch_tick(lower).unwrap();
        watching.watch_tick(upper).unwrap();
    }
    let mut trackers: Vec<Tracker> = ranges
        .iter()
        .map(|(lower, upper)| Tracker::start(&watching, *lower, *upper, liquidity))
        .collect();

    let pick = |rng: &mut Rng, pool: &Pool| -> i32 {
        if rng.chance(60) {
            special[rng.below(special.len() as u64) as usize]
        } else {
            let near = pool.tick() + (rng.below(120) as i32 - 60) * s;
            near.clamp(-400 * s, 600 * s).div_euclid(s) * s
        }
    };

    let mut positions: Vec<(i32, i32, u128)> = Vec::new();
    let mut extra_watches: Vec<i32> = Vec::new();
    let mut seen = BTreeMap::new();
    let mut swaps = 0;
    let mut swaps_onto_special_ticks = 0;
    // Whether the next round swaps a unit or two from where the price is:
    // most often wanted right after the price landed on a watched tick.
    let mut stay = false;

    for round in 0..rounds {
        let context = format!("seed {seed} round {round}");
        let roll = if stay { 19 } else { rng.below(20) };
        let event = match roll {
            0..=2 => {
                let (a, b) = (pick(&mut rng, &executed), pick(&mut rng, &executed));
                let (tick_lower, tick_upper) = (a.min(b), a.max(b));
                let amount = (1 + rng.below(1_000_000) as u128) * 100_000_000_000_000;
                executed
                    .mint(tick_lower, tick_upper, amount)
                    .ok()
                    .map(|(amount0, amount1)| {
                        positions.push((tick_lower, tick_upper, amount));
                        PoolEvent::Mint {
                            tick_lower,
                            tick_upper,
                            amount,
                            amount0,
                            amount1,
                        }
                    })
            }
            3..=4 if !positions.is_empty() => {
                let index = rng.below(positions.len() as u64) as usize;
                let (tick_lower, tick_upper, amount) = positions.swap_remove(index);
                let (amount0, amount1) = executed.burn(tick_lower, tick_upper, amount).unwrap();
                Some(PoolEvent::Burn {
                    tick_lower,
                    tick_upper,
                    amount,
                    amount0,
                    amount1,
                })
            }
            5 => {
                let tick = pick(&mut rng, &executed);
                watching.watch_tick(tick).unwrap();
                extra_watches.push(tick);
                None
            }
            6 if !extra_watches.is_empty() => {
                let index = rng.below(extra_watches.len() as u64) as usize;
                watching.unwatch_tick(extra_watches.swap_remove(index));
                None
            }
            _ => {
                let down = rng.chance(50);
                let kind = if stay { 2 + rng.below(2) } else { rng.below(5) };
                let swap = match kind {
                    0 | 1 => {
                        let tick = pick(&mut rng, &executed);
                        swap_exactly_to(&mut executed, tick)
                    }
                    2 => swap_amount(&mut executed, down, pos(1 + rng.below(3) as u128)),
                    3 => swap_amount(&mut executed, down, neg(1 + rng.below(1_000) as u128)),
                    _ => {
                        // Part of the way toward a tick, by a random amount.
                        let limit = get_sqrt_ratio_at_tick(pick(&mut rng, &executed)).unwrap();
                        let magnitude = (1 + rng.below(1_000_000) as u128)
                            * 10u128.pow(9 + rng.below(10) as u32);
                        let amount = if rng.chance(60) {
                            pos(magnitude)
                        } else {
                            neg(magnitude)
                        };
                        let down = limit < executed.sqrt_price_x96();
                        executed
                            .swap(down, amount, limit, &mut ())
                            .ok()
                            .map(|amounts| event_of(&executed, amounts))
                    }
                };
                swap.map(|event| {
                    swaps += 1;
                    if bound_prices.contains(&event.sqrt_price_x96) {
                        swaps_onto_special_ticks += 1;
                    }
                    PoolEvent::Swap(event)
                })
            }
        };
        if let Some(event) = event {
            let mut observer = |step: &StepInfo| {
                for tracker in trackers.iter_mut() {
                    tracker.on_step(step);
                }
            };
            replay(&mut watching, &event, &mut observer)
                .unwrap_or_else(|e| panic!("{context}: {e}"));
        }
        assert_contract_state(&watching, &executed, &context);
        check_watched_records(&watching, &mut seen, &context);
        stay = bound_prices.contains(&executed.sqrt_price_x96()) && rng.chance(60);
    }

    assert!(swaps > rounds / 2, "seed {seed}: only {swaps} swaps");
    assert!(
        swaps_onto_special_ticks > rounds / 10,
        "seed {seed}: only {swaps_onto_special_ticks} swaps ended on a watched tick"
    );
    for tracker in &trackers {
        tracker.check(&watching, &format!("seed {seed}"));
        assert!(tracker.steps > 0, "seed {seed}: a range never earned");
    }
}

#[test]
fn fee_growth_inside_watched_ranges_matches_the_observer_over_a_long_random_run() {
    watched_fee_scenario(0x5EED_0000_0000_0002, (500, 10), (0, 0), 800);
    watched_fee_scenario(0x5EED_0000_0000_0003, (500, 10), (4, 6), 800);
    watched_fee_scenario(0x0BAD_CAFE_F00D_0004, (100, 1), (0, 0), 800);
    watched_fee_scenario(0x0BAD_CAFE_F00D_0005, (3000, 60), (0, 0), 800);
    watched_fee_scenario(0x0BAD_CAFE_F00D_0006, (10_000, 200), (0, 5), 800);
}

// --- failures change nothing --------------------------------------------

#[test]
fn a_mint_or_burn_that_fails_on_its_upper_tick_changes_nothing() {
    let mut pool = pool_with_background();
    pool.mint(-100, 100, 5_000).unwrap();
    pool.watch_tick(-100).unwrap();
    let before = pool.clone();

    // The lower tick holds enough liquidity, the upper one holds none.
    assert_eq!(
        pool.burn(-100, 300, 5_000),
        Err(PoolError::Math(MathError::LiquidityDelta))
    );
    assert_eq!(pool, before);
    assert!(
        pool.tick_info(300).is_none(),
        "no empty tick is left behind"
    );

    // The lower tick is new, the upper one would go over the per-tick maximum.
    let room = tick_spacing_to_max_liquidity_per_tick(10)
        - pool.tick_info(20_000).unwrap().liquidity_gross;
    assert_eq!(
        pool.mint(-300, 20_000, room + 1),
        Err(PoolError::TickLiquidityOverflow)
    );
    assert_eq!(pool, before);
    assert!(pool.tick_info(-300).is_none());
    pool.mint(-300, 20_000, room).unwrap();
}

#[test]
fn a_flash_that_overflows_changes_nothing() {
    let mut pool = Pool::new(500, 10);
    pool.initialize(Q96).unwrap();
    pool.mint(-100, 100, 1).unwrap();
    let before = pool.clone();
    // Token 0's growth fits, token 1's does not.
    assert_eq!(
        pool.flash(U256::from(5u8), U256::MAX),
        Err(PoolError::Math(MathError::Overflow256))
    );
    assert_eq!(pool, before);
}

#[test]
fn a_swap_that_fails_midway_changes_nothing_and_reports_no_steps() {
    // Corrupt state: the active liquidity is less than what tick -100 takes
    // away when crossed going down. The swap trades and crosses a watched
    // tick before the crossing fails.
    let mut source = Pool::new(500, 10);
    source.initialize(Q96).unwrap();
    source.mint(-100, 100, 5_000_000_000).unwrap();
    let mut snapshot = source.snapshot();
    snapshot.liquidity = 1_000_000_000;
    let mut pool = Pool::from_snapshot(snapshot);
    pool.watch_tick(-50).unwrap();
    let before = pool.clone();

    let mut steps = 0;
    let limit = get_sqrt_ratio_at_tick(-200).unwrap();
    let result = pool.swap(true, pos(u128::MAX >> 2), limit, &mut |_: &StepInfo| {
        steps += 1
    });
    assert_eq!(result, Err(PoolError::Math(MathError::LiquidityDelta)));
    assert_eq!(pool, before);
    assert_eq!(steps, 0);

    // The same walk as a replayed event.
    let event = SwapEvent {
        amount0: pos(1_000_000),
        amount1: neg(1),
        sqrt_price_x96: limit,
        liquidity: 0,
        tick: -200,
    };
    let result = pool.apply_swap_event(&event, Replay::Tolerant, &mut |_: &StepInfo| steps += 1);
    assert_eq!(result, Err(PoolError::Math(MathError::LiquidityDelta)));
    assert_eq!(pool, before);
    assert_eq!(steps, 0);
}

#[test]
fn a_refused_replay_changes_nothing_and_reports_no_steps() {
    let mut executed = pool_with_background();
    executed.mint(100, 200, 7_000_000_000_000_000_000).unwrap();
    let mut watching = executed.clone();
    watching.watch_tick(150).unwrap();
    let mut event = swap_exactly_to(&mut executed, 250).unwrap();
    event.tick += 1;

    let before = watching.clone();
    let mut steps = 0;
    for mode in [Replay::Strict, Replay::Tolerant] {
        let result = watching.apply_swap_event(&event, mode, &mut |_: &StepInfo| steps += 1);
        assert!(matches!(result, Err(PoolError::ReplayMismatch(_))));
        assert_eq!(watching, before);
    }
    assert_eq!(steps, 0);

    event.tick -= 1;
    watching
        .apply_swap_event(&event, Replay::Tolerant, &mut |_: &StepInfo| steps += 1)
        .unwrap();
    assert!(steps > 2, "the steps come once the swap has gone through");
}

// --- snapshots ----------------------------------------------------------

fn snapshot_of(pool: &Pool) -> PoolSnapshot {
    PoolSnapshot {
        fee_pips: pool.fee_pips(),
        tick_spacing: pool.tick_spacing(),
        sqrt_price_x96: pool.sqrt_price_x96(),
        tick: pool.tick(),
        liquidity: pool.liquidity(),
        fee_growth_global0: pool.fee_growth_global().0,
        fee_growth_global1: pool.fee_growth_global().1,
        fee_protocol0: pool.fee_protocol().0,
        fee_protocol1: pool.fee_protocol().1,
        ticks: pool.ticks().map(|(tick, info)| (tick, *info)).collect(),
    }
}

#[test]
fn a_pool_survives_a_snapshot_round_trip() {
    let mut pool = pool_with_background();
    pool.set_fee_protocol(4, 5);
    pool.mint(100, 200, 3_000_000_000).unwrap();
    pool.swap(
        false,
        pos(u128::MAX >> 2),
        get_sqrt_ratio_at_tick(150).unwrap(),
        &mut (),
    )
    .unwrap();
    let restored = Pool::from_snapshot(snapshot_of(&pool));
    assert_eq!(restored, pool);
    assert!(pool.differences(&restored).is_empty());

    // The pool's own snapshot is the same thing, and a watched tick
    // without liquidity is not part of it.
    assert_eq!(pool.snapshot(), snapshot_of(&pool));
    let before = pool.snapshot();
    pool.watch_tick(1_230).unwrap();
    assert_eq!(pool.snapshot(), before);
}

#[test]
fn differences_are_listed_in_plain_words() {
    let a = pool_with_background();
    let mut b = a.clone();
    assert!(a.differences(&b).is_empty());

    b.mint(100, 200, 5).unwrap();
    b.swap(
        true,
        pos(1_000_000_000_000),
        MIN_SQRT_RATIO + one(),
        &mut (),
    )
    .unwrap();
    let diffs = a.differences(&b);
    assert!(
        diffs.iter().any(|d| d.starts_with("sqrt price")),
        "{diffs:?}"
    );
    assert!(
        diffs.iter().any(|d| d.starts_with("fee growth global 0")),
        "{diffs:?}"
    );
    assert!(
        diffs
            .iter()
            .any(|d| d == "tick 100: only in the second pool"),
        "{diffs:?}"
    );

    // A watched tick is not a difference.
    let mut watched = a.clone();
    watched.watch_tick(50).unwrap();
    assert!(a.differences(&watched).is_empty());
}
