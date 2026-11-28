//! What the engine did to its position, step by step: the seam between
//! deciding and carrying out.
//!
//! The engine records every change it makes to its position as a
//! [`StepRecord`]: liquidity added or taken out of a range, and swaps. In a
//! backtest the record is only kept. The executor (`lpbot_exec::planner`)
//! turns the same records into the calls that make the change on the chain,
//! bounded by the amounts recorded, so what a live run sends is what the
//! simulation decided, step by step.

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};

use crate::Tick;

/// One change to the position, in the pool's own token order: `amount0` is
/// token0's smallest unit, `amount1` token1's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    /// Liquidity added to `[tick_lower, tick_upper)`, and what the pool took
    /// for it, rounded up as the pool rounds.
    Mint {
        tick_lower: Tick,
        tick_upper: Tick,
        #[serde(with = "u128_text")]
        liquidity: u128,
        amount0: U256,
        amount1: U256,
    },
    /// Liquidity taken out of `[tick_lower, tick_upper)`: the principal it
    /// gave back, rounded down, and the fees collected with it.
    Burn {
        tick_lower: Tick,
        tick_upper: Tick,
        #[serde(with = "u128_text")]
        liquidity: u128,
        amount0: U256,
        amount1: U256,
        fees0: U256,
        fees1: U256,
    },
    /// `amount_in` of token0 swapped for token1 (`zero_for_one`), or of
    /// token1 for token0, and what came out.
    Swap {
        zero_for_one: bool,
        amount_in: U256,
        amount_out: U256,
    },
}

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::Mint { .. } => "mint",
            Step::Burn { .. } => "burn",
            Step::Swap { .. } => "swap",
        }
    }
}

/// A `u128` as a decimal string, which every JSON reader keeps exact (a
/// tagged enum cannot hold a `u128` as a number in serde).
pub mod u128_text {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &u128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&value.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<u128, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(D::Error::custom)
    }
}

/// A step and when and why the engine took it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    /// The block it landed in, and that block's time.
    pub block: u64,
    pub time: u64,
    /// The campaign it belongs to, counted from 1 within the run.
    pub campaign: u32,
    /// The action it carries out: `open`, `close`, `withdraw`, `flip` or
    /// `recenter`.
    pub action: String,
    /// The pool's price when the action was carried out, as the square root
    /// of token1 per token0 in Q64.96: what the executor bounds against.
    pub sqrt_price_x96: U256,
    pub step: Step,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_reads_back_from_json_as_it_was_written() {
        let record = StepRecord {
            block: 21_000_000,
            time: 1_730_000_000,
            campaign: 2,
            action: "open".into(),
            sqrt_price_x96: U256::from(1u128 << 96),
            step: Step::Mint {
                tick_lower: -600,
                tick_upper: 0,
                liquidity: 12_345,
                amount0: U256::ZERO,
                amount1: U256::from(10_000_000_000u64),
            },
        };
        let text = serde_json::to_string(&record).unwrap();
        assert!(text.contains("\"kind\":\"mint\""), "{text}");
        let back: StepRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(back, record);
        assert_eq!(back.step.name(), "mint");
    }
}
