//! 512-bit multiply-then-divide (FullMath) and rounding division (UnsafeMath).

use alloy_primitives::ruint::UintTryFrom;
use alloy_primitives::{U256, U512};

use super::{MathError, MathResult};

/// floor(a * b / denominator) with a 512-bit intermediate product.
pub fn mul_div(a: U256, b: U256, denominator: U256) -> MathResult<U256> {
    if denominator.is_zero() {
        return Err(MathError::DivisionByZero);
    }
    let quotient = U512::from(a) * U512::from(b) / U512::from(denominator);
    U256::uint_try_from(quotient).map_err(|_| MathError::Overflow256)
}

/// ceil(a * b / denominator) with a 512-bit intermediate product.
pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> MathResult<U256> {
    let result = mul_div(a, b, denominator)?;
    if a.mul_mod(b, denominator).is_zero() {
        Ok(result)
    } else {
        result
            .checked_add(U256::from(1u8))
            .ok_or(MathError::Overflow256)
    }
}

/// ceil(x / y). The Solidity original does not check for a zero divisor; this does.
pub fn div_rounding_up(x: U256, y: U256) -> MathResult<U256> {
    if y.is_zero() {
        return Err(MathError::DivisionByZero);
    }
    let (q, r) = x.div_rem(y);
    Ok(if r.is_zero() { q } else { q + U256::from(1u8) })
}

#[cfg(test)]
mod tests {
    use super::super::Q128;
    use super::*;

    #[test]
    fn rejects_a_zero_denominator() {
        assert_eq!(
            mul_div(Q128, U256::from(5u8), U256::ZERO),
            Err(MathError::DivisionByZero)
        );
        assert_eq!(
            div_rounding_up(Q128, U256::ZERO),
            Err(MathError::DivisionByZero)
        );
    }

    #[test]
    fn rejects_results_wider_than_256_bits() {
        assert_eq!(
            mul_div(Q128, Q128, U256::from(1u8)),
            Err(MathError::Overflow256)
        );
        assert_eq!(
            mul_div(U256::MAX, U256::MAX, U256::MAX - U256::from(1u8)),
            Err(MathError::Overflow256)
        );
        assert_eq!(
            mul_div_rounding_up(U256::MAX, U256::MAX, U256::MAX - U256::from(1u8)),
            Err(MathError::Overflow256)
        );
    }

    #[test]
    fn handles_products_wider_than_256_bits() {
        // Reference vectors from v3-core's FullMath tests.
        assert_eq!(mul_div(U256::MAX, U256::MAX, U256::MAX), Ok(U256::MAX));
        assert_eq!(
            mul_div(
                Q128,
                U256::from(50u8) * Q128 / U256::from(100u8),
                U256::from(150u8) * Q128 / U256::from(100u8)
            ),
            Ok(Q128 / U256::from(3u8))
        );
        assert_eq!(
            mul_div(Q128, U256::from(35u8) * Q128, U256::from(8u8) * Q128),
            Ok(U256::from(4375u16) * Q128 / U256::from(1000u16))
        );
        assert_eq!(
            mul_div(Q128, U256::from(1000u16) * Q128, U256::from(3000u16) * Q128),
            Ok(Q128 / U256::from(3u8))
        );
    }

    #[test]
    fn rounding_up_adds_one_only_when_there_is_a_remainder() {
        assert_eq!(
            mul_div_rounding_up(
                Q128,
                U256::from(50u8) * Q128 / U256::from(100u8),
                U256::from(150u8) * Q128 / U256::from(100u8)
            ),
            Ok(Q128 / U256::from(3u8) + U256::from(1u8))
        );
        assert_eq!(
            mul_div_rounding_up(Q128, U256::from(35u8) * Q128, U256::from(8u8) * Q128),
            Ok(U256::from(4375u16) * Q128 / U256::from(1000u16))
        );
        assert_eq!(
            div_rounding_up(U256::from(10u8), U256::from(5u8)),
            Ok(U256::from(2u8))
        );
        assert_eq!(
            div_rounding_up(U256::from(11u8), U256::from(5u8)),
            Ok(U256::from(3u8))
        );
    }
}
