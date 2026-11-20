//! The handful of Ethereum JSON-RPC methods the bot uses.

use alloy_primitives::{hex, Address, B256};
use serde_json::{json, Value};

use crate::rpc::{RpcClient, RpcError, RpcResult};

/// A log as returned by `eth_getLogs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
    pub block_number: u64,
    pub log_index: u64,
    pub transaction_hash: B256,
}

/// A log with the hash of the block it was read from. Two answers for the
/// same block number with different hashes mean the chain reorganized in
/// between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashedLog {
    pub log: Log,
    pub block_hash: B256,
}

/// What places a block in the chain: its number, its hash, its parent's
/// hash and its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    /// Unix time, in seconds.
    pub time: u64,
}

/// How far a node that is still syncing has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncProgress {
    /// The newest block the node has processed.
    pub current: u64,
    /// The newest block the node knows of.
    pub highest: u64,
}

fn bad(method: &str, reason: impl Into<String>) -> RpcError {
    RpcError::BadResponse {
        method: method.to_string(),
        reason: reason.into(),
    }
}

/// Parses a hex quantity such as `"0x1b4"`.
pub fn parse_quantity(method: &str, value: &Value) -> RpcResult<u64> {
    let text = value
        .as_str()
        .ok_or_else(|| bad(method, "expected a hex string"))?;
    let digits = text
        .strip_prefix("0x")
        .ok_or_else(|| bad(method, format!("`{text}` is not a hex quantity")))?;
    u64::from_str_radix(digits, 16)
        .map_err(|_| bad(method, format!("`{text}` is not a hex quantity")))
}

/// Parses hex data such as `"0xdeadbeef"`.
pub fn parse_bytes(method: &str, value: &Value) -> RpcResult<Vec<u8>> {
    let text = value
        .as_str()
        .ok_or_else(|| bad(method, "expected hex data"))?;
    hex::decode(text).map_err(|e| bad(method, format!("bad hex data: {e}")))
}

fn quantity(n: u64) -> String {
    format!("0x{n:x}")
}

fn parse_log(value: &Value) -> RpcResult<Log> {
    const M: &str = "eth_getLogs";
    let field = |name: &str| {
        value
            .get(name)
            .ok_or_else(|| bad(M, format!("log has no `{name}`")))
    };
    let text = |name: &str| -> RpcResult<&str> {
        field(name)?
            .as_str()
            .ok_or_else(|| bad(M, format!("log `{name}` is not a string")))
    };
    if value.get("removed").and_then(Value::as_bool) == Some(true) {
        return Err(bad(M, "the node returned a removed (reorged) log"));
    }
    let topics = field("topics")?
        .as_array()
        .ok_or_else(|| bad(M, "log `topics` is not a list"))?
        .iter()
        .map(|t| {
            t.as_str()
                .and_then(|s| s.parse::<B256>().ok())
                .ok_or_else(|| bad(M, "bad topic"))
        })
        .collect::<RpcResult<Vec<_>>>()?;
    Ok(Log {
        address: text("address")?
            .parse()
            .map_err(|_| bad(M, "bad log address"))?,
        topics,
        data: parse_bytes(M, field("data")?)?,
        block_number: parse_quantity(M, field("blockNumber")?)?,
        log_index: parse_quantity(M, field("logIndex")?)?,
        transaction_hash: text("transactionHash")?
            .parse()
            .map_err(|_| bad(M, "bad transaction hash"))?,
    })
}

fn parse_hashed_log(value: &Value) -> RpcResult<HashedLog> {
    let block_hash = value
        .get("blockHash")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<B256>().ok())
        .ok_or_else(|| bad("eth_getLogs", "log has no `blockHash`"))?;
    Ok(HashedLog {
        log: parse_log(value)?,
        block_hash,
    })
}

fn parse_header(method: &str, block: &Value) -> RpcResult<BlockHeader> {
    let hash = |name: &str| {
        block
            .get(name)
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<B256>().ok())
            .ok_or_else(|| bad(method, format!("block has no `{name}`")))
    };
    Ok(BlockHeader {
        number: parse_quantity(method, block.get("number").unwrap_or(&Value::Null))?,
        hash: hash("hash")?,
        parent_hash: hash("parentHash")?,
        time: parse_quantity(method, block.get("timestamp").unwrap_or(&Value::Null))?,
    })
}

impl RpcClient {
    /// The header of the newest block the node has.
    pub fn latest_header(&self) -> RpcResult<BlockHeader> {
        const M: &str = "eth_getBlockByNumber";
        let block = self.call(M, json!(["latest", false]))?;
        if block.is_null() {
            return Err(bad(M, "the node has no latest block"));
        }
        parse_header(M, &block)
    }

