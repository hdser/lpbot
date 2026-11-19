//! The followed pools kept current from the node, block by block.
//!
//! A [`LiveState`] is every followed pool's replica, the [`PositionBook`]
//! of their positions, and minute bars of what happened. It starts from
//! the stored history ([`replay_history`]): each pool's snapshot, then the
//! stored days of the pools and of the position manager, merged in chain
//! order. From there a [`Follower`] keeps it at the node's head.
//!
//! The follower keeps two states: `base`, at the newest finalized block,
//! which no reorg can change, and `live`, at the head. Blocks past the
//! finalized one stay in a tail with their logs and hashes. Each step
//! checks that the block the live state stands on is still the node's;
//! when it is not, the live state is rebuilt from the base and the tail
//! blocks still on the chain, and the step says where the chains forked.
//!
//! A checkpoint of the base state lets a restart carry on from the
//! finalized block instead of replaying the history.
//!
//! Floating point only in the bars, which are for viewing; the replicas
//! and the book are exact.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::Path;

use alloy_primitives::{Address, B256, I256};
use lpbot_chain::eth::{BlockHeader, HashedLog};
use lpbot_chain::uniswap_v3::{read_nfts_at, read_positions_at, ChainError};
use lpbot_chain::RpcClient;
use lpbot_venues::uniswap_v3::events::PoolEvent;
use lpbot_venues::uniswap_v3::pool::Pool;
use serde::{Deserialize, Serialize};

use crate::day::Day;
use crate::manager::read_merged_day;
use crate::positions::{
    apply_pool_log, decode_manager, BookState, ChainLog, NftSeed, PositionBook, Range, Seed, Stamp,
    Wanted, MANAGER,
};
use crate::snapshot::{from_json, to_json, StoredSnapshot};
use crate::store::{write_durably, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error(transparent)]
    Chain(#[from] ChainError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("pool {pool:#x}, block {block}, log {log_index}: {reason}")]
    Replay {
        pool: Address,
        block: u64,
        log_index: u32,
        reason: String,
    },
    #[error(
        "the chain reorganized below the finalized block {block}, which should not happen; \
         start again with --rebuild"
    )]
    DeepReorg { block: u64 },
    #[error("the checkpoint {path} cannot be used: {reason}. Start with --rebuild")]
    Checkpoint { path: String, reason: String },
    #[error("{0}")]
    Other(String),
}

/// What happened in one pool in one minute. Prices are token1 per token0
/// in smallest units; amounts in smallest units.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    /// Start of the minute, Unix seconds.
    pub minute: u64,
    /// The last block applied in it.
    pub block: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub tick: i32,
    /// Active liquidity at the close.
    pub liquidity: f64,
    /// Swapped, counted on both sides.
    pub volume0: f64,
    pub volume1: f64,
    /// Fees swappers paid, in the token they paid in.
    pub fees0: f64,
    pub fees1: f64,
    pub swaps: u32,
    pub mints: u32,
    pub burns: u32,
}

/// One swap, as it happened.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapRecord {
    pub pool: Address,
    pub block: u64,
    pub time: u64,
    pub log_index: u32,
    pub tx_hash: B256,
    pub sender: Address,
    pub recipient: Address,
    /// What the pool received (positive) or paid (negative), smallest units.
    pub amount0: f64,
    pub amount1: f64,
    /// Token1 per token0 after the swap, smallest units.
    pub price: f64,
    pub tick: i32,
    pub liquidity: f64,
}

/// One followed pool.
#[derive(Debug, Clone, PartialEq)]
pub struct LivePool {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub pool: Pool,
    /// Minute bars, oldest first, as far back as the state keeps them.
    pub bars: VecDeque<Bar>,
}

/// Token1 per token0 in smallest units, from a replica's price.
pub fn raw_price(pool: &Pool) -> f64 {
    let sqrt = pool
        .sqrt_price_x96()
        .to_string()
        .parse::<f64>()
        .unwrap_or(f64::NAN)
        / 2f64.powi(96);
    sqrt * sqrt
}

fn to_f64(value: I256) -> f64 {
    value.to_string().parse().unwrap_or(0.0)
}

