//! `lpbot watch-safe`: the Safe watched from the local node, block by block
//! (`lpbot_exec::monitor`). Every alert is printed and appended to
//! `logs/safe-alerts.jsonl`; where it is sent beyond that (a table the
//! dashboards read, or Telegram) is the owner's choice (`docs/PLAN.md`,
//! decisions). Two narrow queries a step: the four tokens' and the position
//! manager's transfers and approvals from the Safe, and every event of the
//! Safe and its roles modifier.

use std::io::Write as _;
use std::path::Path;

use alloy_primitives::{keccak256, Address, B256};
use anyhow::{anyhow, Result};
use lpbot_exec::monitor::{classify, Alert, Allowed, Log, Severity};
use serde_json::{json, Value};

fn quantity(value: &Value) -> Result<u64> {
    let text = value
        .as_str()
        .ok_or_else(|| anyhow!("expected a hex quantity, got {value}"))?;
    Ok(u64::from_str_radix(text.trim_start_matches("0x"), 16)?)
}

fn parse_logs(value: &Value) -> Result<Vec<Log>> {
    let items = value
        .as_array()
        .ok_or_else(|| anyhow!("eth_getLogs answered {value}"))?;
    items
        .iter()
        .map(|item| {
            let topics = item["topics"]
                .as_array()
                .ok_or_else(|| anyhow!("a log without topics"))?
                .iter()
                .map(|t| {
                    t.as_str()
                        .unwrap_or_default()
                        .parse::<B256>()
                        .map_err(|e| anyhow!("topic: {e}"))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Log {
                address: item["address"]
                    .as_str()
                    .unwrap_or_default()
                    .parse()
                    .map_err(|e| anyhow!("log address: {e}"))?,
                topics,
                data: hex::decode(
                    item["data"]
                        .as_str()
                        .unwrap_or("0x")
                        .trim_start_matches("0x"),
                )?,
                block: quantity(&item["blockNumber"])?,
                transaction: item["transactionHash"]
                    .as_str()
                    .unwrap_or_default()
                    .parse()
                    .map_err(|e| anyhow!("transaction hash: {e}"))?,
            })
        })
        .collect()
}

fn word(a: Address) -> String {
    format!("0x{:0>64}", hex::encode(a.as_slice()))
}

/// The logs of blocks `from..=to` that can show value leaving the Safe or
/// its permissions changing.
pub fn read_logs(
    client: &lpbot_chain::RpcClient,
    allowed: &Allowed,
    from: u64,
    to: u64,
) -> Result<Vec<Log>> {
    let range = |extra: Value| {
        let mut filter = json!({
            "fromBlock": format!("0x{from:x}"),
            "toBlock": format!("0x{to:x}"),
        });
        for (k, v) in extra.as_object().expect("an object") {
            filter[k] = v.clone();
        }
        filter
    };
    let transfer = format!("{}", keccak256("Transfer(address,address,uint256)"));
    let approval = format!("{}", keccak256("Approval(address,address,uint256)"));
    let mut watched: Vec<Address> = allowed.tokens.clone();
    watched.push(allowed.position_manager);
    let from_safe = client.call(
        "eth_getLogs",
        json!([range(json!({
            "address": watched,
            "topics": [[transfer, approval], word(allowed.safe)],
        }))]),
    )?;
    let own = client.call(
        "eth_getLogs",
        json!([range(json!({"address": [allowed.safe, allowed.roles]}))]),
    )?;
    let mut logs = parse_logs(&from_safe)?;
    logs.extend(parse_logs(&own)?);
    logs.sort_by_key(|l| l.block);
    Ok(logs)
}

/// Appends alerts to the alerts file, one JSON line each.
pub fn record(alerts: &[Alert], path: &Path) -> Result<()> {
    if alerts.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for a in alerts {
        writeln!(
            file,
            "{}",
            json!({
                "severity": match a.severity { Severity::Critical => "critical", Severity::Warning => "warning" },
                "block": a.block,
                "transaction": a.transaction.to_string(),
                "what": a.what,
            })
        )?;
    }
    file.sync_all()?;
    Ok(())
}

/// Watches from `from` (or the head) until stopped, or for `steps` steps.
pub fn watch(
    client: &lpbot_chain::RpcClient,
    allowed: &Allowed,
    from: Option<u64>,
    every: std::time::Duration,
    steps: Option<u32>,
    alerts_path: &Path,
) -> Result<()> {
    let head = || -> Result<u64> { quantity(&client.call("eth_blockNumber", json!([]))?) };
    let mut next = match from {
        Some(block) => block,
        None => head()?,
    };
    println!(
        "watching Safe {:#x} and roles modifier {:#x} from block {next}; alerts go to {}",
        allowed.safe,
        allowed.roles,
        alerts_path.display()
    );
    let mut done = 0u32;
    loop {
        let to = head()?;
        if to >= next {
            // At most 2,000 blocks a query, as the backfill reads.
            let mut from = next;
            while from <= to {
                let end = (from + 1_999).min(to);
                let alerts = classify(&read_logs(client, allowed, from, end)?, allowed);
                for a in &alerts {
                    println!(
                        "{} block {} tx {}: {}",
                        match a.severity {
                            Severity::Critical => "CRITICAL",
                            Severity::Warning => "warning",
                        },
                        a.block,
                        a.transaction,
                        a.what
                    );
                }
                record(&alerts, alerts_path)?;
                from = end + 1;
            }
            next = to + 1;
        }
        done += 1;
        if steps.is_some_and(|s| done >= s) {
            return Ok(());
        }
        std::thread::sleep(every);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_read_back_from_the_nodes_answer_and_alerts_are_recorded() {
        let safe = Address::repeat_byte(0x5a);
        let answer = json!([{
            "address": "0xa0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0a0",
            "topics": [
                format!("{}", keccak256("Transfer(address,address,uint256)")),
                word(safe),
                word(Address::repeat_byte(0xee)),
            ],
            "data": format!("0x{:0>64}", "64"),
            "blockNumber": "0x10",
            "transactionHash": format!("0x{}", "11".repeat(32)),
        }]);
        let logs = parse_logs(&answer).unwrap();
        assert_eq!(logs[0].block, 16);
        let allowed = Allowed {
            safe,
            roles: Address::repeat_byte(0x70),
            position_manager: Address::repeat_byte(0xc3),
            spenders: Vec::new(),
            recipients: Vec::new(),
            tokens: vec![Address::repeat_byte(0xa0)],
        };
        let alerts = classify(&logs, &allowed);
        assert_eq!(alerts.len(), 1);
        assert!(
            alerts[0].what.contains("100 of token"),
            "{}",
            alerts[0].what
        );
        let path = std::env::temp_dir().join(format!("lpbot-alerts-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        record(&alerts, &path).unwrap();
        record(&[], &path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("\"critical\""));
        std::fs::remove_file(path).unwrap();
    }
}
