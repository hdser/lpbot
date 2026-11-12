//! `lpbot backfill`: pool history from the node into the local data folder.

use std::path::{Path, PathBuf};

use alloy_primitives::Address;
use anyhow::{anyhow, bail, Result};
use lpbot_chain::uniswap_v3::{discover_pools, fetch_snapshot};
use lpbot_data::backfill::{backfill_pool, ChainSource, NodeSource, PoolRef};
use lpbot_data::day::Day;
use lpbot_data::lake::replay_from_snapshot;
use lpbot_data::snapshot::SnapshotStore;
use lpbot_data::store::DayState;
use lpbot_venues::uniswap_v3::pool::Pool;

use crate::chain_cmd::Chain;

const HEAD_MARGIN: u64 = 5;

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

pub fn run(chain: &Chain, args: &BackfillArgs) -> Result<()> {
    let source = NodeSource {
        client: &chain.client,
        head_margin: HEAD_MARGIN,
        log_chunk: args.log_chunk,
    };
    let (head, head_time) = source.head()?;
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
        "backfilling {} pool(s) on {} from {from} to block {head}, into {}",
        chosen.len(),
        chain.name,
        args.root.display()
    );

    let mut failed = Vec::new();
    for pool in chosen {
        println!("pool {:#x}", pool.address);
        let reference = PoolRef {
            address: pool.address,
            token0: pool.token0,
            token1: pool.token1,
        };
        let outcome = backfill_pool(
            &source,
            args.root,
            &chain.name,
            reference,
            from,
            |day, rows, state| match state {
                DayState::Partial => println!("  {day}  {rows:>7} logs (day in progress)"),
                _ if rows > 0 => println!("  {day}  {rows:>7} logs"),
                _ => {}
            },
        );
        let report = match outcome {
            Ok(report) => report,
            Err(error) => {
                println!("  failed: {error}");
                failed.push(pool.address);
                continue;
            }
        };
        println!(
            "  {} day(s) written with {} logs, {} already stored",
            report.written.len(),
            report.rows_written(),
            report.skipped
        );

        if args.verify {
            let snapshot = SnapshotStore::new(args.root)
                .latest_on_or_before(&chain.name, pool.address, from)?
                .ok_or_else(|| anyhow!("no stored snapshot for {:#x}", pool.address))?;
            let since = snapshot.day;
            let (replica, events) =
                replay_from_snapshot(args.root, &chain.name, snapshot, Day::of_time(head_time))?;
            let on_chain = Pool::from_snapshot(fetch_snapshot(
                &chain.client,
                pool.address,
                report.head_block,
            )?);
            let differences = replica.differences(&on_chain);
            if differences.is_empty() {
                println!(
                    "  verify: PASS, {events} events since {since} replay to the pool's state at block {}",
                    report.head_block
                );
            } else {
                println!("  verify: FAIL, {} difference(s)", differences.len());
                for difference in differences.iter().take(10) {
                    println!("    {difference}");
                }
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
    Ok(())
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
    fn the_data_folder_argument_wins() {
        assert_eq!(
            data_dir(Some(Path::new("/tmp/lake"))),
            PathBuf::from("/tmp/lake")
        );
    }
}
