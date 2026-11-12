//! Uniswap v3: exact pool math and the pool-state replica.
//!
//! The math modules are ports of the Solidity libraries in Uniswap v3-core
//! (TickMath, FullMath, SqrtPriceMath, SwapMath) and v3-periphery
//! (LiquidityAmounts). They use the same 256-bit integers, the same rounding
//! and the same step structure, so results match the contracts bit for bit.
//! The unit tests carry the reference vectors from the v3-core test suite.
//!
//! Licensing note: the Solidity originals are published under
//! GPL-2.0-or-later (v3-core's Business Source Licence converted on its
//! change date). This port is a derived work. That has no effect on private
//! use, and it matters if this code is ever distributed.

pub mod full_math;
pub mod liquidity_amounts;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_math;

use alloy_primitives::U256;
use thiserror::Error;

/// 2^96, the fixed-point scale of square-root prices.
pub const Q96: U256 = U256::from_limbs([0, 1 << 32, 0, 0]);
/// 2^128, the fixed-point scale of fee growth.
pub const Q128: U256 = U256::from_limbs([0, 0, 1, 0]);
/// Largest value of a `uint160`.
pub const MAX_U160: U256 = U256::from_limbs([u64::MAX, u64::MAX, u32::MAX as u64, 0]);

/// Where the Solidity code would revert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MathError {
    #[error("division by zero")]
    DivisionByZero,
    #[error("result does not fit in 256 bits")]
    Overflow256,
    #[error("result does not fit in 160 bits")]
    Overflow160,
    #[error("result does not fit in 128 bits")]
    Overflow128,
    #[error("tick is outside the allowed range")]
    TickOutOfRange,
    #[error("square-root price is outside the allowed range")]
    SqrtPriceOutOfRange,
    #[error("square-root price must be greater than zero")]
    ZeroSqrtPrice,
    #[error("liquidity must be greater than zero")]
    ZeroLiquidity,
    #[error("the requested amount exceeds what the range can provide")]
    InsufficientReserves,
    #[error("liquidity underflow or overflow")]
    LiquidityDelta,
}

pub type MathResult<T> = Result<T, MathError>;

/// Checks that a value fits a `uint160`, as Solidity's `toUint160` does.
pub(crate) fn to_u160(x: U256) -> MathResult<U256> {
    if x > MAX_U160 {
        Err(MathError::Overflow160)
    } else {
        Ok(x)
    }
}

/// Adds a signed liquidity delta to a liquidity amount (LiquidityMath.addDelta).
pub fn add_liquidity_delta(liquidity: u128, delta: i128) -> MathResult<u128> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or(MathError::LiquidityDelta)
    } else {
        liquidity
            .checked_add(delta as u128)
            .ok_or(MathError::LiquidityDelta)
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use alloy_primitives::{U256, U512};

    pub fn u(s: &str) -> U256 {
        U256::from_str_radix(s, 10).unwrap()
    }

    pub fn e18(n: u64) -> U256 {
        U256::from(n) * U256::from(10u64).pow(U256::from(18u64))
    }

    /// floor(sqrt(reserve1 / reserve0) * 2^96), like the reference tests' encodePriceSqrt.
    pub fn encode_price_sqrt(reserve1: u64, reserve0: u64) -> U256 {
        let n = (U512::from(reserve1) << 192) / U512::from(reserve0);
        // Integer square root by Newton's method.
        let mut x = n;
        let mut y = (x + U512::from(1u8)) >> 1;
        while y < x {
            x = y;
            y = (x + n / x) >> 1;
        }
        U256::from(x)
    }

    #[test]
    fn constants_have_the_right_values() {
        assert_eq!(super::Q96, U256::from(1u8) << 96);
        assert_eq!(super::Q128, U256::from(1u8) << 128);
        assert_eq!(super::MAX_U160, (U256::from(1u8) << 160) - U256::from(1u8));
        assert_eq!(encode_price_sqrt(1, 1), super::Q96);
        assert_eq!(encode_price_sqrt(10000, 100), super::Q96 * U256::from(10u8));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liquidity_delta_is_checked_in_both_directions() {
        assert_eq!(add_liquidity_delta(10, -4), Ok(6));
        assert_eq!(add_liquidity_delta(10, 5), Ok(15));
        assert_eq!(add_liquidity_delta(3, -4), Err(MathError::LiquidityDelta));
        assert_eq!(
            add_liquidity_delta(u128::MAX, 1),
            Err(MathError::LiquidityDelta)
        );
    }
}
