//! `lpbot backfill`: pool history from the node into the local data folder.

use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Result};
use lpbot_chain::uniswap_v3::discover_pools;
use lpbot_data::backfill::{
    backfill_pools, verify_backfill, AtHead, BackfillError, NodeSource, PoolDay, PoolRef, Verified,
};
use lpbot_data::day::Day;
use lpbot_data::store::DayState;

use crate::chain_cmd::{safe_head, Chain, HEAD_MARGIN};

/// Where the stored history lives: `--data-dir`, then `LPBOT_DATA_DIR`,
/// then `data/lake` under the current folder.
pub fn data_dir(argument: Option<&Path>) -> PathBuf {
    match argument {
        Some(path) => path.to_path_buf(),
        None => std::env::var_os("LPBOT_DATA_DIR")
            .filter(|value| !value.is_empty())
            .map_or_else(|| PathBuf::from("data/lake"), PathBuf::from),
    }
}

/// The first day to backfill: `--from` when given, else `days` whole days
/// before the day of the head.
pub fn first_day(from: Option<Day>, days: u32, head_time: u64) -> Day {
    from.unwrap_or_else(|| Day::of_time(head_time).minus(days))
}

pub struct BackfillArgs<'a> {
    /// Pools to backfill. Empty means every pool of the universe.
    pub pools: &'a [Address],
    pub from: Option<Day>,
    pub days: u32,
    pub root: &'a Path,
    pub log_chunk: u64,
    /// Replay what is stored and compare it with the pool on the chain.
    pub verify: bool,
}

/// What a backfill run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stored {
    /// The first day that is stored.
    pub from: Day,
    pub pools: usize,
    /// Days and logs written by this run; days stored before are not counted.
    pub days_written: usize,
    pub logs_written: usize,
}

/// The line printed for a day: how many pools this run fetched it for and
/// their logs. Nothing for a day every pool had stored already.
pub fn day_line(day: Day, seen: &[PoolDay]) -> Option<String> {
    let fetched: Vec<&PoolDay> = seen.iter().filter(|p| p.fetched).collect();
    if fetched.is_empty() {
        return None;
    }
    let rows: usize = fetched.iter().map(|p| p.rows).sum();
    let open = if fetched.iter().any(|p| p.state == DayState::Partial) {
        " (day in progress)"
    } else {
        ""
    };
    Some(format!(
        "  {day}  {:>3} pool(s)  {rows:>8} logs{open}",
        fetched.len()
    ))
}

/// The line `--verify` prints for a pool, and whether the pool passed.
/// A pool with nothing stored yet has nothing to contradict the chain.
pub fn verify_line(outcome: &Result<Option<Verified>, BackfillError>) -> (String, bool) {
    match outcome {
        Ok(None) => ("verify: nothing stored yet to replay".into(), true),
        Ok(Some(verified)) if verified.differences.is_empty() => (
            format!(
                "verify: PASS, {} events since {} replay to the pool's state at block {}",
                verified.events, verified.since, verified.block
            ),
            true,
        ),
        Ok(Some(verified)) => {
            let mut text = format!(
                "verify: FAIL at block {}, {} difference(s)",
                verified.block,
                verified.differences.len()
            );
            for difference in verified.differences.iter().take(10) {
                text.push_str(&format!("\n    {difference}"));
            }
            (text, false)
        }
        Err(error) => (format!("verify: could not finish: {error}"), false),
    }
}