/// Sums of the bars over a stretch of time.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Flow {
    pub volume0: f64,
    pub volume1: f64,
    pub fees0: f64,
    pub fees1: f64,
    pub swaps: u32,
}

/// What changed since the writer last looked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Outbox {
    pub swaps: Vec<SwapRecord>,
    /// Bars that changed, by pool and minute.
    pub bars: BTreeSet<(Address, u64)>,
    /// Pools with any event.
    pub pools: BTreeSet<Address>,
}

/// The followed pools and their positions at one block.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveState {
    pub pools: BTreeMap<Address, LivePool>,
    pub book: PositionBook,
    /// The last block applied.
    pub at: Stamp,
    /// When the book started following positions.
    pub since: Stamp,
    /// Seconds of minute bars kept in memory.
    pub keep_bars: u64,
    /// Swaps and bars of blocks before this time are not recorded.
    pub record_from: u64,
    record: bool,
    out: Outbox,
}

impl LiveState {
    /// A state of pools at the snapshots given: each the state after the
    /// snapshot's block.
    pub fn from_snapshots(snapshots: &[StoredSnapshot], keep_bars: u64) -> Self {
        let pools: BTreeMap<Address, LivePool> = snapshots
            .iter()
            .map(|s| {
                (
                    s.pool,
                    LivePool {
                        address: s.pool,
                        token0: s.token0,
                        token1: s.token1,
                        pool: Pool::from_snapshot(s.state.clone()),
                        bars: VecDeque::new(),
                    },
                )
            })
            .collect();
        let at = snapshots
            .iter()
            .map(|s| Stamp {
                block: s.block,
                time: s.day.start(),
            })
            .min()
            .unwrap_or_default();
        Self {
            book: PositionBook::new(pools.keys().copied()),
            pools,
            at,
            since: at,
            keep_bars,
            record_from: 0,
            record: false,
            out: Outbox::default(),
        }
    }

    /// Whether swaps, bars and position changes are collected for the
    /// writer.
    pub fn set_record(&mut self, record: bool) {
        self.record = record;
        self.book.record = record;
    }

    pub fn records(&self) -> bool {
        self.record
    }

    /// The addresses whose logs the state needs: its pools and the
    /// position manager.
    pub fn addresses(&self) -> Vec<Address> {
        let mut addresses: Vec<Address> = self.pools.keys().copied().collect();
        addresses.push(MANAGER);
        addresses
    }

    /// Whether both ticks of a range are initialized in its pool's replica.
    pub fn initialized(&self, range: &Range) -> bool {
        self.pools.get(&range.pool).is_some_and(|p| {
            p.pool.tick_info(range.tick_lower).is_some()
                && p.pool.tick_info(range.tick_upper).is_some()
        })
    }

    /// The positions in `logs` the book has not met, to read from the chain.
    pub fn wanted(&self, logs: &[ChainLog]) -> Vec<Wanted> {
        self.book.wanted(logs, &|range| self.initialized(range))
    }

    /// Applies logs of whole blocks, in chain order, up to and including
    /// block `through`. `seeds` are the chain's answers for what
    /// [`wanted`](LiveState::wanted) listed.
    pub fn apply(
        &mut self,
        logs: &[ChainLog],
        through: Stamp,
        seeds: Vec<Seed>,
    ) -> Result<(), LiveError> {
        self.book.stage(seeds);
        let result = logs.iter().try_for_each(|log| self.apply_log(log));
        self.book.clear_staged();
        result?;
        if through.block >= self.at.block {
            self.at = through;
        }
        self.trim_bars();
        Ok(())
    }

