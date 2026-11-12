//! Reading Uniswap v3 pools from a node: state snapshots, events, discovery,
//! and the check that the replica agrees with the chain.

use alloy_primitives::{keccak256, Address, I256, U256};
use lpbot_venues::uniswap_v3::events::{decode_log, topics, PoolEvent};
use lpbot_venues::uniswap_v3::pool::{Pool, PoolSnapshot, Replay, TickInfo};
use lpbot_venues::uniswap_v3::tick_math::MAX_TICK;
use thiserror::Error;

use crate::eth::Log;
use crate::rpc::{RpcClient, RpcError};

/// How many `eth_call`s go into one HTTP request.
const CALL_BATCH: usize = 200;

#[derive(Debug, Error)]
pub enum ChainError {
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("the node returned {got} bytes for `{what}`, expected at least {expected}")]
    ShortReturn {
        what: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("`{what}` returned a value that does not fit its type")]
    ValueOutOfRange { what: &'static str },
    #[error("cannot decode a log in block {block}, index {log_index}: {reason}")]
    BadLog {
        block: u64,
        log_index: u64,
        reason: String,
    },
    #[error("block {block}, log {log_index}: {reason}")]
    Replay {
        block: u64,
        log_index: u64,
        reason: String,
    },
    #[error("{0}")]
    Other(String),
}

pub type ChainResult<T> = Result<T, ChainError>;

/// A pool event with its position in the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolLog {
    pub block_number: u64,
    pub log_index: u64,
    pub event: PoolEvent,
}

// --- ABI helpers --------------------------------------------------------

fn selector(signature: &str) -> [u8; 4] {
    let hash = keccak256(signature);
    [hash[0], hash[1], hash[2], hash[3]]
}

fn call_data(signature: &str, args: &[U256]) -> Vec<u8> {
    let mut data = selector(signature).to_vec();
    for arg in args {
        data.extend_from_slice(&arg.to_be_bytes::<32>());
    }
    data
}

fn int_arg(value: i32) -> U256 {
    I256::try_from(value).expect("fits").into_raw()
}

