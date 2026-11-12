//! Conversion between liquidity and token amounts for a range
//! (LiquidityAmounts from v3-periphery).

use alloy_primitives::U256;

use super::full_math::mul_div;
use super::{MathError, MathResult, Q96};

fn ordered(sqrt_a: U256, sqrt_b: U256) -> (U256, U256) {
    if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    }
}

fn to_u128(x: U256) -> MathResult<u128> {
    u128::try_from(x).map_err(|_| MathError::Overflow128)
}

/// Liquidity bought by an amount of token0 over a range.
pub fn get_liquidity_for_amount0(sqrt_a: U256, sqrt_b: U256, amount0: U256) -> MathResult<u128> {
    let (lower, upper) = ordered(sqrt_a, sqrt_b);
    let intermediate = mul_div(lower, upper, Q96)?;
    to_u128(mul_div(amount0, intermediate, upper - lower)?)
}

/// Liquidity bought by an amount of token1 over a range.
pub fn get_liquidity_for_amount1(sqrt_a: U256, sqrt_b: U256, amount1: U256) -> MathResult<u128> {
    let (lower, upper) = ordered(sqrt_a, sqrt_b);
    to_u128(mul_div(amount1, Q96, upper - lower)?)
}

/// The most liquidity the given amounts can buy over a range at the current price.
pub fn get_liquidity_for_amounts(
    sqrt_price: U256,
    sqrt_a: U256,
    sqrt_b: U256,
    amount0: U256,
    amount1: U256,
) -> MathResult<u128> {
    let (lower, upper) = ordered(sqrt_a, sqrt_b);
    if sqrt_price <= lower {
        get_liquidity_for_amount0(lower, upper, amount0)
    } else if sqrt_price < upper {
        let liquidity0 = get_liquidity_for_amount0(sqrt_price, upper, amount0)?;
        let liquidity1 = get_liquidity_for_amount1(lower, sqrt_price, amount1)?;
        Ok(liquidity0.min(liquidity1))
    } else {
        get_liquidity_for_amount1(lower, upper, amount1)
    }
}

/// Amount of token0 held by a liquidity amount over a range, rounded down.
pub fn get_amount0_for_liquidity(sqrt_a: U256, sqrt_b: U256, liquidity: u128) -> MathResult<U256> {
    let (lower, upper) = ordered(sqrt_a, sqrt_b);
    if lower.is_zero() {
        return Err(MathError::ZeroSqrtPrice);
    }
    Ok(mul_div(U256::from(liquidity) << 96, upper - lower, upper)? / lower)
}

/// Amount of token1 held by a liquidity amount over a range, rounded down.
pub fn get_amount1_for_liquidity(sqrt_a: U256, sqrt_b: U256, liquidity: u128) -> MathResult<U256> {
    let (lower, upper) = ordered(sqrt_a, sqrt_b);
    mul_div(U256::from(liquidity), upper - lower, Q96)
}