    fn apply_log(&mut self, log: &ChainLog) -> Result<(), LiveError> {
        let replay_error = |reason: String| LiveError::Replay {
            pool: log.address,
            block: log.block,
            log_index: log.log_index,
            reason,
        };
        if log.address == MANAGER {
            if let Some(event) = decode_manager(&log.topics, &log.data).map_err(replay_error)? {
                self.book.manager_event(&event, log.stamp(), log.tx_hash);
            }
            return Ok(());
        }
        let Some(lp) = self.pools.get_mut(&log.address) else {
            return Ok(());
        };
        let before = raw_price(&lp.pool);
        let event = apply_pool_log(&mut self.book, &mut lp.pool, log).map_err(replay_error)?;
        if !self.record || log.time < self.record_from {
            return Ok(());
        }
        let Some(event) = event else {
            return Ok(());
        };
        let after = raw_price(&lp.pool);
        let minute = log.time - log.time % 60;
        if lp.bars.back().is_none_or(|bar| bar.minute < minute) {
            lp.bars.push_back(Bar {
                minute,
                open: before,
                high: before,
                low: before,
                ..Bar::default()
            });
        }
        let bar = lp.bars.back_mut().expect("just pushed");
        bar.block = log.block;
        bar.close = after;
        bar.high = bar.high.max(after);
        bar.low = bar.low.min(after);
        bar.tick = lp.pool.tick();
        bar.liquidity = lp.pool.liquidity() as f64;
        match event {
            PoolEvent::Swap(swap) => {
                let (a0, a1) = (to_f64(swap.amount0), to_f64(swap.amount1));
                let fee = f64::from(lp.pool.fee_pips()) / 1_000_000.0;
                bar.volume0 += a0.abs();
                bar.volume1 += a1.abs();
                // The token paid in is the one the pool received.
                if a0 > 0.0 {
                    bar.fees0 += a0 * fee;
                } else if a1 > 0.0 {
                    bar.fees1 += a1 * fee;
                }
                bar.swaps += 1;
                let word = |i: usize| log.topics.get(i).map(|t| Address::from_word(*t));
                self.out.swaps.push(SwapRecord {
                    pool: log.address,
                    block: log.block,
                    time: log.time,
                    log_index: log.log_index,
                    tx_hash: log.tx_hash,
                    sender: word(1).unwrap_or_default(),
                    recipient: word(2).unwrap_or_default(),
                    amount0: a0,
                    amount1: a1,
                    price: after,
                    tick: swap.tick,
                    liquidity: swap.liquidity as f64,
                });
            }
            PoolEvent::Mint { .. } => bar.mints += 1,
            PoolEvent::Burn { .. } => bar.burns += 1,
            _ => {}
        }
        self.out.bars.insert((log.address, minute));
        self.out.pools.insert(log.address);
        Ok(())
    }

    fn trim_bars(&mut self) {
        let oldest = self.at.time.saturating_sub(self.keep_bars);
        for lp in self.pools.values_mut() {
            while lp.bars.front().is_some_and(|bar| bar.minute + 60 <= oldest) {
                lp.bars.pop_front();
            }
        }
    }

    /// What the writer has not seen yet.
    pub fn take_outbox(&mut self) -> Outbox {
        std::mem::take(&mut self.out)
    }

    /// One pool's bar for a minute, if kept.
    pub fn bar(&self, pool: Address, minute: u64) -> Option<&Bar> {
        let bars = &self.pools.get(&pool)?.bars;
        let at = bars.partition_point(|bar| bar.minute < minute);
        bars.get(at).filter(|bar| bar.minute == minute)
    }

    /// A pool's volume, fees and swaps in the bars from `from` (Unix
    /// seconds) on.
    pub fn flow(&self, pool: Address, from: u64) -> Flow {
        let mut flow = Flow::default();
        if let Some(lp) = self.pools.get(&pool) {
            for bar in lp.bars.iter().filter(|bar| bar.minute + 60 > from) {
                flow.volume0 += bar.volume0;
                flow.volume1 += bar.volume1;
                flow.fees0 += bar.fees0;
                flow.fees1 += bar.fees1;
                flow.swaps += bar.swaps;
            }
        }
        flow
    }
}

// --- reading from the node ------------------------------------------------

/// Where a follower reads the chain: the node, or a scripted chain in
/// tests.
pub trait HeadSource {
    /// The newest block's header.
    fn latest(&self) -> Result<BlockHeader, ChainError>;
    /// The newest finalized block's number, if the node knows one.
    fn finalized(&self) -> Result<Option<u64>, ChainError>;
    /// Headers of blocks, in the order asked.
    fn headers(&self, numbers: &[u64]) -> Result<Vec<BlockHeader>, ChainError>;
    /// Every log of the addresses in a block range, in chain order, with
    /// block hashes.
    fn logs(&self, addresses: &[Address], from: u64, to: u64)
        -> Result<Vec<HashedLog>, ChainError>;
    /// What the chain holds for the positions wanted, in the order asked.
    fn seeds(&self, wanted: &[Wanted]) -> Result<Vec<Seed>, ChainError>;
}