    /// Headers of several blocks, in the order asked, read in batches. A
    /// block the node does not have is an error.
    pub fn block_headers(&self, numbers: &[u64]) -> RpcResult<Vec<BlockHeader>> {
        const M: &str = "eth_getBlockByNumber";
        const PER_REQUEST: usize = 200;
        let mut headers = Vec::with_capacity(numbers.len());
        for group in numbers.chunks(PER_REQUEST) {
            let calls: Vec<(&str, Value)> = group
                .iter()
                .map(|n| (M, json!([quantity(*n), false])))
                .collect();
            for (number, block) in group.iter().zip(self.batch(&calls)?) {
                if block.is_null() {
                    return Err(bad(M, format!("block {number} is not available")));
                }
                let header = parse_header(M, &block)?;
                if header.number != *number {
                    return Err(bad(
                        M,
                        format!("asked for block {number}, got {}", header.number),
                    ));
                }
                headers.push(header);
            }
        }
        Ok(headers)
    }

    /// Like `get_logs_many`, with each log's block hash, so that logs read
    /// near the head can be checked against the headers they belong to.
    pub fn get_logs_hashed(
        &self,
        addresses: &[Address],
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
    ) -> RpcResult<Vec<HashedLog>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let result = self.call(
            "eth_getLogs",
            json!([log_filter(addresses, topic0s, from_block, to_block)]),
        )?;
        let mut logs = result
            .as_array()
            .ok_or_else(|| bad("eth_getLogs", "expected a list of logs"))?
            .iter()
            .map(parse_hashed_log)
            .collect::<RpcResult<Vec<_>>>()?;
        if let Some(stray) = logs.iter().find(|l| !addresses.contains(&l.log.address)) {
            return Err(bad(
                "eth_getLogs",
                format!(
                    "a log of {:#x} in block {}, which was not asked for",
                    stray.log.address, stray.log.block_number
                ),
            ));
        }
        logs.sort_by_key(|l| (l.log.block_number, l.log.log_index));
        Ok(logs)
    }

    /// Number of the latest block.
    pub fn block_number(&self) -> RpcResult<u64> {
        parse_quantity("eth_blockNumber", &self.call("eth_blockNumber", json!([]))?)
    }

    /// Chain id of the endpoint (1 for Ethereum mainnet).
    pub fn chain_id(&self) -> RpcResult<u64> {
        parse_quantity("eth_chainId", &self.call("eth_chainId", json!([]))?)
    }

    /// `None` when the node says it has caught up with the chain, else how
    /// far it has got. A node that reports no block numbers counts as
    /// syncing from zero.
    pub fn syncing(&self) -> RpcResult<Option<SyncProgress>> {
        const M: &str = "eth_syncing";
        let reply = self.call(M, json!([]))?;
        if reply == Value::Bool(false) {
            return Ok(None);
        }
        let block = |name: &str| match reply.get(name) {
            Some(value) => parse_quantity(M, value),
            None => Ok(0),
        };
        Ok(Some(SyncProgress {
            current: block("currentBlock")?,
            highest: block("highestBlock")?,
        }))
    }

    /// Timestamp and hash of a block.
    pub fn block_header(&self, number: u64) -> RpcResult<(u64, B256)> {
        const M: &str = "eth_getBlockByNumber";
        let block = self.call(M, json!([quantity(number), false]))?;
        if block.is_null() {
            return Err(bad(M, format!("block {number} is not available")));
        }
        let timestamp = parse_quantity(M, block.get("timestamp").unwrap_or(&Value::Null))?;
        let hash = block
            .get("hash")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| bad(M, "block has no hash"))?;
        Ok((timestamp, hash))
    }

    /// Number and timestamp of the newest block the consensus layer has
    /// finalized, which no reorg can change. `None` when the node does not
    /// serve the `finalized` tag: it refuses it, or knows no such block
    /// yet (before the merge, or without a consensus client).
    pub fn finalized_block(&self) -> RpcResult<Option<(u64, u64)>> {
        const M: &str = "eth_getBlockByNumber";
        let block = match self.call(M, json!(["finalized", false])) {
            Ok(block) => block,
            Err(RpcError::Rpc { .. }) => return Ok(None),
            Err(other) => return Err(other),
        };
        if block.is_null() {
            return Ok(None);
        }
        let field = |name: &str| parse_quantity(M, block.get(name).unwrap_or(&Value::Null));
        Ok(Some((field("number")?, field("timestamp")?)))
    }

    /// The contract code at an address after a block; empty when no
    /// contract was there yet.
    pub fn code_at(&self, address: Address, block: u64) -> RpcResult<Vec<u8>> {
        const M: &str = "eth_getCode";
        let code = self.call(M, json!([format!("{address:#x}"), quantity(block)]))?;
        parse_bytes(M, &code)
    }

    /// Timestamps of several blocks, in the order asked, read in batches.
    pub fn block_times(&self, numbers: &[u64]) -> RpcResult<Vec<u64>> {
        const M: &str = "eth_getBlockByNumber";
        const PER_REQUEST: usize = 200;
        let mut times = Vec::with_capacity(numbers.len());
        for group in numbers.chunks(PER_REQUEST) {
            let calls: Vec<(&str, Value)> = group
                .iter()
                .map(|n| (M, json!([quantity(*n), false])))
                .collect();
            for (number, block) in group.iter().zip(self.batch(&calls)?) {
                if block.is_null() {
                    return Err(bad(M, format!("block {number} is not available")));
                }
                times.push(parse_quantity(
                    M,
                    block.get("timestamp").unwrap_or(&Value::Null),
                )?);
            }
        }
        Ok(times)
    }

    /// Logs of one contract in a block range, in chain order. `topic0s`
    /// selects the event types; empty means all.
    pub fn get_logs(
        &self,
        address: Address,
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
    ) -> RpcResult<Vec<Log>> {
        self.get_logs_many(&[address], topic0s, from_block, to_block)
    }

    /// Logs of several contracts in a block range, in one request and in
    /// chain order. The node reads each block's receipts once however many
    /// contracts are asked about, so this costs about what one contract
    /// does; asking contract by contract reads every block again for each.
    /// No contracts means no logs: the node would read an empty list as
    /// "every contract". A log of a contract that was not asked for is an
    /// error.
    pub fn get_logs_many(
        &self,
        addresses: &[Address],
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
    ) -> RpcResult<Vec<Log>> {
        self.get_logs_where(addresses, &[topic0s.to_vec()], from_block, to_block)
    }

    /// Logs of several contracts in a block range whose topics match:
    /// `topics[i]` lists the values allowed at position `i`, and an empty
    /// list, or a position past the end, allows any. In chain order, with
    /// the same checks as `get_logs_many`. Filtering on an indexed
    /// argument, such as a token id or an owner, lets the node skip blocks
    /// by their bloom filter.
    pub fn get_logs_where(
        &self,
        addresses: &[Address],
        topics: &[Vec<B256>],
        from_block: u64,
        to_block: u64,
    ) -> RpcResult<Vec<Log>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let filter = log_filter_where(addresses, topics, from_block, to_block);
        let result = self.call("eth_getLogs", json!([filter]))?;
        let mut logs = result
            .as_array()
            .ok_or_else(|| bad("eth_getLogs", "expected a list of logs"))?
            .iter()
            .map(parse_log)
            .collect::<RpcResult<Vec<_>>>()?;
        if let Some(stray) = logs.iter().find(|l| !addresses.contains(&l.address)) {
            return Err(bad(
                "eth_getLogs",
                format!(
                    "a log of {:#x} in block {}, which was not asked for",
                    stray.address, stray.block_number
                ),
            ));
        }
        logs.sort_by_key(|l| (l.block_number, l.log_index));
        Ok(logs)
    }

    /// Like `get_logs` for a long range. It asks in chunks and halves the
    /// chunk whenever the node says a request asked for too much ("too
    /// many results", "range too wide"). A rate limit is waited out by the
    /// client and does not shrink the chunk; any other refusal is returned.
    pub fn get_logs_chunked(
        &self,
        address: Address,
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
        initial_chunk: u64,
        on_progress: impl FnMut(u64, usize),
    ) -> RpcResult<Vec<Log>> {
        self.get_logs_chunked_many(
            &[address],
            topic0s,
            from_block,
            to_block,
            initial_chunk,
            on_progress,
        )
    }

    /// `get_logs_many` for a long range, in chunks as `get_logs_chunked`.
    pub fn get_logs_chunked_many(
        &self,
        addresses: &[Address],
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
        initial_chunk: u64,
        on_progress: impl FnMut(u64, usize),
    ) -> RpcResult<Vec<Log>> {
        self.get_logs_chunked_where(
            addresses,
            &[topic0s.to_vec()],
            from_block,
            to_block,
            initial_chunk,
            on_progress,
        )
    }

    /// `get_logs_where` for a long range, in chunks as `get_logs_chunked`.
    pub fn get_logs_chunked_where(
        &self,
        addresses: &[Address],
        topics: &[Vec<B256>],
        from_block: u64,
        to_block: u64,
        initial_chunk: u64,
        mut on_progress: impl FnMut(u64, usize),
    ) -> RpcResult<Vec<Log>> {
        let mut logs = Vec::new();
        if addresses.is_empty() {
            return Ok(logs);
        }
        let mut chunk = initial_chunk.max(1);
        let mut start = from_block;
        while start <= to_block {
            let end = to_block.min(start.saturating_add(chunk - 1));
            match self.get_logs_where(addresses, topics, start, end) {
                Ok(mut batch) => {
                    logs.append(&mut batch);
                    on_progress(end, logs.len());
                    start = end + 1;
                    // Grow back slowly after a successful request.
                    if chunk < initial_chunk {
                        chunk = (chunk * 2).min(initial_chunk);
                    }
                }
                Err(error) if error.is_range_too_wide() && chunk > 1 => chunk /= 2,
                Err(other) => return Err(other),
            }
        }
        Ok(logs)
    }

    /// Calls a view function at a block and returns the raw return data.
    pub fn eth_call(&self, to: Address, data: &[u8], block: u64) -> RpcResult<Vec<u8>> {
        let result = self.call("eth_call", call_params(to, data, block))?;
        parse_bytes("eth_call", &result)
    }

    /// Many view calls at one block, sent in batches.
    pub fn eth_call_many(
        &self,
        calls: &[(Address, Vec<u8>)],
        block: u64,
        batch_size: usize,
    ) -> RpcResult<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(calls.len());
        for group in calls.chunks(batch_size.max(1)) {
            let batch: Vec<(&str, Value)> = group
                .iter()
                .map(|(to, data)| ("eth_call", call_params(*to, data, block)))
                .collect();
            for result in self.batch(&batch)? {
                out.push(parse_bytes("eth_call", &result)?);
            }
        }
        Ok(out)
    }
}