/// Token amounts held by a liquidity amount over a range at the current price.
///
/// Below the range the position is all token0, above it all token1.
pub fn get_amounts_for_liquidity(
    sqrt_price: U256,
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
) -> MathResult<(U256, U256)> {
    let (lower, upper) = ordered(sqrt_a, sqrt_b);
    if sqrt_price <= lower {
        Ok((
            get_amount0_for_liquidity(lower, upper, liquidity)?,
            U256::ZERO,
        ))
    } else if sqrt_price < upper {
        Ok((
            get_amount0_for_liquidity(sqrt_price, upper, liquidity)?,
            get_amount1_for_liquidity(lower, sqrt_price, liquidity)?,
        ))
    } else {
        Ok((
            U256::ZERO,
            get_amount1_for_liquidity(lower, upper, liquidity)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::encode_price_sqrt;
    use super::*;

    // Reference vectors from v3-periphery's LiquidityAmounts tests.

    #[test]
    fn liquidity_for_amounts_inside_the_range() {
        let liquidity = get_liquidity_for_amounts(
            encode_price_sqrt(1, 1),
            encode_price_sqrt(100, 110),
            encode_price_sqrt(110, 100),
            U256::from(100u8),
            U256::from(200u8),
        );
        assert_eq!(liquidity, Ok(2148));
    }

    #[test]
    fn liquidity_for_amounts_below_the_range() {
        let liquidity = get_liquidity_for_amounts(
            encode_price_sqrt(99, 110),
            encode_price_sqrt(100, 110),
            encode_price_sqrt(110, 100),
            U256::from(100u8),
            U256::from(200u8),
        );
        assert_eq!(liquidity, Ok(1048));
    }

    #[test]
    fn liquidity_for_amounts_above_the_range() {
        let liquidity = get_liquidity_for_amounts(
            encode_price_sqrt(111, 100),
            encode_price_sqrt(100, 110),
            encode_price_sqrt(110, 100),
            U256::from(100u8),
            U256::from(200u8),
        );
        assert_eq!(liquidity, Ok(2097));
    }

    #[test]
    fn liquidity_for_amounts_at_the_range_edges() {
        let a = encode_price_sqrt(100, 110);
        let b = encode_price_sqrt(110, 100);
        assert_eq!(
            get_liquidity_for_amounts(a, a, b, U256::from(100u8), U256::from(200u8)),
            Ok(1048)
        );
        assert_eq!(
            get_liquidity_for_amounts(b, a, b, U256::from(100u8), U256::from(200u8)),
            Ok(2097)
        );
    }

    #[test]
    fn amounts_for_liquidity_inside_the_range() {
        let amounts = get_amounts_for_liquidity(
            encode_price_sqrt(1, 1),
            encode_price_sqrt(100, 110),
            encode_price_sqrt(110, 100),
            2148,
        );
        assert_eq!(amounts, Ok((U256::from(99u8), U256::from(99u8))));
    }

    #[test]
    fn amounts_for_liquidity_below_and_above_the_range() {
        let a = encode_price_sqrt(100, 110);
        let b = encode_price_sqrt(110, 100);
        assert_eq!(
            get_amounts_for_liquidity(encode_price_sqrt(99, 110), a, b, 1048),
            Ok((U256::from(99u8), U256::ZERO))
        );
        assert_eq!(
            get_amounts_for_liquidity(encode_price_sqrt(111, 100), a, b, 2097),
            Ok((U256::ZERO, U256::from(199u8)))
        );
    }

    #[test]
    fn amounts_for_liquidity_at_the_range_edges() {
        let a = encode_price_sqrt(100, 110);
        let b = encode_price_sqrt(110, 100);
        assert_eq!(
            get_amounts_for_liquidity(a, a, b, 1048),
            Ok((U256::from(99u8), U256::ZERO))
        );
        assert_eq!(
            get_amounts_for_liquidity(b, a, b, 2097),
            Ok((U256::ZERO, U256::from(199u8)))
        );
    }

    #[test]
    fn a_position_below_its_range_holds_only_token0_and_gets_it_all_back() {
        // The core play in tick terms: enter one-sided, price crosses the
        // range and returns, and the position holds the same token0 again.
        let lower = encode_price_sqrt(101, 100);
        let upper = encode_price_sqrt(102, 100);
        let liquidity = 1_000_000_000_000u128;

        let below = encode_price_sqrt(1, 1);
        let (in0, in1) = get_amounts_for_liquidity(below, lower, upper, liquidity).unwrap();
        assert!(in0 > U256::ZERO);
        assert_eq!(in1, U256::ZERO);

        let above = encode_price_sqrt(103, 100);
        let (mid0, mid1) = get_amounts_for_liquidity(above, lower, upper, liquidity).unwrap();
        assert_eq!(mid0, U256::ZERO);
        assert!(mid1 > U256::ZERO);

        let (out0, out1) = get_amounts_for_liquidity(below, lower, upper, liquidity).unwrap();
        assert_eq!((out0, out1), (in0, in1));
    }
}