fn address_arg(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn word(what: &'static str, data: &[u8], index: usize) -> ChainResult<U256> {
    let end = (index + 1) * 32;
    if data.len() < end {
        return Err(ChainError::ShortReturn {
            what,
            expected: end,
            got: data.len(),
        });
    }
    Ok(U256::from_be_slice(&data[index * 32..end]))
}

fn as_i32(what: &'static str, value: U256) -> ChainResult<i32> {
    i32::try_from(I256::from_raw(value)).map_err(|_| ChainError::ValueOutOfRange { what })
}

fn as_u128(what: &'static str, value: U256) -> ChainResult<u128> {
    u128::try_from(value).map_err(|_| ChainError::ValueOutOfRange { what })
}

fn as_i128(what: &'static str, value: U256) -> ChainResult<i128> {
    i128::try_from(I256::from_raw(value)).map_err(|_| ChainError::ValueOutOfRange { what })
}

fn as_address(value: U256) -> Address {
    Address::from_slice(&value.to_be_bytes::<32>()[12..])
}

// --- snapshots ----------------------------------------------------------

/// Reads a pool's full state at one block: price, liquidity, fee growth and
/// every initialized tick.
///
/// The block must be one the node still has state for. An archive node has
/// all of them; a full node only the recent ones.
pub fn fetch_snapshot(client: &RpcClient, pool: Address, block: u64) -> ChainResult<PoolSnapshot> {
    let head = client.eth_call_many(
        &[
            (pool, call_data("fee()", &[])),
            (pool, call_data("tickSpacing()", &[])),
            (pool, call_data("slot0()", &[])),
            (pool, call_data("liquidity()", &[])),
            (pool, call_data("feeGrowthGlobal0X128()", &[])),
            (pool, call_data("feeGrowthGlobal1X128()", &[])),
        ],
        block,
        CALL_BATCH,
    )?;
    let fee_pips = u32::try_from(word("fee", &head[0], 0)?)
        .map_err(|_| ChainError::ValueOutOfRange { what: "fee" })?;
    let tick_spacing = as_i32("tickSpacing", word("tickSpacing", &head[1], 0)?)?;
    if tick_spacing <= 0 {
        return Err(ChainError::ValueOutOfRange {
            what: "tickSpacing",
        });
    }
    let sqrt_price_x96 = word("slot0", &head[2], 0)?;
    let tick = as_i32("slot0", word("slot0", &head[2], 1)?)?;
    // feeProtocol packs token0's share in the low four bits and token1's in the high four.
    let fee_protocol = u8::try_from(word("slot0", &head[2], 5)?)
        .map_err(|_| ChainError::ValueOutOfRange { what: "slot0" })?;
    let liquidity = as_u128("liquidity", word("liquidity", &head[3], 0)?)?;
    let fee_growth_global0 = word("feeGrowthGlobal0X128", &head[4], 0)?;
    let fee_growth_global1 = word("feeGrowthGlobal1X128", &head[5], 0)?;

    // The tick bitmap says which ticks are initialized, 256 spacings per word.
    let max_compressed = MAX_TICK / tick_spacing;
    let first_word = (-max_compressed).div_euclid(256);
    let last_word = max_compressed.div_euclid(256);
    let word_calls: Vec<(Address, Vec<u8>)> = (first_word..=last_word)
        .map(|w| (pool, call_data("tickBitmap(int16)", &[int_arg(w)])))
        .collect();
    let words = client.eth_call_many(&word_calls, block, CALL_BATCH)?;

    let mut initialized = Vec::new();
    for (word_pos, data) in (first_word..=last_word).zip(&words) {
        let bits = word("tickBitmap", data, 0)?;
        if bits.is_zero() {
            continue;
        }
        for bit in 0..256usize {
            if bits.bit(bit) {
                initialized.push((word_pos * 256 + bit as i32) * tick_spacing);
            }
        }
    }

    let tick_calls: Vec<(Address, Vec<u8>)> = initialized
        .iter()
        .map(|t| (pool, call_data("ticks(int24)", &[int_arg(*t)])))
        .collect();
    let tick_data = client.eth_call_many(&tick_calls, block, CALL_BATCH)?;
    let mut ticks = Vec::with_capacity(initialized.len());
    for (tick_index, data) in initialized.iter().zip(&tick_data) {
        ticks.push((
            *tick_index,
            TickInfo {
                liquidity_gross: as_u128("ticks", word("ticks", data, 0)?)?,
                liquidity_net: as_i128("ticks", word("ticks", data, 1)?)?,
                fee_growth_outside0: word("ticks", data, 2)?,
                fee_growth_outside1: word("ticks", data, 3)?,
                watchers: 0,
            },
        ));
    }

    Ok(PoolSnapshot {
        fee_pips,
        tick_spacing,
        sqrt_price_x96,
        tick,
        liquidity,
        fee_growth_global0,
        fee_growth_global1,
        fee_protocol0: fee_protocol % 16,
        fee_protocol1: fee_protocol >> 4,
        ticks,
    })
}

// --- events -------------------------------------------------------------

/// Turns raw logs into pool events, dropping the ones that do not change state.
pub fn decode_pool_logs(logs: &[Log]) -> ChainResult<Vec<PoolLog>> {
    let mut out = Vec::with_capacity(logs.len());
    for log in logs {
        match decode_log(&log.topics, &log.data) {
            Ok(Some(event)) => out.push(PoolLog {
                block_number: log.block_number,
                log_index: log.log_index,
                event,
            }),
            Ok(None) => {}
            Err(e) => {
                return Err(ChainError::BadLog {
                    block: log.block_number,
                    log_index: log.log_index,
                    reason: e.to_string(),
                })
            }
        }
    }
    Ok(out)
}

/// Fetches a pool's state-changing events in a block range, in chain order.
pub fn fetch_events(
    client: &RpcClient,
    pool: Address,
    from_block: u64,
    to_block: u64,
    chunk: u64,
    on_progress: impl FnMut(u64, usize),
) -> ChainResult<Vec<PoolLog>> {
    let logs = client.get_logs_chunked(
        pool,
        &topics::all(),
        from_block,
        to_block,
        chunk,
        on_progress,
    )?;
    decode_pool_logs(&logs)
}

// --- discovery ----------------------------------------------------------

/// Fee tiers the Uniswap v3 factory has enabled, in hundredths of a basis point.
pub const FEE_TIERS: [u32; 4] = [100, 500, 3_000, 10_000];

/// A pool found through the factory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveredPool {
    pub address: Address,
    /// The pool's token0 and token1, in the pool's own order.
    pub token0: Address,
    pub token1: Address,
    pub fee_pips: u32,
}