/// The local node as a [`HeadSource`]. Position reads are split over
/// `threads` connections at once.
pub struct NodeHead<'a> {
    pub client: &'a RpcClient,
    pub threads: usize,
}

impl HeadSource for NodeHead<'_> {
    fn latest(&self) -> Result<BlockHeader, ChainError> {
        Ok(self.client.latest_header()?)
    }

    fn finalized(&self) -> Result<Option<u64>, ChainError> {
        Ok(self.client.finalized_block()?.map(|(number, _)| number))
    }

    fn headers(&self, numbers: &[u64]) -> Result<Vec<BlockHeader>, ChainError> {
        Ok(self.client.block_headers(numbers)?)
    }

    fn logs(
        &self,
        addresses: &[Address],
        from: u64,
        to: u64,
    ) -> Result<Vec<HashedLog>, ChainError> {
        Ok(self.client.get_logs_hashed(addresses, &[], from, to)?)
    }

    fn seeds(&self, wanted: &[Wanted]) -> Result<Vec<Seed>, ChainError> {
        let parts = self.threads.max(1);
        let size = wanted.len().div_ceil(parts).max(1);
        let answers: Vec<Result<Vec<Seed>, ChainError>> = std::thread::scope(|scope| {
            let handles: Vec<_> = wanted
                .chunks(size)
                .map(|chunk| scope.spawn(move || read_seeds(self.client, chunk)))
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(ChainError::Other("a read panicked".into())))
                })
                .collect()
        });
        let mut seeds = Vec::with_capacity(wanted.len());
        for part in answers {
            seeds.extend(part?);
        }
        Ok(seeds)
    }
}

/// Reads what the chain held for each position wanted.
pub fn read_seeds(client: &RpcClient, wanted: &[Wanted]) -> Result<Vec<Seed>, ChainError> {
    let pool_reads: Vec<_> = wanted
        .iter()
        .filter_map(|w| match w {
            Wanted::Pool { key, block } => Some((
                key.range.pool,
                key.owner,
                key.range.tick_lower,
                key.range.tick_upper,
                *block,
            )),
            Wanted::Nft { .. } => None,
        })
        .collect();
    let nft_reads: Vec<(u64, u64)> = wanted
        .iter()
        .filter_map(|w| match w {
            Wanted::Nft { token, block, .. } => Some((*token, *block)),
            Wanted::Pool { .. } => None,
        })
        .collect();
    let mut pools = read_positions_at(client, &pool_reads)?.into_iter();
    let mut nfts = read_nfts_at(client, MANAGER, &nft_reads)?.into_iter();
    let mut seeds = Vec::with_capacity(wanted.len());
    for w in wanted {
        seeds.push(match w {
            Wanted::Pool { key, .. } => {
                let info = pools.next().ok_or_else(|| short("pool positions"))?;
                Seed::Pool {
                    key: *key,
                    liquidity: info.liquidity,
                    last_inside: (info.fee_growth_inside0_last, info.fee_growth_inside1_last),
                    owed: (info.tokens_owed0, info.tokens_owed1),
                }
            }
            Wanted::Nft { token, range, .. } => {
                let read = nfts.next().ok_or_else(|| short("NFTs"))?;
                Seed::Nft {
                    token: *token,
                    state: read.map(|(info, holder)| NftSeed {
                        range: Range {
                            pool: range.pool,
                            tick_lower: info.tick_lower,
                            tick_upper: info.tick_upper,
                        },
                        holder,
                        liquidity: info.liquidity,
                        last_inside: (info.fee_growth_inside0_last, info.fee_growth_inside1_last),
                        owed: (info.tokens_owed0, info.tokens_owed1),
                    }),
                }
            }
        });
    }
    Ok(seeds)
}

fn short(what: &str) -> ChainError {
    ChainError::Other(format!("the node answered fewer {what} than asked"))
}

