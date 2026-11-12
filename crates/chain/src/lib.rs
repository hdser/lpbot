//! Chain access.
//!
//! All chain reads and writes go through one `RpcPool` with three tiers:
//! the local node (every read that feeds a decision), an optional second own
//! node, and public endpoints (cross-check, exit-only fallback, backfill).
//! No decision is ever made on public-endpoint data alone.
//!
//! Not implemented yet. See `docs/DESIGN.md`, "Node and RPC layer".

/// Where an answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Local node: ExEx, IPC or WebSocket.
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
