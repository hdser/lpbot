//! Block times without fetching every header.
//!
//! Since the merge a mainnet block lands every 12 seconds unless a slot is
//! missed, so the time of a block between two known headers is found by
//! interpolation. With an anchor every few hundred blocks the error is a
//! few seconds, which is far below anything the zone statistics resolve.
//! Anything that needs exact times reads the header itself.

use crate::rpc::RpcResult;
use crate::RpcClient;

/// Seconds per slot on Ethereum mainnet.
pub const SLOT_SECONDS: u64 = 12;

/// Known block times, with interpolation in between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockClock {
    /// `(block, unix time)`, sorted by block, without duplicates.
    anchors: Vec<(u64, u64)>,
}

impl BlockClock {
    /// Builds a clock from known `(block, unix time)` pairs, in any order.
    pub fn new(mut anchors: Vec<(u64, u64)>) -> Self {
        anchors.sort_unstable();
        anchors.dedup_by_key(|a| a.0);
        Self { anchors }
    }

    pub fn anchors(&self) -> &[(u64, u64)] {
        &self.anchors
    }

    /// Time of a block: exact on an anchor, interpolated between two, and
    /// one slot per block beyond either end. `None` without any anchor.
    pub fn time_of(&self, block: u64) -> Option<u64> {
        let (first, last) = (self.anchors.first()?, self.anchors.last()?);
        if block <= first.0 {
            return Some(first.1.saturating_sub((first.0 - block) * SLOT_SECONDS));
        }
        if block >= last.0 {
            return Some(last.1 + (block - last.0) * SLOT_SECONDS);
        }
        let after = self.anchors.partition_point(|a| a.0 <= block);
        let (b0, t0) = self.anchors[after - 1];
        let (b1, t1) = self.anchors[after];
        let elapsed = u128::from(t1.saturating_sub(t0)) * u128::from(block - b0);
        Some(t0 + (elapsed / u128::from(b1 - b0)) as u64)
    }
}

impl RpcClient {
    /// Reads the headers at both ends of a block range and every `step`
    /// blocks in between.
    pub fn block_clock(&self, from_block: u64, to_block: u64, step: u64) -> RpcResult<BlockClock> {
        let step = step.max(1);
        let mut anchors = Vec::new();
        let mut block = from_block;
        while block < to_block {
            anchors.push((block, self.block_header(block)?.0));
            block = block.saturating_add(step);
        }
        anchors.push((to_block, self.block_header(to_block)?.0));
        Ok(BlockClock::new(anchors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::parse_quantity;
    use crate::testing::MockNode;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn anchors_are_exact_and_blocks_between_are_interpolated() {
        // A missed slot between 100 and 200: 100 blocks took 1,212 seconds.
        let clock = BlockClock::new(vec![(200, 2_212), (100, 1_000), (300, 3_412)]);
        assert_eq!(clock.time_of(100), Some(1_000));
        assert_eq!(clock.time_of(200), Some(2_212));
        assert_eq!(clock.time_of(150), Some(1_606));
        assert_eq!(clock.time_of(250), Some(2_812));
        assert_eq!(clock.time_of(201), Some(2_224));
    }

    #[test]
    fn blocks_beyond_the_anchors_advance_one_slot_each() {
        let clock = BlockClock::new(vec![(100, 1_000), (200, 2_200)]);
        assert_eq!(clock.time_of(90), Some(880));
        assert_eq!(clock.time_of(210), Some(2_320));
        assert_eq!(clock.time_of(0), Some(0));
        assert_eq!(BlockClock::new(vec![]).time_of(5), None);
        assert_eq!(BlockClock::new(vec![(7, 70)]).time_of(7), Some(70));
    }

    #[test]
    fn time_never_runs_backwards() {
        let clock = BlockClock::new(vec![(0, 0), (1_000, 12_500), (2_000, 24_000)]);
        let mut previous = 0;
        for block in 0..=2_100 {
            let time = clock.time_of(block).unwrap();
            assert!(time >= previous);
            previous = time;
        }
    }

    #[test]
    fn the_clock_reads_both_ends_and_every_step_between() {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        let node = MockNode::start(move |method, params| {
            assert_eq!(method, "eth_getBlockByNumber");
            let number = parse_quantity("test", &params[0]).unwrap();
            seen.lock().unwrap().push(number);
            Ok(json!({
                "timestamp": format!("0x{:x}", 1_000 + number * 12),
                "hash": format!("0x{:064x}", number),
            }))
        });
        let clock = RpcClient::new(node.url())
            .block_clock(100, 350, 100)
            .unwrap();
        assert_eq!(*asked.lock().unwrap(), vec![100, 200, 300, 350]);
        assert_eq!(clock.anchors().len(), 4);
        assert_eq!(clock.time_of(325), Some(1_000 + 325 * 12));

        let single = RpcClient::new(node.url()).block_clock(7, 7, 0).unwrap();
        assert_eq!(single.anchors(), &[(7, 1_084)]);
    }
}
