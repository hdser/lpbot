//! Token amounts and price moves inside one liquidity range (SqrtPriceMath).

use alloy_primitives::U256;

use super::full_math::{div_rounding_up, mul_div, mul_div_rounding_up};
use super::{to_u160, MathError, MathResult, MAX_U160, Q96};

/// Next square-root price after adding or removing an amount of token0.
/// Always rounds up, so the price moves at least as far as it must.
pub fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_p: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> MathResult<U256> {
    // The result is otherwise not guaranteed to equal the input price.
    if amount.is_zero() {
        return Ok(sqrt_p);
    }
    let numerator1: U256 = U256::from(liquidity) << 96;

    if add {
        if let Some(product) = amount.checked_mul(sqrt_p) {
            if let Some(denominator) = numerator1.checked_add(product) {
                // Always fits in 160 bits.
                return mul_div_rounding_up(numerator1, sqrt_p, denominator);
            }
        }
        let denominator = (numerator1 / sqrt_p)
            .checked_add(amount)
            .ok_or(MathError::Overflow256)?;
        div_rounding_up(numerator1, denominator)
    } else {
        // If the product overflows, the denominator would underflow.
        let product = amount
            .checked_mul(sqrt_p)
            .ok_or(MathError::InsufficientReserves)?;
        if numerator1 <= product {
            return Err(MathError::InsufficientReserves);
        }
        let denominator = numerator1 - product;
        to_u160(mul_div_rounding_up(numerator1, sqrt_p, denominator)?)
    }
}

/// Next square-root price after adding or removing an amount of token1.
/// Always rounds down.
pub fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_p: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> MathResult<U256> {
    let liquidity = U256::from(liquidity);
    if add {
        let quotient = if amount <= MAX_U160 {
            (amount << 96) / liquidity
        } else {
            mul_div(amount, Q96, liquidity)?
        };
        to_u160(sqrt_p.checked_add(quotient).ok_or(MathError::Overflow256)?)
    } else {
        let quotient = if amount <= MAX_U160 {
            div_rounding_up(amount << 96, liquidity)?
        } else {
            mul_div_rounding_up(amount, Q96, liquidity)?
        };
        if sqrt_p <= quotient {
            return Err(MathError::InsufficientReserves);
        }
        Ok(sqrt_p - quotient)
    }
}

/// Next square-root price after an input of token0 (`zero_for_one`) or token1.
/// Rounds so the target price is never passed.
pub fn get_next_sqrt_price_from_input(
    sqrt_p: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> MathResult<U256> {
    check_price_and_liquidity(sqrt_p, liquidity)?;
    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount_in, true)
    }
}

/// Next square-root price after an output of token1 (`zero_for_one`) or token0.
/// Rounds so the target price is always passed.
pub fn get_next_sqrt_price_from_output(
    sqrt_p: U256,
    liquidity: u128,
    amount_out: U256,
    zero_for_one: bool,
) -> MathResult<U256> {
    check_price_and_liquidity(sqrt_p, liquidity)?;
    if zero_for_one {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_p, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_p, liquidity, amount_out, false)
    }
}

fn check_price_and_liquidity(sqrt_p: U256, liquidity: u128) -> MathResult<()> {
    if sqrt_p.is_zero() {
        return Err(MathError::ZeroSqrtPrice);
    }
    if liquidity == 0 {
        return Err(MathError::ZeroLiquidity);
    }
    Ok(())
}