// --- following the head ---------------------------------------------------

/// A block past the finalized one, kept until it is finalized.
#[derive(Debug, Clone, PartialEq)]
pub struct TailBlock {
    pub header: BlockHeader,
    pub logs: Vec<ChainLog>,
    /// The chain's answers for positions first met in this block.
    pub seeds: Vec<Seed>,
}

/// What a step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing new.
    Idle,
    /// Applied blocks `from` to `to`.
    Advanced { from: u64, to: u64 },
    /// The block the live state stood on is no longer the chain's. The
    /// live state was rebuilt up to `fork`, the last block both chains
    /// share; `dropped` blocks were taken back.
    Reorg { fork: u64, dropped: u64 },
    /// The node's answers did not fit together, most likely because the
    /// chain moved while they were read. Nothing was applied.
    Retry(String),
}

/// Keeps a [`LiveState`] at the node's head.
pub struct Follower {
    /// The state at the newest finalized block applied.
    pub base: LiveState,
    /// The hash of the base state's block, when known.
    pub base_hash: Option<B256>,
    /// The state at the head.
    pub live: LiveState,
    tail: VecDeque<TailBlock>,
    addresses: Vec<Address>,
    /// The most blocks one step applies, so that catching up on a long
    /// gap goes in steps the writer can follow.
    pub max_blocks: u64,
    /// The node's head and finalized block at the last step.
    pub head: u64,
    pub finalized: u64,
    pub reorgs: u64,
}

/// Blocks behind the head taken as final when the node does not say:
/// two epochs.
const FINALITY_FALLBACK: u64 = 64;

impl Follower {
    /// Follows on from `state`, which must be at a finalized block.
    pub fn new(state: LiveState, hash: Option<B256>) -> Self {
        let mut base = state;
        base.set_record(false);
        base.take_outbox();
        let mut live = base.clone();
        live.set_record(true);
        Self {
            addresses: base.addresses(),
            base,
            base_hash: hash,
            live,
            tail: VecDeque::new(),
            max_blocks: 500,
            head: 0,
            finalized: 0,
            reorgs: 0,
        }
    }

    /// Blocks applied past the finalized one.
    pub fn tail_len(&self) -> usize {
        self.tail.len()
    }

    /// Reads what is new at the node and applies it.
    pub fn step(&mut self, source: &impl HeadSource) -> Result<Step, LiveError> {
        let latest = source.latest()?;
        let finalized = source
            .finalized()?
            .unwrap_or_else(|| latest.number.saturating_sub(FINALITY_FALLBACK))
            .min(latest.number);
        self.head = latest.number;
        self.finalized = finalized;

        // Is the block the live state stands on still the chain's?
        if let Some(tip) = self.tail.back() {
            let still = tip.header.number <= latest.number
                && source
                    .headers(&[tip.header.number])?
                    .first()
                    .map(|h| h.hash)
                    == Some(tip.header.hash);
            if !still {
                return self.rewind(source, latest.number);
            }
        }

        let from = self.live.at.block + 1;
        if from > latest.number {
            self.finalize(finalized)?;
            return Ok(Step::Idle);
        }
        let to = latest.number.min(from + self.max_blocks.max(1) - 1);
        let numbers: Vec<u64> = (from..=to).collect();
        let headers = source.headers(&numbers)?;
        let mut parent = self.tail.back().map(|t| t.header.hash).or(self.base_hash);
        for header in &headers {
            if parent.is_some_and(|p| p != header.parent_hash) {
                return Ok(Step::Retry(format!(
                    "block {} does not follow the block before it",
                    header.number
                )));
            }
            parent = Some(header.hash);
        }
        let by_number: HashMap<u64, &BlockHeader> = headers.iter().map(|h| (h.number, h)).collect();
        let mut logs = Vec::new();
        for hashed in source.logs(&self.addresses, from, to)? {
            let log = hashed.log;
            let Some(header) = by_number.get(&log.block_number) else {
                return Ok(Step::Retry(format!(
                    "a log of block {} outside the range asked",
                    log.block_number
                )));
            };
            if header.hash != hashed.block_hash {
                return Ok(Step::Retry(format!(
                    "the logs of block {} are of another block",
                    log.block_number
                )));
            }
            logs.push(ChainLog {
                address: log.address,
                block: log.block_number,
                time: header.time,
                log_index: u32::try_from(log.log_index).unwrap_or(u32::MAX),
                tx_hash: log.transaction_hash,
                topics: log.topics,
                data: log.data,
            });
        }
        let wanted = self.live.wanted(&logs);
        let seeds = if wanted.is_empty() {
            Vec::new()
        } else {
            source.seeds(&wanted)?
        };
        let last = headers.last().expect("at least one block");
        self.live.apply(
            &logs,
            Stamp {
                block: last.number,
                time: last.time,
            },
            seeds.clone(),
        )?;

        // Keep each block, with its logs and seeds, until it is final.
        let mut by_block: BTreeMap<u64, (Vec<ChainLog>, Vec<Seed>)> = BTreeMap::new();
        for log in logs {
            by_block.entry(log.block).or_default().0.push(log);
        }
        for (w, seed) in wanted.iter().zip(seeds) {
            let block = match w {
                Wanted::Pool { block, .. } | Wanted::Nft { block, .. } => block + 1,
            };
            by_block.entry(block).or_default().1.push(seed);
        }
        for header in headers {
            let (logs, seeds) = by_block.remove(&header.number).unwrap_or_default();
            self.tail.push_back(TailBlock {
                header,
                logs,
                seeds,
            });
        }
        self.finalize(finalized)?;
        Ok(Step::Advanced { from, to })
    }

