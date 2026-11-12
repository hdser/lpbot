//! Venue adapters.
//!
//! A venue supplies three things: the exact pool math, a state replica that
//! is rebuilt from events, and the encoding of position changes into calls.
//! Uniswap v3 is the first venue. v4 and v3 forks plug into the same trait.
//!
//! Not implemented yet. Phase 1 adds `uniswap_v3` with a port of the pool's
//! integer math (TickMath, SqrtPriceMath, SwapMath, fee growth), checked
//! against the deployed pool on a mainnet fork.

pub mod uniswap_v3;

use lpbot_core::{PoolId, Tick};

/// Static facts about a pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolInfo {
    pub id: PoolId,
    pub token0: String,
    pub token1: String,
    /// Fee in hundredths of a basis point, as the pool stores it (500 = 0.05%).
    pub fee_pips: u32,
    pub tick_spacing: Tick,
}

impl PoolInfo {
    /// Fee tier as a fraction (0.0005 for 500 pips).
    pub fn fee_fraction(&self) -> f64 {
        f64::from(self.fee_pips) / 1_000_000.0
    }
}

/// A trading venue. Methods are added with the first implementation.
pub trait Venue: Send + Sync {
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_pips_convert_to_a_fraction() {
        let pool = PoolInfo {
            id: PoolId("weth-usdc-5".into()),
            token0: "USDC".into(),
            token1: "WETH".into(),
            fee_pips: 500,
            tick_spacing: 10,
        };
        assert!((pool.fee_fraction() - 0.0005).abs() < 1e-12);
    }
}
