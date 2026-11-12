use alloy_primitives::{I256, U256};

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