pub fn run(chain: &Chain, args: &BackfillArgs) -> Result<Stored> {
    let node = NodeSource {
        client: &chain.client,
        head_margin: HEAD_MARGIN,
        log_chunk: args.log_chunk,
    };
    // One head for the whole run: every pool is stored up to it and
    // checked at it, however long the run takes.
    let head = safe_head(&chain.client)?;
    let kind = match &head.warning {
        Some(warning) => {
            println!("warning: {warning}");
            format!("{HEAD_MARGIN} behind the head")
        }
        None => "finalized".to_string(),
    };
    let source = AtHead {
        source: &node,
        block: head.block,
        time: head.time,
    };
    let (head, head_time) = (head.block, head.time);
    let from = first_day(args.from, args.days, head_time);

    let addresses: Vec<_> = chain.tokens.values().map(|(a, _)| *a).collect();
    let found = discover_pools(&chain.client, chain.factory, &addresses, head)?;
    let chosen: Vec<_> = if args.pools.is_empty() {
        found.iter().collect()
    } else {
        args.pools
            .iter()
            .map(|address| {
                found.iter().find(|p| p.address == *address).ok_or_else(|| {
                    anyhow!("{address:#x} is not a pool of the universe; `lpbot pools` lists them")
                })
            })
            .collect::<Result<_>>()?
    };
    println!(
        "backfilling {} pool(s) on {} from {from} to block {head} ({kind}), into {}",
        chosen.len(),
        chain.name,
        args.root.display()
    );

    let mut stored = Stored {
        from,
        pools: chosen.len(),
        days_written: 0,
        logs_written: 0,
    };
    let mut failed = Vec::new();
    let references: Vec<PoolRef> = chosen
        .iter()
        .map(|pool| PoolRef {
            address: pool.address,
            token0: pool.token0,
            token1: pool.token1,
        })
        .collect();
    // Day by day, every pool that needs the day at once.
    let outcomes = backfill_pools(
        &source,
        args.root,
        &chain.name,
        &references,
        from,
        |day, seen| {
            if let Some(line) = day_line(day, seen) {
                println!("{line}");
            }
        },
    )
    .map_err(|error| {
        anyhow!("the backfill stopped: {error}. Run it again: it carries on from what is stored")
    })?;
    for (pool, outcome) in chosen.iter().zip(outcomes) {
        println!("pool {:#x}", pool.address);
        let report = match outcome {
            Ok(report) => report,
            Err(error) => {
                println!("  failed: {error}");
                failed.push(pool.address);
                continue;
            }
        };
        for note in report.notes() {
            println!("  {note}");
        }
        println!(
            "  {} day(s) written with {} logs, {} already stored",
            report.written.len(),
            report.rows_written(),
            report.skipped
        );
        stored.days_written += report.written.len();
        stored.logs_written += report.rows_written();

        if args.verify {
            let outcome = verify_backfill(&source, args.root, &chain.name, pool.address, &report);
            let (line, passed) = verify_line(&outcome);
            println!("  {line}");
            if !passed {
                failed.push(pool.address);
            }
        }
    }

    if !failed.is_empty() {
        bail!(
            "{} pool(s) did not backfill cleanly: {}",
            failed.len(),
            failed
                .iter()
                .map(|a| format!("{a:#x}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_day_comes_from_the_date_or_the_day_count() {
        let head_time = "2026-11-12".parse::<Day>().unwrap().start() + 3_600;
        assert_eq!(first_day(None, 90, head_time).to_string(), "2026-08-14");
        assert_eq!(first_day(None, 0, head_time).to_string(), "2026-11-12");
        let explicit: Day = "2026-01-01".parse().unwrap();
        assert_eq!(first_day(Some(explicit), 90, head_time), explicit);
    }

    #[test]
    fn the_verify_line_says_where_it_compared_and_only_a_difference_or_an_error_fails() {
        let day: Day = "2026-11-01".parse().unwrap();
        let verified = |differences: Vec<String>| {
            Ok(Some(Verified {
                since: day,
                events: 42,
                block: 1_234,
                differences,
            }))
        };
        let (line, passed) = verify_line(&verified(vec![]));
        assert!(passed);
        assert!(
            line.contains("PASS")
                && line.contains("42 events since 2026-11-01")
                && line.contains("block 1234"),
            "{line}"
        );
        let (line, passed) = verify_line(&verified(vec!["liquidity: 1 vs 2".into()]));
        assert!(!passed);
        assert!(
            line.contains("FAIL at block 1234") && line.contains("liquidity"),
            "{line}"
        );
        assert!(verify_line(&Ok(None)).1);
        let error = BackfillError::Inconsistent("no".into());
        assert!(!verify_line(&Err(error)).1);
    }

    #[test]
    fn a_day_line_counts_the_pools_fetched_and_says_when_the_day_is_open() {
        let day: Day = "2026-11-01".parse().unwrap();
        let entry = |byte, rows, state, fetched| PoolDay {
            pool: Address::repeat_byte(byte),
            rows,
            state,
            fetched,
        };
        assert_eq!(day_line(day, &[]), None);
        assert_eq!(
            day_line(day, &[entry(1, 0, DayState::Complete, false)]),
            None
        );
        let line = day_line(
            day,
            &[
                entry(1, 0, DayState::Complete, false),
                entry(2, 120, DayState::Complete, true),
                entry(3, 30, DayState::Complete, true),
            ],
        )
        .unwrap();
        assert!(
            line.contains("2026-11-01")
                && line.contains("2 pool(s)")
                && line.contains("150 logs")
                && !line.contains("in progress"),
            "{line}"
        );
        let open = day_line(day, &[entry(2, 5, DayState::Partial, true)]).unwrap();
        assert!(open.ends_with("(day in progress)"), "{open}");
    }

    #[test]
    fn the_data_folder_argument_wins() {
        assert_eq!(
            data_dir(Some(Path::new("/tmp/lake"))),
            PathBuf::from("/tmp/lake")
        );
    }
}
