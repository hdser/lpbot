//! `lpbot verify-fees`: the replica's fees against what real positions
//! collected, from the stored history.

use std::path::Path;

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Result};
use lpbot_data::fee_check::{check_fees, summarize, PositionCheck, Verdict};
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::EventStore;

/// The gate asks for 0.1%.
pub const TOLERANCE: f64 = 0.001;

fn describe(check: &PositionCheck) -> String {
    let owed = check.owed();
    format!(
        "owner {:#x} ticks {}..{}: collected {} / {}, replica says {} / {}",
        check.key.owner,
        check.key.tick_lower,
        check.key.tick_upper,
        check.collected.0,
        check.collected.1,
        owed.0,
        owed.1
    )
}

/// Returns whether the check passed.
pub fn run(root: &Path, chain: &str, pool: Address, needed: usize) -> Result<bool> {
    let days = EventStore::new(root).days(chain, pool)?;
    let Some((last, _)) = days.last() else {
        bail!(
            "no events stored for pool {pool:#x} under {}; run `lpbot backfill --pool {pool:#x}` first",
            root.display()
        );
    };
    let snapshots = SnapshotStore::new(root);
    let first = snapshots
        .days(chain, pool)?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no snapshot stored for pool {pool:#x}"))?;
    let snapshot = snapshots.read(chain, pool, first)?;
    let logs = EventStore::new(root).read_days(chain, pool, first, *last)?;
    println!(
        "pool {pool:#x}: {} stored logs from {first} to {last}",
        logs.len()
    );

    let checks = check_fees(snapshot, &logs)?;
    let summary = summarize(&checks, TOLERANCE);
    println!(
        "{} positions started inside the history and could be followed",
        checks.len()
    );
    println!(
        "  {} emptied and collected exactly what the replica computed",
        summary.exact
    );
    println!(
        "  {} emptied and within {}% of it",
        summary.close,
        TOLERANCE * 100.0
    );
    println!(
        "  {} still hold liquidity or have not collected everything",
        summary.open
    );
    println!("  {} do not fit the replica", summary.mismatched);
    for check in checks
        .iter()
        .filter(|c| c.verdict(TOLERANCE) == Verdict::Mismatch)
        .take(10)
    {
        println!("    {}", describe(check));
    }

    let passed = summary.passed(needed);
    if passed {
        println!("PASS");
    } else if summary.mismatched > 0 {
        println!("FAIL: the replica's fees differ from what was collected");
    } else {
        println!(
            "NOT ENOUGH: {} positions confirm the replica, the check asks for {needed}. \
             Store a longer history or use a busier pool",
            summary.exact + summary.close
        );
    }
    Ok(passed)
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
        assert!(!run(&root, "ethereum", crate::demo_cmd::DEMO_POOL, 20).unwrap());
        assert!(run(&root, "ethereum", crate::demo_cmd::DEMO_POOL, 0).unwrap());
        assert!(run(&root, "ethereum", Address::repeat_byte(7), 0).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
