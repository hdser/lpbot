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

impl RpcClient {
    /// Number of the latest block.
    pub fn block_number(&self) -> RpcResult<u64> {
        parse_quantity("eth_blockNumber", &self.call("eth_blockNumber", json!([]))?)
    }

    /// Chain id of the endpoint (1 for Ethereum mainnet).
    pub fn chain_id(&self) -> RpcResult<u64> {
        parse_quantity("eth_chainId", &self.call("eth_chainId", json!([]))?)
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

    /// Logs of one contract in a block range, in chain order. `topic0s`
    /// selects the event types; empty means all.
    pub fn get_logs(
        &self,
        address: Address,
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
    ) -> RpcResult<Vec<Log>> {
        let mut filter = json!({
            "address": format!("{address:#x}"),
            "fromBlock": quantity(from_block),
            "toBlock": quantity(to_block),
        });
        if !topic0s.is_empty() {
            let list: Vec<String> = topic0s.iter().map(|t| format!("{t:#x}")).collect();
            filter["topics"] = json!([list]);
        }
        let result = self.call("eth_getLogs", json!([filter]))?;
        let mut logs = result
            .as_array()
            .ok_or_else(|| bad("eth_getLogs", "expected a list of logs"))?
            .iter()
            .map(parse_log)
            .collect::<RpcResult<Vec<_>>>()?;
        logs.sort_by_key(|l| (l.block_number, l.log_index));
        Ok(logs)
    }

    /// Like `get_logs` for a long range. It asks in chunks and halves the
    /// chunk whenever the node refuses a request, which is how public
    /// endpoints signal "too many results" or "range too wide".
    pub fn get_logs_chunked(
        &self,
        address: Address,
        topic0s: &[B256],
        from_block: u64,
        to_block: u64,
        initial_chunk: u64,
        mut on_progress: impl FnMut(u64, usize),
    ) -> RpcResult<Vec<Log>> {
        let mut logs = Vec::new();
        let mut chunk = initial_chunk.max(1);
        let mut start = from_block;
        while start <= to_block {
            let end = to_block.min(start.saturating_add(chunk - 1));
            match self.get_logs(address, topic0s, start, end) {
                Ok(mut batch) => {
                    logs.append(&mut batch);
                    on_progress(end, logs.len());
                    start = end + 1;
                    // Grow back slowly after a successful request.
                    if chunk < initial_chunk {
                        chunk = (chunk * 2).min(initial_chunk);
                    }
                }
                Err(RpcError::Rpc { .. }) if chunk > 1 => chunk /= 2,
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
}