    /// Moves the blocks that are final now from the tail into the base.
    fn finalize(&mut self, finalized: u64) -> Result<(), LiveError> {
        while self
            .tail
            .front()
            .is_some_and(|b| b.header.number <= finalized)
        {
            let block = self.tail.pop_front().expect("checked");
            self.base.apply(
                &block.logs,
                Stamp {
                    block: block.header.number,
                    time: block.header.time,
                },
                block.seeds,
            )?;
            self.base_hash = Some(block.header.hash);
        }
        Ok(())
    }

    /// Rebuilds the live state from the base and the tail blocks the
    /// chain still has.
    fn rewind(&mut self, source: &impl HeadSource, latest: u64) -> Result<Step, LiveError> {
        let numbers: Vec<u64> = self
            .tail
            .iter()
            .map(|b| b.header.number)
            .filter(|n| *n <= latest)
            .collect();
        let now: HashMap<u64, B256> = source
            .headers(&numbers)?
            .into_iter()
            .map(|h| (h.number, h.hash))
            .collect();
        let keep = self
            .tail
            .iter()
            .take_while(|b| now.get(&b.header.number) == Some(&b.header.hash))
            .count();
        if keep == 0 {
            if let Some(hash) = self.base_hash {
                let base = self.base.at.block;
                if source.headers(&[base])?.first().map(|h| h.hash) != Some(hash) {
                    return Err(LiveError::DeepReorg { block: base });
                }
            }
        }
        let tip = self.live.at.block;
        self.tail.truncate(keep);
        let mut live = self.base.clone();
        live.set_record(true);
        for block in &self.tail {
            live.apply(
                &block.logs,
                Stamp {
                    block: block.header.number,
                    time: block.header.time,
                },
                block.seeds.clone(),
            )?;
        }
        self.live = live;
        self.reorgs += 1;
        let fork = self.live.at.block;
        Ok(Step::Reorg {
            fork,
            dropped: tip.saturating_sub(fork),
        })
    }
}

// --- the stored history ---------------------------------------------------

