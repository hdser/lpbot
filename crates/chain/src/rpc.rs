//! A small JSON-RPC client over HTTP.
//!
//! It does only what the bot needs: single calls, batches, and retries on
//! transport errors and overload responses. It holds no keys and sends no
//! transactions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum RpcError {
    /// The request never produced a usable HTTP response.
    #[error("cannot reach {url}: {reason}")]
    Transport { url: String, reason: String },
    /// The endpoint answered with a JSON-RPC error.
    #[error("the node refused `{method}`: {message} (code {code})")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },
    /// The endpoint answered with something that is not valid JSON-RPC.
    #[error("unexpected reply to `{method}`: {reason}")]
    BadResponse { method: String, reason: String },
}

pub type RpcResult<T> = Result<T, RpcError>;

/// A JSON-RPC endpoint.
pub struct RpcClient {
    url: String,
    agent: ureq::Agent,
    max_retries: u32,
    retry_base: Duration,
    next_id: AtomicU64,
}

impl RpcClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(5))
                .timeout(Duration::from_secs(120))
                .build(),
            max_retries: 4,
            retry_base: Duration::from_millis(200),
            next_id: AtomicU64::new(1),
        }
    }

    /// Changes how often and how patiently failed requests are retried.
    pub fn with_retries(mut self, max_retries: u32, retry_base: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_base = retry_base;
        self
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// One JSON-RPC call. Returns the `result` value.
    pub fn call(&self, method: &str, params: Value) -> RpcResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let reply = self.post(method, &body)?;
        extract_result(method, reply)
    }

    /// Several calls in one HTTP request. Results come back in call order.
    /// The first JSON-RPC error fails the whole batch.
    pub fn batch(&self, calls: &[(&str, Value)]) -> RpcResult<Vec<Value>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let first_id = self
            .next_id
            .fetch_add(calls.len() as u64, Ordering::Relaxed);
        let body: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| {
                json!({"jsonrpc": "2.0", "id": first_id + i as u64, "method": method, "params": params})
            })
            .collect();
        let label = calls[0].0;
        let reply = self.post(label, &Value::Array(body))?;

        let Value::Array(items) = reply else {
            // Some endpoints answer a refused batch with a single error object.
            return extract_result(label, reply).and_then(|_| {
                Err(RpcError::BadResponse {
                    method: label.to_string(),
                    reason: "expected a batch reply".into(),
                })
            });
        };
        if items.len() != calls.len() {
            return Err(RpcError::BadResponse {
                method: label.to_string(),
                reason: format!("sent {} calls, got {} replies", calls.len(), items.len()),
            });
        }

        let mut results: Vec<Option<Value>> = vec![None; calls.len()];
        for item in items {
            let index = item
                .get("id")
                .and_then(Value::as_u64)
                .and_then(|id| id.checked_sub(first_id))
                .filter(|i| (*i as usize) < calls.len())
                .ok_or_else(|| RpcError::BadResponse {
                    method: label.to_string(),
                    reason: "a batch reply has an unknown id".into(),
                })? as usize;
            results[index] = Some(extract_result(calls[index].0, item)?);
        }
        results
            .into_iter()
            .map(|r| {
                r.ok_or_else(|| RpcError::BadResponse {
                    method: label.to_string(),
                    reason: "a batch reply is missing".into(),
                })
            })
            .collect()
    }

    fn post(&self, method: &str, body: &Value) -> RpcResult<Value> {
        let mut attempt = 0;
        loop {
            let outcome = self.agent.post(&self.url).send_json(body);
            let retryable_reason = match outcome {
                Ok(response) => {
                    return response
                        .into_json::<Value>()
                        .map_err(|e| RpcError::BadResponse {
                            method: method.to_string(),
                            reason: format!("reply is not JSON: {e}"),
                        })
                }
                Err(ureq::Error::Status(code, response)) => {
                    // JSON-RPC errors sometimes arrive with a non-200 status.
                    let text = response.into_string().unwrap_or_default();
                    if let Ok(value) = serde_json::from_str::<Value>(&text) {
                        if value.get("error").is_some() || value.is_array() {
                            return Ok(value);
                        }
                    }
                    if code == 429 || code >= 500 {
                        format!("HTTP {code}")
                    } else {
                        return Err(RpcError::Transport {
                            url: self.url.clone(),
                            reason: format!("HTTP {code}"),
                        });
                    }
                }
                Err(ureq::Error::Transport(t)) => t.to_string(),
            };

            if attempt >= self.max_retries {
                return Err(RpcError::Transport {
                    url: self.url.clone(),
                    reason: retryable_reason,
                });
            }
            std::thread::sleep(self.retry_base * 2u32.pow(attempt));
            attempt += 1;
        }
    }
}