/// Base fee and median priority fee of each block in a range, from
/// `eth_feeHistory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeeHistory {
    /// The first block the lists cover.
    pub oldest: u64,
    /// Base fee per gas of each block, in wei.
    pub base_fees: Vec<u64>,
    /// The median priority fee per gas paid in each block, in wei; zero
    /// for an empty block.
    pub median_tips: Vec<u64>,
}

impl RpcClient {
    /// The fees of the `count` blocks ending at `newest`. Nodes serve at
    /// most 1,024 blocks per request.
    pub fn fee_history(&self, newest: u64, count: u64) -> RpcResult<FeeHistory> {
        const M: &str = "eth_feeHistory";
        let reply = self.call(M, json!([quantity(count), quantity(newest), [50.0]]))?;
        let oldest = parse_quantity(M, reply.get("oldestBlock").unwrap_or(&Value::Null))?;
        let list = |name: &str| {
            reply
                .get(name)
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| bad(M, format!("no `{name}` in the reply")))
        };
        // The base fees list one more entry: the block after `newest`.
        let mut base_fees = list("baseFeePerGas")?
            .iter()
            .map(|v| parse_quantity(M, v))
            .collect::<RpcResult<Vec<u64>>>()?;
        let rewards = list("reward")?;
        let blocks = newest + 1 - oldest;
        base_fees.truncate(blocks as usize);
        let median_tips = rewards
            .iter()
            .map(|r| match r.as_array().and_then(|r| r.first()) {
                Some(value) => parse_quantity(M, value),
                None => Ok(0),
            })
            .collect::<RpcResult<Vec<u64>>>()?;
        if base_fees.len() as u64 != blocks || median_tips.len() as u64 != blocks {
            return Err(bad(
                M,
                format!(
                    "asked for blocks {oldest} to {newest}, got {} base fees and {} tips",
                    base_fees.len(),
                    median_tips.len()
                ),
            ));
        }
        Ok(FeeHistory {
            oldest,
            base_fees,
            median_tips,
        })
    }
}