/// Replays the stored history into a live state, day by day: every pool
/// from its snapshot, the position manager alongside, positions met for
/// the first time read through `source`. `after_day` sees the state at
/// the end of each day, to write what changed. Returns the state after
/// the last log of day `to`.
pub fn replay_history(
    lake: &Path,
    chain: &str,
    snapshots: &[StoredSnapshot],
    to: Day,
    state: LiveState,
    source: &impl HeadSource,
    mut after_day: impl FnMut(Day, &mut LiveState) -> Result<(), LiveError>,
) -> Result<LiveState, LiveError> {
    let mut state = state;
    let Some(first) = snapshots.iter().map(|s| s.day).min() else {
        return Ok(state);
    };
    let mut day = first;
    while day <= to {
        let pools: Vec<Address> = snapshots
            .iter()
            .filter(|s| s.day <= day)
            .map(|s| s.pool)
            .collect();
        let logs = read_merged_day(lake, chain, &pools, day, true)?;
        let wanted = state.wanted(&logs);
        let seeds = if wanted.is_empty() {
            Vec::new()
        } else {
            source.seeds(&wanted)?
        };
        let through = logs.last().map_or(state.at, |l| l.stamp());
        state.apply(&logs, through, seeds)?;
        after_day(day, &mut state)?;
        day = day.next();
    }
    Ok(state)
}

// --- checkpoints ----------------------------------------------------------

/// The base state on disk, to carry on from after a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    chain: String,
    at: Stamp,
    hash: Option<B256>,
    since: Stamp,
    keep_bars: u64,
    pools: Vec<PoolCheckpoint>,
    book: BookState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PoolCheckpoint {
    snapshot: serde_json::Value,
    bars: Vec<Bar>,
}

const CHECKPOINT_VERSION: u32 = 1;

/// Writes a state, at a finalized block with hash `hash`, as a checkpoint.
/// The file appears under its name only once it is whole on the disk.
pub fn save_checkpoint(
    path: &Path,
    chain: &str,
    state: &LiveState,
    hash: Option<B256>,
) -> Result<(), LiveError> {
    let pools = state
        .pools
        .values()
        .map(|lp| PoolCheckpoint {
            snapshot: to_json(&StoredSnapshot {
                pool: lp.address,
                token0: lp.token0,
                token1: lp.token1,
                block: state.at.block,
                day: Day::of_time(state.at.time),
                state: lp.pool.snapshot(),
            }),
            bars: lp.bars.iter().cloned().collect(),
        })
        .collect();
    let checkpoint = Checkpoint {
        version: CHECKPOINT_VERSION,
        chain: chain.to_string(),
        at: state.at,
        hash,
        since: state.since,
        keep_bars: state.keep_bars,
        pools,
        book: state.book.state(),
    };
    let text = serde_json::to_vec(&checkpoint).map_err(|e| LiveError::Other(e.to_string()))?;
    write_durably(path, &text).map_err(|source| {
        LiveError::Store(StoreError::Io {
            path: path.to_path_buf(),
            source,
        })
    })
}

/// Reads a checkpoint: the state and its block's hash. `None` when there
/// is none.
pub fn load_checkpoint(
    path: &Path,
    chain: &str,
) -> Result<Option<(LiveState, Option<B256>)>, LiveError> {
    if !path.is_file() {
        return Ok(None);
    }
    let bad = |reason: String| LiveError::Checkpoint {
        path: path.display().to_string(),
        reason,
    };
    let text = std::fs::read(path).map_err(|e| bad(e.to_string()))?;
    let checkpoint: Checkpoint = serde_json::from_slice(&text).map_err(|e| bad(e.to_string()))?;
    if checkpoint.version != CHECKPOINT_VERSION {
        return Err(bad(format!(
            "it is of version {}, this program reads {CHECKPOINT_VERSION}",
            checkpoint.version
        )));
    }
    if checkpoint.chain != chain {
        return Err(bad(format!("it is of chain `{}`", checkpoint.chain)));
    }
    let mut pools = BTreeMap::new();
    for pool in checkpoint.pools {
        let snapshot = from_json(&pool.snapshot).map_err(bad)?;
        pools.insert(
            snapshot.pool,
            LivePool {
                address: snapshot.pool,
                token0: snapshot.token0,
                token1: snapshot.token1,
                pool: Pool::from_snapshot(snapshot.state),
                bars: pool.bars.into(),
            },
        );
    }
    let state = LiveState {
        pools,
        book: PositionBook::from_state(checkpoint.book),
        at: checkpoint.at,
        since: checkpoint.since,
        keep_bars: checkpoint.keep_bars,
        record_from: 0,
        record: false,
        out: Outbox::default(),
    };
    Ok(Some((state, checkpoint.hash)))
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
