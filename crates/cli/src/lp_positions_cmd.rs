//! `lpbot lp-positions`: a liquidity provider's positions on the stored
//! pools, read from the node through the Uniswap v3 position manager, one
//! row per stretch with liquidity in place. A backtest can follow the file
//! (`strategy.follow`), replaying the provider's ranges with its own
//! capital.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use alloy_primitives::{Address, B256, U256};
use anyhow::{Context, Result};
use lpbot_chain::eth::Log;
use lpbot_chain::uniswap_v3::ChainError;
use lpbot_chain::RpcClient;
use lpbot_core::Config;
use lpbot_data::backfill::{first_block_at_or_after, NodeSource};
use lpbot_data::day::Day;
use lpbot_data::lake::stored_pools;
use lpbot_data::lp_history::{
    holdings_of_many, owner_of, read_positions, LpPosition, LpSource, POSITION_MANAGER,
};
use lpbot_data::snapshot::SnapshotStore;

use crate::pool_stats_cmd::quote_for;

/// The node, as the position history reads it.
struct NodeLp<'a> {
    client: &'a RpcClient,
    chunk: u64,
}

impl LpSource for NodeLp<'_> {
    fn logs(&self, topics: &[Vec<B256>], from: u64, to: u64) -> Result<Vec<Log>, ChainError> {
        // A long scan says how far it is, every million blocks or so.
        let mut reported = from;
        Ok(self.client.get_logs_chunked_where(
            &[POSITION_MANAGER],
            topics,
            from,
            to,
            self.chunk,
            |done, found| {
                if done - reported >= 1_000_000 && to - from > 1_000_000 {
                    eprintln!("    block {done} of {to}: {found} log(s)");
                    reported = done;
                }
            },
        )?)
    }

    fn call(&self, data: &[u8], block: u64) -> Result<Vec<u8>, ChainError> {
        Ok(self.client.eth_call(POSITION_MANAGER, data, block)?)
    }
}

pub struct LpPositionsArgs<'a> {
    pub nfts: &'a [U256],
    pub accounts: &'a [Address],
    pub from: Day,
    pub root: &'a Path,
    pub out: &'a Path,
    pub chain: &'a str,
    pub log_chunk: u64,
}

/// A stored pool, by its tokens and fee.
type PoolIndex = BTreeMap<(Address, Address, u32), (Address, String)>;

fn stored_pool_index(config: &Config, root: &Path, chain: &str) -> Result<PoolIndex> {
    let snapshots = SnapshotStore::new(root);
    let mut index = BTreeMap::new();
    for pool in stored_pools(root, chain)? {
        let Some(day) = snapshots.days(chain, pool)?.first().copied() else {
            continue;
        };
        let snapshot = snapshots.read(chain, pool, day)?;
        let quote = quote_for(config, chain, snapshot.token0, snapshot.token1)?;
        let name = format!(
            "{}/{} {}",
            quote.symbol0,
            quote.symbol1,
            f64::from(snapshot.state.fee_pips) / 10_000.0
        );
        index.insert(
            (snapshot.token0, snapshot.token1, snapshot.state.fee_pips),
            (pool, name),
        );
    }
    Ok(index)
}

/// The CSV header of the file the command writes and a follow reads.
pub const HEADER: &str = "owner,token_id,pool,pool_name,tick_lower,tick_upper,open_block,open_time,close_block,close_time,deposited0,deposited1,withdrawn0,withdrawn1,collected0,collected1";

/// One row per stretch with liquidity in place, for the positions on
/// stored pools; the times are looked up by `time_of`.
pub fn rows(
    positions: &[LpPosition],
    pools: &PoolIndex,
    time_of: &BTreeMap<u64, u64>,
) -> Vec<String> {
    let mut out = Vec::new();
    for position in positions {
        let key = position.key;
        let Some((pool, name)) = pools.get(&(key.token0, key.token1, key.fee)) else {
            continue;
        };
        for span in position.spans() {
            let time = |block: u64| time_of.get(&block).map(u64::to_string).unwrap_or_default();
            let mut row = String::new();
            let _ = write!(
                row,
                "{:#x},{},{:#x},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                position.owner,
                position.token_id,
                pool,
                name,
                key.tick_lower,
                key.tick_upper,
                span.open_block,
                time(span.open_block),
                span.close_block.map(|b| b.to_string()).unwrap_or_default(),
                span.close_block.map(time).unwrap_or_default(),
                span.deposited.0,
                span.deposited.1,
                span.withdrawn.0,
                span.withdrawn.1,
                span.collected.0,
                span.collected.1,
            );
            out.push(row);
        }
    }
    out
}

