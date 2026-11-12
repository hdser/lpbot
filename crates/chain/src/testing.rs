//! A tiny JSON-RPC server for tests. It speaks just enough HTTP for the
//! client in this crate and runs on a free local port.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

/// Answers a JSON-RPC call with a result or an error `(code, message)`.
pub type Handler = dyn Fn(&str, Value) -> Result<Value, (i64, String)> + Send + Sync;
/// Answers a raw request body with an HTTP status and a body.
pub type RawHandler = dyn Fn(&str) -> (u16, String) + Send + Sync;

pub struct MockNode {
    url: String,
}

impl MockNode {
    /// Starts a node that answers every JSON-RPC call, single or batched,
    /// through `handler`.
    pub fn start(
        handler: impl Fn(&str, Value) -> Result<Value, (i64, String)> + Send + Sync + 'static,
    ) -> Self {
        let handler: Arc<Handler> = Arc::new(handler);
        Self::start_raw(move |body| {
            let request: Value = serde_json::from_str(body).expect("request is JSON");
            let answer_one = |call: &Value| {
                let method = call["method"].as_str().expect("method").to_string();
                let params = call.get("params").cloned().unwrap_or(json!([]));
                match handler(&method, params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": call["id"], "result": result}),
                    Err((code, message)) => json!({
                        "jsonrpc": "2.0",
                        "id": call["id"],
                        "error": {"code": code, "message": message}
                    }),
                }
            };
            let reply = match &request {
                // Reply out of order on purpose: clients must match by id.
                Value::Array(calls) => Value::Array(calls.iter().rev().map(answer_one).collect()),
                single => answer_one(single),
            };
            (200, reply.to_string())
        })
    }

    /// Starts a node with full control over status and body.
    pub fn start_raw(handler: impl Fn(&str) -> (u16, String) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let url = format!("http://{}", listener.local_addr().expect("local address"));
        let handler: Arc<RawHandler> = Arc::new(handler);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = handler.clone();
                thread::spawn(move || serve(stream, &*handler));
            }
        });
        Self { url }
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

fn serve(stream: TcpStream, handler: &RawHandler) {
    let mut reader = BufReader::new(stream);
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let (status, reply) = handler(&String::from_utf8_lossy(&body));
    let reason = if status == 200 { "OK" } else { "Error" };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}
