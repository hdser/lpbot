//! A small JSON-RPC client over HTTP.
//!
//! It does only what the bot needs: single calls, batches, and retries on
//! transport errors and overload responses. It holds no keys and sends no
//! transactions.
//!
//! Retried, with waits that double up to a cap: transport errors, HTTP
//! 429 and 5xx (whatever their body says), and JSON-RPC errors that say
//! the endpoint is rate limiting. A `Retry-After` header sets the wait.
//! An answer that says a log query asked for too much is never retried:
//! the caller narrows the query instead.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

/// Whether a JSON-RPC error says a log query asked for too much at once:
/// too many results, too wide a block range or too large a response.
/// Narrowing the range is the answer to it.
///
/// Endpoints use all sorts of codes for it (Infura answers `-32005 query
/// returned more than 10000 results`, with the code it also uses for rate
/// limits), so only the message decides.
pub fn range_too_wide(_code: i64, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if says_rate_limited(&message) {
        return false;
    }
    [
        "more than",
        "too many results",
        "max results",
        "block range",
        "range is too",
        "range too",
        "too large",
        "too wide",
        "response size",
        "size exceeded",
        "limited to",
        "reduce the range",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

fn says_rate_limited(message: &str) -> bool {
    [
        "rate limit",
        "rate-limit",
        "ratelimit",
        "rate exceeded",
        "too many requests",
        "request limit",
        "requests per",
        "throttl",
    ]
    .iter()
    .any(|phrase| message.contains(phrase))
}

/// Whether a JSON-RPC error says the endpoint is rate limiting: worth a
/// wait and a retry, not a narrower query. `-32005` counts only when its
/// message speaks of a limit or of an exceeded rate, since Infura also
/// uses it for queries with too many results.
pub fn rate_limited(code: i64, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    if says_rate_limited(&message) {
        return true;
    }
    code == -32005
        && (message.contains("limit") || message.contains("exceeded"))
        && !range_too_wide(code, &message)
}

impl RpcError {
    /// The endpoint is rate limiting; see [`rate_limited`].
    pub fn is_rate_limit(&self) -> bool {
        matches!(self, RpcError::Rpc { code, message, .. } if rate_limited(*code, message))
    }

    /// The query asked for too much; see [`range_too_wide`].
    pub fn is_range_too_wide(&self) -> bool {
        matches!(self, RpcError::Rpc { code, message, .. } if range_too_wide(*code, message))
    }
}

/// The error objects of a reply, single or batched.
fn reply_errors(reply: &Value) -> Vec<(i64, &str)> {
    fn error_of(item: &Value) -> Option<(i64, &str)> {
        item.get("error").map(|error| {
            (
                error.get("code").and_then(Value::as_i64).unwrap_or(0),
                error.get("message").and_then(Value::as_str).unwrap_or(""),
            )
        })
    }
    match reply {
        Value::Array(items) => items.iter().filter_map(error_of).collect(),
        single => error_of(single).into_iter().collect(),
    }
}

/// Seconds to wait from a `Retry-After` header. Dates are not read.
fn retry_after(header: Option<&str>) -> Option<Duration> {
    header?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// How the client waits between attempts.
pub type Sleep = Arc<dyn Fn(Duration) + Send + Sync>;

/// A JSON-RPC endpoint.
pub struct RpcClient {
    url: String,
    agent: ureq::Agent,
    max_retries: u32,
    retry_base: Duration,
    max_wait: Duration,
    sleep: Sleep,
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
            max_wait: Duration::from_secs(30),
            sleep: Arc::new(std::thread::sleep),
            next_id: AtomicU64::new(1),
        }
    }

    /// Changes how often and how patiently failed requests are retried.
    /// The wait doubles from `retry_base` on each attempt.
    pub fn with_retries(mut self, max_retries: u32, retry_base: Duration) -> Self {
        self.max_retries = max_retries;
        self.retry_base = retry_base;
        self
    }

    /// The longest single wait, `Retry-After` included.
    pub fn with_max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }

    /// Replaces how the client waits between attempts; tests record the
    /// waits instead of sleeping.
    pub fn with_sleep(mut self, sleep: impl Fn(Duration) + Send + Sync + 'static) -> Self {
        self.sleep = Arc::new(sleep);
        self
    }

    /// The wait before retry number `attempt + 1`: the server's
    /// `Retry-After` when given, else the base doubled `attempt` times,
    /// and never more than the cap.
    pub fn backoff(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        let doubled = self
            .retry_base
            .saturating_mul(1u32.checked_shl(attempt).unwrap_or(u32::MAX));
        retry_after.unwrap_or(doubled).min(self.max_wait)
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
        self.batch_each(calls)?.into_iter().collect()
    }

    /// Several calls in one HTTP request, each with its own outcome, in
    /// call order. A call the node refuses (a view call that reverts, say)
    /// is an error in its place and leaves the others standing; a reply
    /// that does not fit the request is an error for the whole batch.
    pub fn batch_each(&self, calls: &[(&str, Value)]) -> RpcResult<Vec<RpcResult<Value>>> {
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

        let mut results: Vec<Option<RpcResult<Value>>> = vec![None; calls.len()];
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
            results[index] = Some(extract_result(calls[index].0, item));
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
            // Why to try again, how long the server asked to wait, and the
            // JSON-RPC reply to hand back if no attempt is left.
            let (reason, asked_wait, last_reply) = match self.agent.post(&self.url).send_json(body)
            {
                Ok(response) => {
                    let reply =
                        response
                            .into_json::<Value>()
                            .map_err(|e| RpcError::BadResponse {
                                method: method.to_string(),
                                reason: format!("reply is not JSON: {e}"),
                            })?;
                    let limited = reply_errors(&reply)
                        .into_iter()
                        .find(|(code, message)| rate_limited(*code, message))
                        .map(|(_, message)| message.to_string());
                    match limited {
                        Some(message) => (format!("rate limited: {message}"), None, Some(reply)),
                        None => return Ok(reply),
                    }
                }
                Err(ureq::Error::Status(code, response)) => {
                    let asked_wait = retry_after(response.header("retry-after"));
                    // JSON-RPC errors sometimes arrive with a non-200 status.
                    let text = response.into_string().unwrap_or_default();
                    let reply = serde_json::from_str::<Value>(&text)
                        .ok()
                        .filter(|value| value.get("error").is_some() || value.is_array());
                    let errors = reply.as_ref().map(reply_errors).unwrap_or_default();
                    // A query that asked for too much is the caller's
                    // to narrow, whatever the status.
                    if errors.iter().any(|(c, m)| range_too_wide(*c, m)) {
                        return Ok(reply.expect("errors come from a reply"));
                    }
                    let limited = errors.iter().any(|(c, m)| rate_limited(*c, m));
                    if !(code == 429 || code >= 500 || limited) {
                        return match reply {
                            Some(reply) => Ok(reply),
                            None => Err(RpcError::Transport {
                                url: self.url.clone(),
                                reason: format!("HTTP {code}"),
                            }),
                        };
                    }
                    (format!("HTTP {code}"), asked_wait, reply)
                }
                Err(ureq::Error::Transport(t)) => (t.to_string(), None, None),
            };

            if attempt >= self.max_retries {
                // The endpoint's own words say more than "HTTP 429".
                return match last_reply {
                    Some(reply) => Ok(reply),
                    None => Err(RpcError::Transport {
                        url: self.url.clone(),
                        reason,
                    }),
                };
            }
            (self.sleep)(self.backoff(attempt, asked_wait));
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

    /// A client that records its waits instead of sleeping.
    fn recording(url: &str, retries: u32) -> (RpcClient, Arc<std::sync::Mutex<Vec<Duration>>>) {
        let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = waits.clone();
        let client = RpcClient::new(url)
            .with_retries(retries, Duration::from_secs(1))
            .with_sleep(move |wait| seen.lock().unwrap().push(wait));
        (client, waits)
    }

    fn error_body(code: i64, message: &str) -> String {
        json!({"jsonrpc": "2.0", "id": 1, "error": {"code": code, "message": message}}).to_string()
    }

    #[test]
    fn a_rate_limit_answered_with_a_json_rpc_error_is_retried_with_doubling_waits() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        // Alchemy and others answer 429 with a JSON-RPC error body.
        let node = MockNode::start_raw(move |_| match seen.fetch_add(1, Ordering::SeqCst) {
            0 | 1 => (429, error_body(429, "Your app has exceeded its capacity")),
            // Infura answers a rate limit with status 200 and -32005.
            2 => (200, error_body(-32005, "project ID request rate exceeded")),
            _ => (
                200,
                json!({"jsonrpc": "2.0", "id": 1, "result": "0x1"}).to_string(),
            ),
        });
        let (client, waits) = recording(node.url(), 4);
        assert_eq!(client.call("eth_chainId", json!([])), Ok(json!("0x1")));
        assert_eq!(hits.load(Ordering::SeqCst), 4);
        assert_eq!(
            *waits.lock().unwrap(),
            [1, 2, 4].map(Duration::from_secs).to_vec()
        );
    }

    #[test]
    fn a_server_error_with_a_json_rpc_body_is_retried_and_its_words_kept() {
        let node = MockNode::start_raw(|_| (503, error_body(-32603, "upstream unavailable")));
        let (client, waits) = recording(node.url(), 2);
        // After the last attempt the endpoint's own message comes back.
        let error = client.call("eth_blockNumber", json!([])).unwrap_err();
        assert!(
            error.to_string().contains("upstream unavailable"),
            "{error}"
        );
        assert_eq!(waits.lock().unwrap().len(), 2);
    }

    #[test]
    fn retry_after_sets_the_wait_and_every_wait_has_a_cap() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let node = MockNode::start_with_headers(move |_| {
            if seen.fetch_add(1, Ordering::SeqCst) < 2 {
                let after = vec![("Retry-After".to_string(), "7".to_string())];
                (429, after, "slow down".to_string())
            } else {
                let ok = json!({"jsonrpc": "2.0", "id": 1, "result": "0x1"});
                (200, Vec::new(), ok.to_string())
            }
        });
        let (client, waits) = recording(node.url(), 4);
        let client = client.with_max_wait(Duration::from_secs(5));
        assert_eq!(client.call("eth_chainId", json!([])), Ok(json!("0x1")));
        assert_eq!(
            *waits.lock().unwrap(),
            [5, 5].map(Duration::from_secs).to_vec()
        );

        let client = RpcClient::new("http://unused")
            .with_retries(9, Duration::from_secs(1))
            .with_max_wait(Duration::from_secs(5));
        let waits: Vec<u64> = (0..7).map(|a| client.backoff(a, None).as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 5, 5, 5, 5]);
        assert_eq!(client.backoff(40, None), Duration::from_secs(5));
        assert_eq!(
            client.backoff(0, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn a_query_that_asks_for_too_much_is_not_retried() {
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let node = MockNode::start_raw(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            (
                200,
                error_body(-32005, "query returned more than 10000 results"),
            )
        });
        let (client, waits) = recording(node.url(), 4);
        let error = client.call("eth_getLogs", json!([])).unwrap_err();
        assert!(
            error.is_range_too_wide() && !error.is_rate_limit(),
            "{error}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(waits.lock().unwrap().is_empty());

        // The same, sent with a server error status.
        let node = MockNode::start_raw(|_| (500, error_body(-32602, "block range too large")));
        let (client, waits) = recording(node.url(), 4);
        assert!(client
            .call("eth_getLogs", json!([]))
            .unwrap_err()
            .is_range_too_wide());
        assert!(waits.lock().unwrap().is_empty());
    }

    #[test]
    fn rate_limits_and_ranges_that_are_too_wide_are_told_apart() {
        for (code, message) in [
            (-32005, "project ID request rate exceeded"),
            (-32005, "daily request count exceeded, request rate limited"),
            (-32005, "limit exceeded"),
            (
                -32007,
                "100/second request limit reached - reduce calls per second",
            ),
            (429, "Too Many Requests"),
            (-32000, "rate limited, retry later"),
        ] {
            assert!(rate_limited(code, message), "{message}");
            assert!(!range_too_wide(code, message), "{message}");
        }
        for (code, message) in [
            (-32005, "query returned more than 10000 results"),
            (-32602, "query exceeds max block range 100000"),
            (-32602, "query exceeds max results 20000"),
            (-32602, "Log response size exceeded. You can make eth_getLogs requests with up to a 2K block range"),
            (-32000, "exceed maximum block range: 50000"),
            (-32600, "eth_getLogs is limited to a 10,000 range"),
        ] {
            assert!(range_too_wide(code, message), "{message}");
            assert!(!rate_limited(code, message), "{message}");
        }
        for (code, message) in [
            (-32000, "header not found"),
            (-32000, "missing trie node"),
            (3, "execution reverted"),
        ] {
            assert!(!rate_limited(code, message) && !range_too_wide(code, message));
        }
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