impl RpcClient {
    /// Many view calls at one block, sent in batches, each with its own
    /// outcome: a call that reverts is an error in its place, and the
    /// others stand. A reply that does not fit is an error for all.
    pub fn eth_call_each(
        &self,
        calls: &[(Address, Vec<u8>)],
        block: u64,
        batch_size: usize,
    ) -> RpcResult<Vec<RpcResult<Vec<u8>>>> {
        let at: Vec<(Address, Vec<u8>, u64)> = calls
            .iter()
            .map(|(to, data)| (*to, data.clone(), block))
            .collect();
        self.eth_call_each_at(&at, batch_size)
    }

    /// Like `eth_call_each`, each call at a block of its own.
    pub fn eth_call_each_at(
        &self,
        calls: &[(Address, Vec<u8>, u64)],
        batch_size: usize,
    ) -> RpcResult<Vec<RpcResult<Vec<u8>>>> {
        let mut out = Vec::with_capacity(calls.len());
        for group in calls.chunks(batch_size.max(1)) {
            let batch: Vec<(&str, Value)> = group
                .iter()
                .map(|(to, data, block)| ("eth_call", call_params(*to, data, *block)))
                .collect();
            for result in self.batch_each(&batch)? {
                out.push(result.and_then(|value| parse_bytes("eth_call", &value)));
            }
        }
        Ok(out)
    }
}

/// The filter of an `eth_getLogs` request on the event types `topic0s`;
/// empty means all.
fn log_filter(addresses: &[Address], topic0s: &[B256], from_block: u64, to_block: u64) -> Value {
    log_filter_where(addresses, &[topic0s.to_vec()], from_block, to_block)
}

/// The filter of an `eth_getLogs` request whose topics match `topics`, as
/// `RpcClient::get_logs_where` describes.
fn log_filter_where(
    addresses: &[Address],
    topics: &[Vec<B256>],
    from_block: u64,
    to_block: u64,
) -> Value {
    let address = match addresses {
        [one] => json!(format!("{one:#x}")),
        many => json!(many.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>()),
    };
    let mut filter = json!({
        "address": address,
        "fromBlock": quantity(from_block),
        "toBlock": quantity(to_block),
    });
    // Trailing positions that allow anything are left out.
    let used = topics
        .iter()
        .rposition(|t| !t.is_empty())
        .map_or(0, |i| i + 1);
    if used > 0 {
        let positions: Vec<Value> = topics[..used]
            .iter()
            .map(|allowed| match allowed.as_slice() {
                [] => Value::Null,
                some => json!(some.iter().map(|t| format!("{t:#x}")).collect::<Vec<_>>()),
            })
            .collect();
        filter["topics"] = json!(positions);
    }
    filter
}

