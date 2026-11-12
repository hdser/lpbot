//! Domain types shared by every crate.

use serde::{Deserialize, Serialize};

/// A Uniswap v3 tick index.
pub type Tick = i32;

/// Identifier of a pool as it appears in config and result tables.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PoolId(pub String);

/// One range a strategy wants to hold, with its share of the strategy's capital.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeTarget {
    pub tick_lower: Tick,
    pub tick_upper: Tick,
    pub weight: f64,
}

impl RangeTarget {
    pub fn width(&self) -> Tick {
        self.tick_upper - self.tick_lower
    }
}

/// Why a position is being taken out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    RoundTripComplete,
    PatienceClose,
    LossLimit,
    Operator,
    Watchdog,
    Infrastructure,
}

/// What a strategy asks for. Intents are declarative: the planner diffs them
/// against on-chain positions and produces the actions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Intent {
    SetPositions {
        pool: PoolId,
        targets: Vec<RangeTarget>,
    },
    Exit {
        pool: PoolId,
        reason: ExitReason,
    },
}

/// Which side of the entry price a one-sided range was placed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrySide {
    /// Range above the price at entry: the position starts fully in token0.
    Above,
    /// Range below the price at entry: the position starts fully in token1.
    Below,
}

/// One entry, the fees it collects, and its exit, valued in one token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Campaign {
    pub id: String,
    pub pool: PoolId,
    /// Symbol of the token results are counted in.
    pub value_token: String,
    pub tick_lower: Tick,
    pub tick_upper: Tick,
    pub side: EntrySide,
    /// Amount of the value token put in, in the token's smallest unit.
    pub amount_in_raw: u128,
    pub opened_at_block: u64,
}

impl Campaign {
    pub fn width(&self) -> Tick {
        self.tick_upper - self.tick_lower
    }

    /// True when price has travelled through the whole range away from the
    /// entry side, so the position is fully in the other token.
    pub fn is_beyond_range(&self, current_tick: Tick) -> bool {
        match self.side {
            EntrySide::Above => current_tick >= self.tick_upper,
            EntrySide::Below => current_tick < self.tick_lower,
        }
    }
}

/// Normalized events consumed by signals, strategies, risk and monitoring.
///
/// This is deliberately minimal for now. Pool events are added with the
/// state replica in phase 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    BlockEnd { number: u64, timestamp: u64 },
    Timer { name: String, timestamp: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn campaign(side: EntrySide) -> Campaign {
        Campaign {
            id: "c1".into(),
            pool: PoolId("p".into()),
            value_token: "WETH".into(),
            tick_lower: 100,
            tick_upper: 200,
            side,
            amount_in_raw: 1,
            opened_at_block: 0,
        }
    }

    #[test]
    fn beyond_range_depends_on_entry_side() {
        let above = campaign(EntrySide::Above);
        assert!(!above.is_beyond_range(50));
        assert!(!above.is_beyond_range(150));
        assert!(above.is_beyond_range(200));

        let below = campaign(EntrySide::Below);
        assert!(!below.is_beyond_range(250));
        assert!(!below.is_beyond_range(100));
        assert!(below.is_beyond_range(99));
    }
}