/// Reads every owner's positions since `from` and writes them to `out`.
/// Returns the rows written.
pub fn run(config: &Config, client: &RpcClient, args: &LpPositionsArgs) -> Result<usize> {
    let source = NodeLp {
        client,
        chunk: args.log_chunk,
    };
    let head = client.block_number()?;
    let blocks = NodeSource {
        client,
        head_margin: 0,
        log_chunk: args.log_chunk,
    };
    let from = first_block_at_or_after(&blocks, args.from.start(), 1, head)
        .context("finding the first block of --from")?;

    let mut owners: Vec<Address> = args.accounts.to_vec();
    for nft in args.nfts {
        match owner_of(&source, *nft, head)? {
            Some(owner) => {
                println!("  NFT {nft} belongs to {owner:#x}");
                owners.push(owner);
            }
            None => println!("  NFT {nft} is burned; its owner cannot be read, left out"),
        }
    }
    owners.sort();
    owners.dedup();

    let pools = stored_pool_index(config, args.root, args.chain)?;
    println!(
        "  reading the NFTs of {} owner(s) since {}",
        owners.len(),
        args.from
    );
    let mut all_held = holdings_of_many(&source, &owners, from, head)?;
    let mut all = Vec::new();
    for owner in &owners {
        let held = all_held.remove(owner).unwrap_or_default();
        let positions = read_positions(&source, *owner, &held)?;
        let on_ours: Vec<&LpPosition> = positions
            .iter()
            .filter(|p| pools.contains_key(&(p.key.token0, p.key.token1, p.key.fee)))
            .collect();
        let spans: usize = on_ours.iter().map(|p| p.spans().len()).sum();
        let open = on_ours
            .iter()
            .filter(|p| p.spans().last().is_some_and(|s| s.close_block.is_none()))
            .count();
        println!(
            "  {owner:#x}: {} NFT(s) received since {}, {} on stored pools, {spans} stretch(es) with liquidity, {open} open now",
            held.len(),
            args.from,
            on_ours.len()
        );
        all.extend(positions);
    }

    // The time of every block a stretch opens or closes in.
    let mut wanted: Vec<u64> = all
        .iter()
        .flat_map(|p| {
            p.spans()
                .into_iter()
                .flat_map(|s| std::iter::once(s.open_block).chain(s.close_block))
        })
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    let times = client.block_times(&wanted)?;
    let time_of: BTreeMap<u64, u64> = wanted.into_iter().zip(times).collect();

    let lines = rows(&all, &pools, &time_of);
    let mut text = String::from(HEADER);
    text.push('\n');
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(args.out, text).with_context(|| format!("writing {}", args.out.display()))?;
    Ok(lines.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_data::lp_history::{LpEvent, LpEventKind, PositionKey};

    #[test]
    fn a_row_per_stretch_on_a_stored_pool() {
        let pool = Address::repeat_byte(0x55);
        let key = PositionKey {
            token0: Address::repeat_byte(1),
            token1: Address::repeat_byte(2),
            fee: 500,
            tick_lower: -10,
            tick_upper: 10,
        };
        let event = |block: u64, kind: LpEventKind, liquidity: u128, amount: u64| LpEvent {
            block,
            log_index: 0,
            kind,
            liquidity,
            amount0: U256::from(amount),
            amount1: U256::from(amount),
        };
        let position = |token_id: u64, key: PositionKey| LpPosition {
            token_id: U256::from(token_id),
            owner: Address::repeat_byte(9),
            key,
            events: vec![
                event(10, LpEventKind::Increase, 100, 5),
                event(20, LpEventKind::Decrease, 100, 4),
                event(20, LpEventKind::Collect, 0, 6),
            ],
        };
        let mut pools = PoolIndex::new();
        pools.insert(
            (key.token0, key.token1, 500),
            (pool, "USDC/WETH 0.05".into()),
        );
        let elsewhere = PositionKey { fee: 3_000, ..key };
        let times = BTreeMap::from([(10, 1_000), (20, 2_000)]);
        let rows = rows(&[position(7, key), position(8, elsewhere)], &pools, &times);
        assert_eq!(
            rows.len(),
            1,
            "the position on a pool not stored is left out"
        );
        let fields: Vec<&str> = rows[0].split(',').collect();
        assert_eq!(fields.len(), HEADER.split(',').count());
        assert_eq!(fields[1], "7");
        assert_eq!(fields[3], "USDC/WETH 0.05");
        assert_eq!(&fields[4..10], &["-10", "10", "10", "1000", "20", "2000"]);
        assert_eq!(&fields[10..], &["5", "5", "4", "4", "6", "6"]);
    }
}
