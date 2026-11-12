//! One step of a swap inside a single liquidity range (SwapMath).

use alloy_primitives::{I256, U256};

use super::full_math::{mul_div, mul_div_rounding_up};
use super::sqrt_price_math::{
    get_amount0_delta, get_amount1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};
use super::MathResult;

/// Fees are expressed in hundredths of a basis point: 1e6 is 100%.
const FEE_DENOMINATOR: u32 = 1_000_000;

/// Result of one swap step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapStep {
    /// Price after the step. Never beyond the target.
    pub sqrt_ratio_next: U256,
    /// Amount swapped in, without the fee.
    pub amount_in: U256,
    /// Amount received.
    pub amount_out: U256,
    /// Part of the input taken as fee.
    pub fee_amount: U256,
}

/// Computes how far a swap gets inside one range.
///
/// `amount_remaining` is positive for an exact-input swap and negative for an
/// exact-output swap. The direction follows from the two prices: the swap is
/// token0-for-token1 when the target is at or below the current price.
pub fn compute_swap_step(
    sqrt_ratio_current: U256,
    sqrt_ratio_target: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> MathResult<SwapStep> {
    let zero_for_one = sqrt_ratio_current >= sqrt_ratio_target;
    let exact_in = !amount_remaining.is_negative();
    let remaining_abs = amount_remaining.unsigned_abs();

    let mut amount_in = U256::ZERO;
    let mut amount_out = U256::ZERO;
    let sqrt_ratio_next;

    if exact_in {
        let remaining_less_fee = mul_div(
            remaining_abs,
            U256::from(FEE_DENOMINATOR - fee_pips),
            U256::from(FEE_DENOMINATOR),
        )?;
        amount_in = if zero_for_one {
            get_amount0_delta(sqrt_ratio_target, sqrt_ratio_current, liquidity, true)?
        } else {
            get_amount1_delta(sqrt_ratio_current, sqrt_ratio_target, liquidity, true)?
        };
        sqrt_ratio_next = if remaining_less_fee >= amount_in {
            sqrt_ratio_target
        } else {
            get_next_sqrt_price_from_input(
                sqrt_ratio_current,
                liquidity,
                remaining_less_fee,
                zero_for_one,
            )?
        };
    } else {
        amount_out = if zero_for_one {
            get_amount1_delta(sqrt_ratio_target, sqrt_ratio_current, liquidity, false)?
        } else {
            get_amount0_delta(sqrt_ratio_current, sqrt_ratio_target, liquidity, false)?
        };
        sqrt_ratio_next = if remaining_abs >= amount_out {
            sqrt_ratio_target
        } else {
            get_next_sqrt_price_from_output(
                sqrt_ratio_current,
                liquidity,
                remaining_abs,
                zero_for_one,
            )?
        };
    }

    let max = sqrt_ratio_target == sqrt_ratio_next;

    if zero_for_one {
        if !(max && exact_in) {
            amount_in = get_amount0_delta(sqrt_ratio_next, sqrt_ratio_current, liquidity, true)?;
        }
        if !(max && !exact_in) {
            amount_out = get_amount1_delta(sqrt_ratio_next, sqrt_ratio_current, liquidity, false)?;
        }
    } else {
        if !(max && exact_in) {
            amount_in = get_amount1_delta(sqrt_ratio_current, sqrt_ratio_next, liquidity, true)?;
        }
        if !(max && !exact_in) {
            amount_out = get_amount0_delta(sqrt_ratio_current, sqrt_ratio_next, liquidity, false)?;
        }
    }

    // Never hand out more than the remaining output amount.
    if !exact_in && amount_out > remaining_abs {
        amount_out = remaining_abs;
    }

    let fee_amount = if exact_in && sqrt_ratio_next != sqrt_ratio_target {
        // The target was not reached: the rest of the input is the fee.
        remaining_abs - amount_in
    } else {
        mul_div_rounding_up(
            amount_in,
            U256::from(fee_pips),
            U256::from(FEE_DENOMINATOR - fee_pips),
        )?
    };

    Ok(SwapStep {
        sqrt_ratio_next,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{e18, encode_price_sqrt, u};
    use super::*;

    fn pos(x: U256) -> I256 {
        I256::from_raw(x)
    }

    fn neg(x: U256) -> I256 {
        -I256::from_raw(x)
    }

    fn two_e18() -> u128 {
        e18(2).to::<u128>()
    }

    #[test]
    fn exact_in_capped_at_the_price_target_one_for_zero() {
        let price = encode_price_sqrt(1, 1);
        let target = encode_price_sqrt(101, 100);
        let step = compute_swap_step(price, target, two_e18(), pos(e18(1)), 600).unwrap();

        assert_eq!(step.amount_in, u("9975124224178055"));
        assert_eq!(step.fee_amount, u("5988667735148"));
        assert_eq!(step.amount_out, u("9925619580021728"));
        assert!(step.amount_in + step.fee_amount < e18(1));

        let after_whole_input =
            get_next_sqrt_price_from_input(price, two_e18(), e18(1), false).unwrap();
        assert_eq!(step.sqrt_ratio_next, target);
        assert!(step.sqrt_ratio_next < after_whole_input);
    }

    #[test]
    fn exact_out_capped_at_the_price_target_one_for_zero() {
        let price = encode_price_sqrt(1, 1);
        let target = encode_price_sqrt(101, 100);
        let step = compute_swap_step(price, target, two_e18(), neg(e18(1)), 600).unwrap();

        assert_eq!(step.amount_in, u("9975124224178055"));
        assert_eq!(step.fee_amount, u("5988667735148"));
        assert_eq!(step.amount_out, u("9925619580021728"));
        assert!(step.amount_out < e18(1));

        let after_whole_output =
            get_next_sqrt_price_from_output(price, two_e18(), e18(1), false).unwrap();
        assert_eq!(step.sqrt_ratio_next, target);
        assert!(step.sqrt_ratio_next < after_whole_output);
    }

    #[test]
    fn exact_in_fully_spent_one_for_zero() {
        let price = encode_price_sqrt(1, 1);
        let target = encode_price_sqrt(1000, 100);
        let step = compute_swap_step(price, target, two_e18(), pos(e18(1)), 600).unwrap();

        assert_eq!(step.amount_in, u("999400000000000000"));
        assert_eq!(step.fee_amount, u("600000000000000"));
        assert_eq!(step.amount_out, u("666399946655997866"));
        assert_eq!(step.amount_in + step.fee_amount, e18(1));

        let after_input_less_fee =
            get_next_sqrt_price_from_input(price, two_e18(), e18(1) - step.fee_amount, false)
                .unwrap();
        assert!(step.sqrt_ratio_next < target);
        assert_eq!(step.sqrt_ratio_next, after_input_less_fee);
    }

    #[test]
    fn exact_out_fully_received_one_for_zero() {
        let price = encode_price_sqrt(1, 1);
        let target = encode_price_sqrt(10000, 100);
        let step = compute_swap_step(price, target, two_e18(), neg(e18(1)), 600).unwrap();

        assert_eq!(step.amount_in, u("2000000000000000000"));
        assert_eq!(step.fee_amount, u("1200720432259356"));
        assert_eq!(step.amount_out, e18(1));

        let after_whole_output =
            get_next_sqrt_price_from_output(price, two_e18(), e18(1), false).unwrap();
        assert!(step.sqrt_ratio_next < target);
        assert_eq!(step.sqrt_ratio_next, after_whole_output);
    }

    #[test]
    fn amount_out_is_capped_at_the_desired_amount() {
        let step = compute_swap_step(
            u("417332158212080721273783715441582"),
            u("1452870262520218020823638996"),
            159344665391607089467575320103,
            neg(U256::from(1u8)),
            1,
        )
        .unwrap();
        assert_eq!(step.amount_in, U256::from(1u8));
        assert_eq!(step.fee_amount, U256::from(1u8));
        assert_eq!(step.amount_out, U256::from(1u8)); // would be 2 if not capped
        assert_eq!(step.sqrt_ratio_next, u("417332158212080721273783715441581"));
    }

    #[test]
    fn target_price_of_one_uses_partial_input() {
        let step = compute_swap_step(
            U256::from(2u8),
            U256::from(1u8),
            1,
            pos(u("3915081100057732413702495386755767")),
            1,
        )
        .unwrap();
        assert_eq!(step.amount_in, u("39614081257132168796771975168"));
        assert_eq!(step.fee_amount, u("39614120871253040049813"));
        assert!(step.amount_in + step.fee_amount <= u("3915081100057732413702495386755767"));
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next, U256::from(1u8));
    }

    #[test]
    fn entire_input_taken_as_fee() {
        let step = compute_swap_step(
            U256::from(2413u16),
            u("79887613182836312"),
            1985041575832132834610021537970,
            pos(U256::from(10u8)),
            1872,
        )
        .unwrap();
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.fee_amount, U256::from(10u8));
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next, U256::from(2413u16));
    }

    #[test]
    fn insufficient_liquidity_zero_for_one_exact_output() {
        let sqrt_p = u("20282409603651670423947251286016");
        let target = sqrt_p * U256::from(11u8) / U256::from(10u8);
        let step = compute_swap_step(sqrt_p, target, 1024, neg(U256::from(4u8)), 3000).unwrap();
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.sqrt_ratio_next, target);
        assert_eq!(step.amount_in, U256::from(26215u32));
        assert_eq!(step.fee_amount, U256::from(79u8));
    }

    #[test]
    fn insufficient_liquidity_one_for_zero_exact_output() {
        let sqrt_p = u("20282409603651670423947251286016");
        let target = sqrt_p * U256::from(9u8) / U256::from(10u8);
        let step =
            compute_swap_step(sqrt_p, target, 1024, neg(U256::from(263000u32)), 3000).unwrap();
        assert_eq!(step.amount_out, U256::from(26214u32));
        assert_eq!(step.sqrt_ratio_next, target);
        assert_eq!(step.amount_in, U256::from(1u8));
        assert_eq!(step.fee_amount, U256::from(1u8));
    }
}