/// Asks the factory for the pool of every pair of `tokens` at every fee tier.
pub fn discover_pools(
    client: &RpcClient,
    factory: Address,
    tokens: &[Address],
    block: u64,
) -> ChainResult<Vec<DiscoveredPool>> {
    let mut queries = Vec::new();
    for (i, a) in tokens.iter().enumerate() {
        for b in &tokens[i + 1..] {
            // Uniswap orders a pool's tokens by address.
            let (token0, token1) = if a < b { (*a, *b) } else { (*b, *a) };
            for fee in FEE_TIERS {
                queries.push((token0, token1, fee));
            }
        }
    }
    let calls: Vec<(Address, Vec<u8>)> = queries
        .iter()
        .map(|(token0, token1, fee)| {
            (
                factory,
                call_data(
                    "getPool(address,address,uint24)",
                    &[address_arg(*token0), address_arg(*token1), U256::from(*fee)],
                ),
            )
        })
        .collect();
    let replies = client.eth_call_many(&calls, block, CALL_BATCH)?;

    let mut pools = Vec::new();
    for ((token0, token1, fee_pips), data) in queries.into_iter().zip(&replies) {
        let address = as_address(word("getPool", data, 0)?);
        if address != Address::ZERO {
            pools.push(DiscoveredPool {
                address,
                token0,
                token1,
                fee_pips,
            });
        }
    }
    Ok(pools)
}

// --- verification -------------------------------------------------------

/// Outcome of checking the replica against the chain over a block range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub from_block: u64,
    pub to_block: u64,
    pub events: usize,
    pub swaps: usize,
    pub mints: usize,
    pub burns: usize,
    pub ticks_at_end: usize,
    /// Empty when the replica matches the chain exactly.
    pub differences: Vec<String>,
}

impl VerifyReport {
    pub fn passed(&self) -> bool {
        self.differences.is_empty()
    }
}

/// Checks the replica against a deployed pool.
///
/// It reads the pool's state at `from_block`, replays every event up to
/// `to_block` in strict mode, and compares the result with the pool's state
/// read at `to_block`. A strict replay already fails on the first event the
/// math cannot reproduce; the final comparison also covers fee growth and
/// every tick.
pub fn verify_pool(
    client: &RpcClient,
    pool_address: Address,
    from_block: u64,
    to_block: u64,
    log_chunk: u64,
    mut on_progress: impl FnMut(&str),
) -> ChainResult<VerifyReport> {
    if from_block >= to_block {
        return Err(ChainError::Other(format!(
            "from block {from_block} must be before to block {to_block}"
        )));
    }

    on_progress(&format!("reading pool state at block {from_block}"));
    let mut pool = Pool::from_snapshot(fetch_snapshot(client, pool_address, from_block)?);

    on_progress(&format!(
        "fetching events for blocks {} to {to_block}",
        from_block + 1
    ));
    let events = fetch_events(
        client,
        pool_address,
        from_block + 1,
        to_block,
        log_chunk,
        |_, _| {},
    )?;

    on_progress(&format!("replaying {} events", events.len()));
    let (mut swaps, mut mints, mut burns) = (0, 0, 0);
    for log in &events {
        match log.event {
            PoolEvent::Swap(_) => swaps += 1,
            PoolEvent::Mint { .. } => mints += 1,
            PoolEvent::Burn { .. } => burns += 1,
            _ => {}
        }
        pool.apply_event(&log.event, Replay::Strict, &mut ())
            .map_err(|e| ChainError::Replay {
                block: log.block_number,
                log_index: log.log_index,
                reason: e.to_string(),
            })?;
    }

    on_progress(&format!("reading pool state at block {to_block}"));
    let on_chain = Pool::from_snapshot(fetch_snapshot(client, pool_address, to_block)?);

    Ok(VerifyReport {
        from_block,
        to_block,
        events: events.len(),
        swaps,
        mints,
        burns,
        ticks_at_end: on_chain.ticks().count(),
        differences: pool.differences(&on_chain),
    })
}

#[cfg(test)]
#[path = "uniswap_v3_tests.rs"]
mod tests;
