//! Chain access.
//!
//! All chain reads go through `RpcClient`. Endpoints come in three tiers:
//! the local node (every read that feeds a decision), an optional second own
//! node, and public endpoints (cross-check, exit-only fallback, backfill).
//! No decision is ever made on public-endpoint data alone.
//!
//! What exists: the JSON-RPC client, log fetching with adaptive chunking,
//! pool snapshots, block times, pool discovery through the factory, and the check that
//! the replica agrees with a deployed pool. Live subscriptions, reorg
//! handling and transaction submission are not built yet.

pub mod clock;
pub mod eth;
pub mod rpc;
pub mod uniswap_v3;

#[cfg(test)]
pub(crate) mod testing;

pub use rpc::{RpcClient, RpcError};

/// Where an answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Local node: ExEx, IPC, HTTP or WebSocket.
    Local,
    /// A second node you run.
    OwnFallback,
    /// A public endpoint.
    Public,
}

impl Tier {
    /// Only data from your own nodes may feed a decision.
    pub fn may_feed_decisions(self) -> bool {
        !matches!(self, Tier::Public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_endpoints_never_feed_decisions() {
        assert!(Tier::Local.may_feed_decisions());
        assert!(Tier::OwnFallback.may_feed_decisions());
        assert!(!Tier::Public.may_feed_decisions());
    }
}