/// Amount of token0 between two square-root prices: `L * (1/sqrt(a) - 1/sqrt(b))`.
pub fn get_amount0_delta(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> MathResult<U256> {
    let (lower, upper) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    if lower.is_zero() {
        return Err(MathError::ZeroSqrtPrice);
    }
    let numerator1: U256 = U256::from(liquidity) << 96;
    let numerator2 = upper - lower;

    if round_up {
        div_rounding_up(mul_div_rounding_up(numerator1, numerator2, upper)?, lower)
    } else {
        Ok(mul_div(numerator1, numerator2, upper)? / lower)
    }
}

/// Amount of token1 between two square-root prices: `L * (sqrt(b) - sqrt(a))`.
pub fn get_amount1_delta(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> MathResult<U256> {
    let (lower, upper) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    let liquidity = U256::from(liquidity);
    if round_up {
        mul_div_rounding_up(liquidity, upper - lower, Q96)
    } else {
        mul_div(liquidity, upper - lower, Q96)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{e18, encode_price_sqrt, u};
    use super::*;

    fn one_e17() -> U256 {
        e18(1) / U256::from(10u8)
    }

    fn liq(x: U256) -> u128 {
        x.to::<u128>()
    }

    // --- get_next_sqrt_price_from_input ---------------------------------

    #[test]
    fn input_rejects_zero_price_and_zero_liquidity() {
        assert_eq!(
            get_next_sqrt_price_from_input(U256::ZERO, 1, one_e17(), false),
            Err(MathError::ZeroSqrtPrice)
        );
        assert_eq!(
            get_next_sqrt_price_from_input(U256::from(1u8), 0, one_e17(), true),
            Err(MathError::ZeroLiquidity)
        );
    }

    #[test]
    fn input_fails_when_the_price_would_overflow() {
        assert!(
            get_next_sqrt_price_from_input(MAX_U160, 1024, U256::from(1024u16), false).is_err()
        );
    }

    #[test]
    fn input_cannot_underflow_the_price() {
        let amount_in = U256::from(1u8) << 255;
        assert_eq!(
            get_next_sqrt_price_from_input(U256::from(1u8), 1, amount_in, true),
            Ok(U256::from(1u8))
        );
    }

    #[test]
    fn input_of_zero_returns_the_input_price() {
        let price = encode_price_sqrt(1, 1);
        for zero_for_one in [true, false] {
            assert_eq!(
                get_next_sqrt_price_from_input(price, liq(one_e17()), U256::ZERO, zero_for_one),
                Ok(price)
            );
        }
    }

    #[test]
    fn input_returns_the_minimum_price_for_max_inputs() {
        let liquidity = u128::MAX;
        let max_amount_no_overflow = U256::MAX - ((U256::from(liquidity) << 96) / MAX_U160);
        assert_eq!(
            get_next_sqrt_price_from_input(MAX_U160, liquidity, max_amount_no_overflow, true),
            Ok(U256::from(1u8))
        );
    }

    #[test]
    fn input_reference_values() {
        assert_eq!(
            get_next_sqrt_price_from_input(encode_price_sqrt(1, 1), liq(e18(1)), one_e17(), false),
            Ok(u("87150978765690771352898345369"))
        );
        assert_eq!(
            get_next_sqrt_price_from_input(encode_price_sqrt(1, 1), liq(e18(1)), one_e17(), true),
            Ok(u("72025602285694852357767227579"))
        );
        assert_eq!(
            get_next_sqrt_price_from_input(
                encode_price_sqrt(1, 1),
                liq(e18(10)),
                U256::from(1u8) << 100,
                true
            ),
            Ok(u("624999999995069620"))
        );
        assert_eq!(
            get_next_sqrt_price_from_input(
                encode_price_sqrt(1, 1),
                1,
                U256::MAX / U256::from(2u8),
                true
            ),
            Ok(U256::from(1u8))
        );
    }

    // --- get_next_sqrt_price_from_output --------------------------------

    #[test]
    fn output_rejects_zero_price_and_zero_liquidity() {
        assert_eq!(
            get_next_sqrt_price_from_output(U256::ZERO, 1, one_e17(), false),
            Err(MathError::ZeroSqrtPrice)
        );
        assert_eq!(
            get_next_sqrt_price_from_output(U256::from(1u8), 0, one_e17(), true),
            Err(MathError::ZeroLiquidity)
        );
    }

    #[test]
    fn output_fails_at_or_beyond_the_virtual_reserves() {
        let price = u("20282409603651670423947251286016");
        let liquidity = 1024u128;
        // token0 reserves are 4, token1 reserves are 262144.
        assert!(get_next_sqrt_price_from_output(price, liquidity, U256::from(4u8), false).is_err());
        assert!(get_next_sqrt_price_from_output(price, liquidity, U256::from(5u8), false).is_err());
        assert!(
            get_next_sqrt_price_from_output(price, liquidity, U256::from(262145u32), true).is_err()
        );
        assert!(
            get_next_sqrt_price_from_output(price, liquidity, U256::from(262144u32), true).is_err()
        );
        assert_eq!(
            get_next_sqrt_price_from_output(price, liquidity, U256::from(262143u32), true),
            Ok(u("77371252455336267181195264"))
        );
    }

    #[test]
    fn output_of_zero_returns_the_input_price() {
        let price = encode_price_sqrt(1, 1);
        for zero_for_one in [true, false] {
            assert_eq!(
                get_next_sqrt_price_from_output(price, liq(one_e17()), U256::ZERO, zero_for_one),
                Ok(price)
            );
        }
    }

    #[test]
    fn output_reference_values() {
        assert_eq!(
            get_next_sqrt_price_from_output(encode_price_sqrt(1, 1), liq(e18(1)), one_e17(), false),
            Ok(u("88031291682515930659493278152"))
        );
        assert_eq!(
            get_next_sqrt_price_from_output(encode_price_sqrt(1, 1), liq(e18(1)), one_e17(), true),
            Ok(u("71305346262837903834189555302"))
        );
    }

    #[test]
    fn output_fails_when_the_amount_is_impossible() {
        let price = encode_price_sqrt(1, 1);
        assert!(get_next_sqrt_price_from_output(price, 1, U256::MAX, true).is_err());
        assert!(get_next_sqrt_price_from_output(price, 1, U256::MAX, false).is_err());
    }

    // --- get_amount0_delta ----------------------------------------------

    #[test]
    fn amount0_is_zero_without_liquidity_or_price_difference() {
        assert_eq!(
            get_amount0_delta(encode_price_sqrt(1, 1), encode_price_sqrt(2, 1), 0, true),
            Ok(U256::ZERO)
        );
        assert_eq!(
            get_amount0_delta(encode_price_sqrt(1, 1), encode_price_sqrt(1, 1), 0, true),
            Ok(U256::ZERO)
        );
    }

    #[test]
    fn amount0_reference_values() {
        let a = encode_price_sqrt(1, 1);
        let b = encode_price_sqrt(121, 100);
        let up = get_amount0_delta(a, b, liq(e18(1)), true).unwrap();
        assert_eq!(up, u("90909090909090910"));
        let down = get_amount0_delta(a, b, liq(e18(1)), false).unwrap();
        assert_eq!(down, up - U256::from(1u8));
    }

    #[test]
    fn amount0_works_for_prices_that_overflow() {
        let a = u("2787593149816327892691964784081045188247552");
        let b = u("22300745198530623141535718272648361505980416");
        let up = get_amount0_delta(a, b, liq(e18(1)), true).unwrap();
        let down = get_amount0_delta(a, b, liq(e18(1)), false).unwrap();
        assert_eq!(up, down + U256::from(1u8));
    }

    // --- get_amount1_delta ----------------------------------------------

    #[test]
    fn amount1_is_zero_without_liquidity_or_price_difference() {
        assert_eq!(
            get_amount1_delta(encode_price_sqrt(1, 1), encode_price_sqrt(2, 1), 0, true),
            Ok(U256::ZERO)
        );
        assert_eq!(
            get_amount1_delta(encode_price_sqrt(1, 1), encode_price_sqrt(1, 1), 0, true),
            Ok(U256::ZERO)
        );
    }

    #[test]
    fn amount1_reference_values() {
        let a = encode_price_sqrt(1, 1);
        let b = encode_price_sqrt(121, 100);
        let up = get_amount1_delta(a, b, liq(e18(1)), true).unwrap();
        assert_eq!(up, u("100000000000000000"));
        let down = get_amount1_delta(a, b, liq(e18(1)), false).unwrap();
        assert_eq!(down, up - U256::from(1u8));
    }

    // --- swap computation -----------------------------------------------

    #[test]
    fn sqrt_p_times_sqrt_q_overflow_case() {
        // Reference: "swap computation: sqrtP * sqrtQ overflows".
        let sqrt_p = u("1025574284609383690408304870162715216695788925244");
        let liquidity = 50015962439936049619261659728067971248u128;
        let amount_in = U256::from(406u16);
        let sqrt_q = get_next_sqrt_price_from_input(sqrt_p, liquidity, amount_in, true).unwrap();
        assert_eq!(
            sqrt_q,
            u("1025574284609383582644711336373707553698163132913")
        );
        assert_eq!(
            get_amount0_delta(sqrt_q, sqrt_p, liquidity, true),
            Ok(U256::from(406u16))
        );
    }
}