fn call_params(to: Address, data: &[u8], block: u64) -> Value {
    json!([
        {"to": format!("{to:#x}"), "data": format!("0x{}", hex::encode(data))},
        quantity(block)
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockNode;
    use std::sync::{Arc, Mutex};

    fn log_json(block: u64, index: u64) -> Value {
        json!({
            "address": "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640",
            "topics": ["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"],
            "data": "0x0102",
            "blockNumber": quantity(block),
            "logIndex": quantity(index),
            "transactionHash": format!("0x{:064x}", block * 1000 + index),
            "removed": false
        })
    }

    fn pool() -> Address {
        "0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"
            .parse()
            .unwrap()
    }

    #[test]
    fn quantities_and_bytes_are_parsed_strictly() {
        assert_eq!(parse_quantity("m", &json!("0x1b4")), Ok(436));
        assert_eq!(parse_quantity("m", &json!("0x0")), Ok(0));
        assert!(parse_quantity("m", &json!("436")).is_err());
        assert!(parse_quantity("m", &json!(436)).is_err());
        assert_eq!(
            parse_bytes("m", &json!("0xdeadbeef")),
            Ok(vec![0xde, 0xad, 0xbe, 0xef])
        );
        assert_eq!(parse_bytes("m", &json!("0x")), Ok(vec![]));
        assert!(parse_bytes("m", &json!("0xzz")).is_err());
    }

    #[test]
    fn a_node_says_whether_it_is_still_syncing_and_how_far_it_is() {
        let synced = MockNode::start(|method, _| {
            assert_eq!(method, "eth_syncing");
            Ok(json!(false))
        });
        assert_eq!(RpcClient::new(synced.url()).syncing(), Ok(None));

        // Reth answers with more fields than the two that matter.
        let behind = MockNode::start(|_, _| {
            Ok(json!({
                "startingBlock": "0x0",
                "currentBlock": "0x64",
                "highestBlock": "0xc8",
                "stages": [{"name": "Headers", "block": "0xc8"}]
            }))
        });
        assert_eq!(
            RpcClient::new(behind.url()).syncing(),
            Ok(Some(SyncProgress {
                current: 100,
                highest: 200
            }))
        );

        // Anything but `false` means syncing, even without block numbers.
        let vague = MockNode::start(|_, _| Ok(json!(true)));
        assert_eq!(
            RpcClient::new(vague.url()).syncing(),
            Ok(Some(SyncProgress {
                current: 0,
                highest: 0
            }))
        );
        let broken = MockNode::start(|_, _| Ok(json!({"currentBlock": 12})));
        assert!(RpcClient::new(broken.url()).syncing().is_err());
    }

    #[test]
    fn fee_history_drops_the_next_block_and_reads_the_median_tip() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_feeHistory");
            assert_eq!(params[0], json!("0x3"));
            assert_eq!(params[1], json!("0xc"));
            Ok(json!({
                "oldestBlock": "0xa",
                "baseFeePerGas": ["0x64", "0x65", "0x66", "0x67"],
                "gasUsedRatio": [0.5, 0.5, 0.0],
                "reward": [["0x5"], ["0x6"], []]
            }))
        });
        let fees = RpcClient::new(node.url()).fee_history(12, 3).unwrap();
        assert_eq!(fees.oldest, 10);
        assert_eq!(fees.base_fees, vec![100, 101, 102]);
        assert_eq!(
            fees.median_tips,
            vec![5, 6, 0],
            "an empty block pays no tip"
        );

        let short = MockNode::start(|_, _| {
            Ok(json!({"oldestBlock": "0xa", "baseFeePerGas": ["0x64"], "reward": [["0x5"]]}))
        });
        assert!(RpcClient::new(short.url()).fee_history(12, 3).is_err());
    }

    #[test]
    fn block_times_come_back_in_the_order_asked_across_batches() {
        let requests = Arc::new(Mutex::new(0usize));
        let seen = Arc::clone(&requests);
        let node = MockNode::start(move |method, params| {
            assert_eq!(method, "eth_getBlockByNumber");
            *seen.lock().unwrap() += 1;
            let number = parse_quantity("test", &params[0]).unwrap();
            if number == 999 {
                return Ok(Value::Null);
            }
            Ok(json!({"timestamp": quantity(number * 12)}))
        });
        let client = RpcClient::new(node.url());
        let numbers: Vec<u64> = (0..450).rev().collect();
        let times = client.block_times(&numbers).unwrap();
        assert_eq!(times.len(), 450);
        assert!(numbers.iter().zip(&times).all(|(n, t)| *t == n * 12));
        assert_eq!(*requests.lock().unwrap(), 450);
        assert_eq!(client.block_times(&[]).unwrap(), Vec::<u64>::new());
        let error = client.block_times(&[5, 999]).unwrap_err();
        assert!(error.to_string().contains("999"), "{error}");
    }

    #[test]
    fn a_filter_on_later_topics_is_sent_position_by_position() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let node = MockNode::start(move |method, params| {
            assert_eq!(method, "eth_getLogs");
            record.lock().unwrap().push(params[0]["topics"].clone());
            Ok(json!([]))
        });
        let client = RpcClient::new(node.url());
        let a = B256::repeat_byte(0xaa);
        let b = B256::repeat_byte(0xbb);
        let c = B256::repeat_byte(0xcc);
        client
            .get_logs_where(&[pool()], &[vec![a], vec![], vec![b, c]], 1, 2)
            .unwrap();
        // Positions that allow anything at the end are left out.
        client
            .get_logs_where(&[pool()], &[vec![a], vec![], vec![]], 1, 2)
            .unwrap();
        client.get_logs_where(&[pool()], &[], 1, 2).unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0],
            json!([
                [format!("{a:#x}")],
                null,
                [format!("{b:#x}"), format!("{c:#x}")]
            ])
        );
        assert_eq!(seen[1], json!([[format!("{a:#x}")]]));
        assert_eq!(seen[2], Value::Null);
    }

    #[test]
    fn logs_are_parsed_and_sorted_into_chain_order() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_getLogs");
            let filter = &params[0];
            assert_eq!(filter["fromBlock"], "0x64");
            assert_eq!(filter["toBlock"], "0xc8");
            assert_eq!(
                filter["address"],
                "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"
            );
            assert!(filter["topics"][0].is_array());
            Ok(json!([
                log_json(150, 7),
                log_json(120, 3),
                log_json(150, 2)
            ]))
        });
        let client = RpcClient::new(node.url());
        let topic: B256 = "0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"
            .parse()
            .unwrap();
        let logs = client.get_logs(pool(), &[topic], 100, 200).unwrap();
        let order: Vec<(u64, u64)> = logs.iter().map(|l| (l.block_number, l.log_index)).collect();
        assert_eq!(order, vec![(120, 3), (150, 2), (150, 7)]);
        assert_eq!(logs[0].data, vec![1, 2]);
        assert_eq!(logs[0].address, pool());
    }

    #[test]
    fn a_removed_log_is_an_error() {
        let node = MockNode::start(|_, _| {
            let mut log = log_json(1, 0);
            log["removed"] = json!(true);
            Ok(json!([log]))
        });
        let client = RpcClient::new(node.url());
        assert!(client.get_logs(pool(), &[], 1, 1).is_err());
    }

    fn other() -> Address {
        "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
            .parse()
            .unwrap()
    }

    fn log_of(address: Address, block: u64, index: u64) -> Value {
        let mut log = log_json(block, index);
        log["address"] = json!(format!("{address:#x}"));
        log
    }

    #[test]
    fn logs_of_several_contracts_come_from_one_request_in_chain_order() {
        let requests = Arc::new(Mutex::new(0usize));
        let seen = requests.clone();
        let node = MockNode::start(move |method, params| {
            assert_eq!(method, "eth_getLogs");
            *seen.lock().unwrap() += 1;
            assert_eq!(
                params[0]["address"],
                json!([
                    "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640",
                    "0x8ad599c3a0ff1de082011efddc58f1908eb6e6d8"
                ])
            );
            Ok(json!([
                log_of(other(), 150, 1),
                log_of(pool(), 120, 3),
                log_of(pool(), 150, 0)
            ]))
        });
        let client = RpcClient::new(node.url());
        let logs = client
            .get_logs_many(&[pool(), other()], &[], 100, 200)
            .unwrap();
        let order: Vec<(Address, u64, u64)> = logs
            .iter()
            .map(|l| (l.address, l.block_number, l.log_index))
            .collect();
        assert_eq!(
            order,
            vec![(pool(), 120, 3), (pool(), 150, 0), (other(), 150, 1)]
        );
        assert_eq!(*requests.lock().unwrap(), 1);

        // In chunks, still one request per chunk for both contracts.
        *requests.lock().unwrap() = 0;
        client
            .get_logs_chunked_many(&[pool(), other()], &[], 100, 299, 100, |_, _| {})
            .unwrap();
        assert_eq!(*requests.lock().unwrap(), 2);
    }

    #[test]
    fn a_log_of_a_contract_not_asked_for_is_an_error() {
        let node = MockNode::start(|_, _| Ok(json!([log_of(pool(), 5, 0), log_of(other(), 6, 0)])));
        let client = RpcClient::new(node.url());
        let error = client.get_logs(pool(), &[], 1, 10).unwrap_err();
        assert!(error.to_string().contains("not asked for"), "{error}");
    }

    #[test]
    fn no_contracts_means_no_logs_and_no_request() {
        let node = MockNode::start(|_, _| panic!("no request should be sent"));
        let client = RpcClient::new(node.url());
        assert_eq!(client.get_logs_many(&[], &[], 1, 10), Ok(vec![]));
        assert_eq!(
            client.get_logs_chunked_many(&[], &[], 1, 10, 5, |_, _| {}),
            Ok(vec![])
        );
    }

    #[test]
    fn chunked_log_fetching_shrinks_the_range_when_refused_and_covers_everything() {
        let requests: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        // This node refuses ranges wider than 10 blocks and has one log per block.
        let node = MockNode::start(move |_, params| {
            let from = parse_quantity("t", &params[0]["fromBlock"]).unwrap();
            let to = parse_quantity("t", &params[0]["toBlock"]).unwrap();
            seen.lock().unwrap().push((from, to));
            if to - from + 1 > 10 {
                return Err((-32602, "block range too wide".into()));
            }
            Ok(Value::Array((from..=to).map(|b| log_json(b, 0)).collect()))
        });
        let client = RpcClient::new(node.url());
        let mut progress = Vec::new();
        let logs = client
            .get_logs_chunked(pool(), &[], 1_000, 1_099, 64, |block, count| {
                progress.push((block, count))
            })
            .unwrap();

        let blocks: Vec<u64> = logs.iter().map(|l| l.block_number).collect();
        assert_eq!(
            blocks,
            (1_000..=1_099).collect::<Vec<_>>(),
            "no gaps, no duplicates"
        );
        assert_eq!(progress.last(), Some(&(1_099, 100)));
        let accepted: Vec<(u64, u64)> = requests
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|(f, t)| t - f < 10)
            .collect();
        assert!(
            accepted.windows(2).all(|w| w[1].0 == w[0].1 + 1),
            "ranges are contiguous"
        );
    }

    #[test]
    fn chunked_log_fetching_waits_out_a_rate_limit_without_shrinking_the_range() {
        let requests: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        // The first two requests are rate limited, the way Infura says it.
        let node = MockNode::start(move |_, params| {
            let from = parse_quantity("t", &params[0]["fromBlock"]).unwrap();
            let to = parse_quantity("t", &params[0]["toBlock"]).unwrap();
            let mut requests = seen.lock().unwrap();
            requests.push((from, to));
            if requests.len() <= 2 {
                return Err((-32005, "project ID request rate exceeded".into()));
            }
            Ok(Value::Array((from..=to).map(|b| log_json(b, 0)).collect()))
        });
        let waits = Arc::new(Mutex::new(0usize));
        let counted = waits.clone();
        let client = RpcClient::new(node.url()).with_sleep(move |_| *counted.lock().unwrap() += 1);
        let logs = client
            .get_logs_chunked(pool(), &[], 1_000, 1_099, 64, |_, _| {})
            .unwrap();
        assert_eq!(logs.len(), 100);
        assert_eq!(
            *requests.lock().unwrap(),
            vec![
                (1_000, 1_063),
                (1_000, 1_063),
                (1_000, 1_063),
                (1_064, 1_099)
            ]
        );
        assert_eq!(*waits.lock().unwrap(), 2);
    }

    #[test]
    fn chunked_log_fetching_returns_other_refusals_at_once() {
        let requests = Arc::new(Mutex::new(0usize));
        let seen = requests.clone();
        let node = MockNode::start(move |_, _| {
            *seen.lock().unwrap() += 1;
            Err((-32000, "header not found".into()))
        });
        let client = RpcClient::new(node.url()).with_sleep(|_| {});
        let error = client
            .get_logs_chunked(pool(), &[], 1_000, 1_099, 64, |_, _| {})
            .unwrap_err();
        assert!(error.to_string().contains("header not found"), "{error}");
        assert_eq!(*requests.lock().unwrap(), 1);
    }

    #[test]
    fn the_finalized_block_is_read_when_the_node_serves_the_tag() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_getBlockByNumber");
            assert_eq!(params[0], "finalized");
            Ok(json!({"number": "0x64", "timestamp": "0x3e8", "hash": format!("0x{:064x}", 1)}))
        });
        assert_eq!(
            RpcClient::new(node.url()).finalized_block(),
            Ok(Some((100, 1_000)))
        );
        // A node that refuses the tag, or knows no finalized block yet.
        let refusing = MockNode::start(|_, _| Err((-32602, "invalid block tag".into())));
        assert_eq!(RpcClient::new(refusing.url()).finalized_block(), Ok(None));
        let unaware = MockNode::start(|_, _| Ok(Value::Null));
        assert_eq!(RpcClient::new(unaware.url()).finalized_block(), Ok(None));
        let broken = MockNode::start(|_, _| Ok(json!({"timestamp": "0x1"})));
        assert!(RpcClient::new(broken.url()).finalized_block().is_err());
    }

    #[test]
    fn code_is_read_at_a_block() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_getCode");
            assert_eq!(params[0], "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640");
            Ok(json!(if params[1] == "0x10" { "0x6080" } else { "0x" }))
        });
        let client = RpcClient::new(node.url());
        assert_eq!(client.code_at(pool(), 16), Ok(vec![0x60, 0x80]));
        assert_eq!(client.code_at(pool(), 15), Ok(vec![]));
    }

    #[test]
    fn chunked_log_fetching_gives_up_on_transport_errors() {
        let client = RpcClient::new("http://127.0.0.1:9")
            .with_retries(0, std::time::Duration::from_millis(1));
        assert!(matches!(
            client.get_logs_chunked(pool(), &[], 1, 10, 5, |_, _| {}),
            Err(RpcError::Transport { .. })
        ));
    }

    #[test]
    fn view_calls_carry_the_target_data_and_block() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_call");
            assert_eq!(
                params[0]["to"],
                "0x88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"
            );
            assert_eq!(params[1], "0x10");
            // Echo the call data back.
            Ok(params[0]["data"].clone())
        });
        let client = RpcClient::new(node.url());
        assert_eq!(
            client.eth_call(pool(), &[0xab, 0xcd], 16),
            Ok(vec![0xab, 0xcd])
        );

        let calls: Vec<(Address, Vec<u8>)> = (0..7u8).map(|i| (pool(), vec![i])).collect();
        let results = client.eth_call_many(&calls, 16, 3).unwrap();
        assert_eq!(results, (0..7u8).map(|i| vec![i]).collect::<Vec<_>>());
    }

    #[test]
    fn block_header_reads_timestamp_and_hash() {
        let node = MockNode::start(|_, params| {
            if params[0] == "0x5" {
                Ok(json!({"timestamp": "0x64", "hash": format!("0x{:064x}", 5)}))
            } else {
                Ok(Value::Null)
            }
        });
        let client = RpcClient::new(node.url());
        let (timestamp, hash) = client.block_header(5).unwrap();
        assert_eq!(timestamp, 100);
        assert_eq!(format!("{hash:#x}"), format!("0x{:064x}", 5));
        assert!(client.block_header(6).is_err());
    }

    fn header_json(number: u64) -> Value {
        json!({
            "number": quantity(number),
            "hash": format!("0x{:064x}", number + 1000),
            "parentHash": format!("0x{:064x}", number + 999),
            "timestamp": quantity(1_000 + number * 12),
        })
    }

    #[test]
    fn headers_come_back_with_number_hash_parent_and_time_in_the_order_asked() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_getBlockByNumber");
            match params[0].as_str().unwrap() {
                "latest" => Ok(header_json(42)),
                "0x63" => Ok(Value::Null),
                "0x64" => Ok(header_json(7)),
                tag => Ok(header_json(
                    u64::from_str_radix(tag.trim_start_matches("0x"), 16).unwrap(),
                )),
            }
        });
        let client = RpcClient::new(node.url());
        let latest = client.latest_header().unwrap();
        assert_eq!(latest.number, 42);
        assert_eq!(latest.time, 1_000 + 42 * 12);
        assert_eq!(
            format!("{:#x}", latest.parent_hash),
            format!("0x{:064x}", 1041)
        );

        let numbers: Vec<u64> = (1..=450).rev().filter(|n| *n != 99 && *n != 100).collect();
        let headers = client.block_headers(&numbers).unwrap();
        assert_eq!(
            headers.iter().map(|h| h.number).collect::<Vec<_>>(),
            numbers
        );
        assert_eq!(
            headers[0].hash,
            format!("0x{:064x}", 1450).parse::<B256>().unwrap()
        );
        // A block the node does not have, and an answer for another block, are errors.
        assert!(client.block_headers(&[99]).is_err());
        assert!(client.block_headers(&[100]).is_err());
    }

    #[test]
    fn hashed_logs_carry_their_block_hash_and_come_in_chain_order() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_getLogs");
            assert_eq!(params[0]["fromBlock"], "0x5");
            let with_hash = |block: u64, index: u64| {
                let mut log = log_json(block, index);
                log["blockHash"] = json!(format!("0x{:064x}", block + 77));
                log
            };
            Ok(json!([with_hash(6, 0), with_hash(5, 3), with_hash(5, 1)]))
        });
        let client = RpcClient::new(node.url());
        let logs = client.get_logs_hashed(&[pool()], &[], 5, 6).unwrap();
        let order: Vec<(u64, u64)> = logs
            .iter()
            .map(|l| (l.log.block_number, l.log.log_index))
            .collect();
        assert_eq!(order, vec![(5, 1), (5, 3), (6, 0)]);
        assert_eq!(
            logs[2].block_hash,
            format!("0x{:064x}", 83).parse::<B256>().unwrap()
        );
        assert!(client.get_logs_hashed(&[], &[], 5, 6).unwrap().is_empty());

        // A log without its block hash cannot be checked, so it is refused.
        let bare = MockNode::start(|_, _| Ok(json!([log_json(5, 0)])));
        assert!(RpcClient::new(bare.url())
            .get_logs_hashed(&[pool()], &[], 5, 5)
            .is_err());
    }

    #[test]
    fn view_calls_one_by_one_keep_a_revert_in_its_place() {
        let node = MockNode::start(|_, params| {
            let data = params[0]["data"].as_str().unwrap().to_string();
            if data == "0x02" || data == "0x05" {
                Err((3, "execution reverted: Invalid token ID".into()))
            } else {
                Ok(json!(data))
            }
        });
        let client = RpcClient::new(node.url());
        let calls: Vec<(Address, Vec<u8>)> = (0..7u8).map(|i| (pool(), vec![i])).collect();
        let results = client.eth_call_each(&calls, 16, 3).unwrap();
        assert_eq!(results.len(), 7);
        for (i, result) in results.iter().enumerate() {
            match i {
                2 | 5 => assert!(
                    matches!(result, Err(RpcError::Rpc { code: 3, .. })),
                    "{result:?}"
                ),
                _ => assert_eq!(result, &Ok(vec![i as u8])),
            }
        }
        // The batch that fails on the first refusal still does.
        assert!(client.eth_call_many(&calls, 16, 3).is_err());
    }
}