fn extract_result(method: &str, reply: Value) -> RpcResult<Value> {
    if let Some(error) = reply.get("error") {
        return Err(RpcError::Rpc {
            method: method.to_string(),
            code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message")
                .to_string(),
        });
    }
    match reply {
        Value::Object(mut map) => map.remove("result").ok_or_else(|| RpcError::BadResponse {
            method: method.to_string(),
            reason: "reply has neither result nor error".into(),
        }),
        _ => Err(RpcError::BadResponse {
            method: method.to_string(),
            reason: "reply is not an object".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockNode;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[test]
    fn a_call_returns_the_result_value() {
        let node = MockNode::start(|method, params| {
            assert_eq!(method, "eth_blockNumber");
            assert_eq!(params, json!([]));
            Ok(json!("0x10"))
        });
        let client = RpcClient::new(node.url());
        assert_eq!(client.call("eth_blockNumber", json!([])), Ok(json!("0x10")));
    }

    #[test]
    fn a_json_rpc_error_names_the_method_and_message() {
        let node =
            MockNode::start(|_, _| Err((-32005, "query returned more than 10000 results".into())));
        let client = RpcClient::new(node.url());
        let err = client.call("eth_getLogs", json!([])).unwrap_err();
        assert_eq!(
            err,
            RpcError::Rpc {
                method: "eth_getLogs".into(),
                code: -32005,
                message: "query returned more than 10000 results".into()
            }
        );
    }

    #[test]
    fn a_batch_keeps_call_order() {
        let node = MockNode::start(|method, params| Ok(json!(format!("{method}:{}", params[0]))));
        let client = RpcClient::new(node.url());
        let calls: Vec<(&str, Value)> = (0..25).map(|i| ("m", json!([i]))).collect();
        let results = client.batch(&calls).unwrap();
        for (i, r) in results.iter().enumerate() {
            assert_eq!(r, &json!(format!("m:{i}")));
        }
        assert_eq!(client.batch(&[]), Ok(vec![]));
    }

    #[test]
    fn a_batch_fails_on_the_first_error() {
        let node = MockNode::start(|_, params| {
            if params[0] == json!(3) {
                Err((-32000, "boom".into()))
            } else {
                Ok(json!("ok"))
            }
        });
        let client = RpcClient::new(node.url());
        let calls: Vec<(&str, Value)> = (0..5).map(|i| ("m", json!([i]))).collect();
        assert!(matches!(
            client.batch(&calls),
            Err(RpcError::Rpc { code: -32000, .. })
        ));
    }

    #[test]
    fn overload_responses_are_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let node = MockNode::start_raw(move |_body| {
            if seen.fetch_add(1, Ordering::SeqCst) < 2 {
                (429, "slow down".to_string())
            } else {
                (
                    200,
                    json!({"jsonrpc": "2.0", "id": 1, "result": "0x1"}).to_string(),
                )
            }
        });
        let client = RpcClient::new(node.url()).with_retries(4, Duration::from_millis(1));
        assert_eq!(client.call("eth_chainId", json!([])), Ok(json!("0x1")));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn an_unreachable_endpoint_is_a_transport_error_after_retries() {
        // Nothing listens on this port.
        let client = RpcClient::new("http://127.0.0.1:9").with_retries(1, Duration::from_millis(1));
        assert!(matches!(
            client.call("eth_blockNumber", json!([])),
            Err(RpcError::Transport { .. })
        ));
    }

    #[test]
    fn a_client_error_status_is_not_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let node = MockNode::start_raw(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            (403, "forbidden".to_string())
        });
        let client = RpcClient::new(node.url()).with_retries(4, Duration::from_millis(1));
        assert!(matches!(
            client.call("eth_blockNumber", json!([])),
            Err(RpcError::Transport { .. })
        ));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
