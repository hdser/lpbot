//! `lpbot verify-fees`: the replica's fees against what real positions
//! collected, from the stored history.

use std::path::Path;

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Result};
use lpbot_data::fee_check::{
    check_fees, check_fees_on_chain, summarize, FeeCheckError, FeeCheckSummary, PositionCheck,
    PositionReader, Verdict,
};
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::EventStore;

/// The gate asks for 0.1%.
pub const TOLERANCE: f64 = 0.001;

fn describe(check: &PositionCheck) -> String {
    let owed = check.owed();
    let still = match check.owed_on_chain {
        Some((owed0, owed1)) => format!(", the pool still owes {owed0} / {owed1}"),
        None => String::new(),
    };
    format!(
        "owner {:#x} ticks {}..{}: collected {} / {}{still}, replica says {} / {}",
        check.key.owner,
        check.key.tick_lower,
        check.key.tick_upper,
        check.collected.0,
        check.collected.1,
        owed.0,
        owed.1
    )
}

/// Prints the check without the pool's own records of its positions and
/// returns its counts. Every agreement it finds is unverified, so it
/// passes only when nothing is asked for; `run_with` with a node is the
/// check that can pass.
pub fn run(root: &Path, chain: &str, pool: Address, needed: usize) -> Result<FeeCheckSummary> {
    run_with(root, chain, pool, needed, None)
}

/// Prints the check and returns its counts; `passed(needed)` on them says
/// whether it passed. `reader` (the node, an archive node for an old
/// history) tells which positions started empty and what the pool still
/// owes each at the end. When it cannot answer, the check runs without
/// it and says so.
pub fn run_with(
    root: &Path,
    chain: &str,
    pool: Address,
    needed: usize,
    reader: Option<&dyn PositionReader>,
) -> Result<FeeCheckSummary> {
    let days = EventStore::new(root).days(chain, pool)?;
    let Some((last, _)) = days.last() else {
        bail!(
            "no events stored for pool {pool:#x} under {}; run `lpbot backfill --pool {pool:#x}` first",
            root.display()
        );
    };
    let snapshot = SnapshotStore::new(root)
        .earliest(chain, pool)?
        .ok_or_else(|| {
            anyhow!(
                "no snapshot stored for pool {pool:#x}; run `lpbot backfill --pool {pool:#x}` first"
            )
        })?;
    let first = snapshot.day;
    let logs = EventStore::new(root).read_days(chain, pool, first, *last)?;
    println!(
        "pool {pool:#x}: {} stored logs from {first} to {last}",
        logs.len()
    );

    let checks = match reader {
        Some(reader) => match check_fees_on_chain(snapshot.clone(), &logs, reader) {
            Ok(checks) => {
                println!(
                    "the pool's own records say which positions started empty and what it \
                     still owes each"
                );
                checks
            }
            Err(FeeCheckError::Store(error)) => return Err(error.into()),
            Err(error) => {
                println!(
                    "{error}. The check runs without them, and its agreements stay unverified"
                );
                check_fees(snapshot, &logs)?
            }
        },
        None => {
            println!(
                "no node to read the pool's records from: agreements stay unverified, as \
                 fees left in the pool from before the history cannot be ruled out"
            );
            check_fees(snapshot, &logs)?
        }
    };
    let summary = summarize(&checks, TOLERANCE);
    println!(
        "{} positions started inside the history and could be followed",
        checks.len()
    );
    println!(
        "  {} emptied, and collected plus still owed exactly what the replica computed",
        summary.exact
    );
    println!(
        "  {} emptied and within {}% of it",
        summary.close,
        TOLERANCE * 100.0
    );
    println!(
        "  {} still hold liquidity or cannot be judged from their collections",
        summary.open
    );
    println!(
        "  {} agree but are unverified without the pool's records",
        summary.unverified
    );
    println!("  {} do not fit the replica", summary.mismatched);
    for check in checks
        .iter()
        .filter(|c| c.verdict(TOLERANCE) == Verdict::Mismatch)
        .take(10)
    {
        println!("    {}", describe(check));
    }

    if summary.passed(needed) {
        println!("PASS");
    } else if summary.mismatched > 0 {
        println!("FAIL: the replica's fees differ from what was collected");
    } else if summary.unverified > 0 {
        println!(
            "NOT ENOUGH: {} positions confirm the replica and {} more agree unverified; the \
             check asks for {needed}. Run it against an archive node, which has the pool's \
             records for the whole history",
            summary.exact + summary.close,
            summary.unverified
        );
    } else {
        println!(
            "NOT ENOUGH: {} positions confirm the replica, the check asks for {needed}. \
             Store a longer history or use a busier pool",
            summary.exact + summary.close
        );
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chain_cmd::Tokens;
    use alloy_primitives::address;

    #[test]
    fn the_demo_history_has_no_collections_so_the_check_asks_for_more() {
        let root = std::env::temp_dir().join(format!("lpbot-fees-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut tokens = Tokens::new();
        tokens.insert(
            "WETH".into(),
            (address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"), 18),
        );
        tokens.insert(
            "USDC".into(),
            (address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"), 6),
        );
        crate::demo_cmd::run(&root, "ethereum", &tokens, 2, 1).unwrap();

        // Every position is followed and none contradicts the replica,
        // but without Collect events none can confirm it either.
        let summary = run(&root, "ethereum", crate::demo_cmd::DEMO_POOL, 20).unwrap();
        assert!(!summary.passed(20));
        assert_eq!(summary.mismatched, 0);
        assert!(summary.passed(0));
        assert!(run(&root, "ethereum", Address::repeat_byte(7), 0).is_err());

        // With the pool's records: a pool that owes nothing to positions
        // that burned and never collected contradicts the replica.
        use lpbot_chain::uniswap_v3::ChainError;
        use lpbot_data::fee_check::{PositionKey, PositionState};
        struct OwesNothing;
        impl PositionReader for OwesNothing {
            fn positions(
                &self,
                _: Address,
                keys: &[PositionKey],
                _: u64,
            ) -> Result<Vec<PositionState>, ChainError> {
                Ok(vec![PositionState::default(); keys.len()])
            }
        }
        let checked = run_with(
            &root,
            "ethereum",
            crate::demo_cmd::DEMO_POOL,
            20,
            Some(&OwesNothing),
        )
        .unwrap();
        assert!(checked.mismatched > 0, "{checked:?}");
        assert!(!checked.passed(0));

        // A node that cannot answer leaves the check as it is without one.
        struct Down;
        impl PositionReader for Down {
            fn positions(
                &self,
                _: Address,
                _: &[PositionKey],
                _: u64,
            ) -> Result<Vec<PositionState>, ChainError> {
                Err(ChainError::Other("missing trie node".into()))
            }
        }
        let fallback = run_with(
            &root,
            "ethereum",
            crate::demo_cmd::DEMO_POOL,
            20,
            Some(&Down),
        )
        .unwrap();
        assert_eq!(fallback, summary);
        let _ = std::fs::remove_dir_all(&root);
    }
}
