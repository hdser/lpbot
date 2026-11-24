//! The peers' records an agent is handed at a look: only those as of the
//! look or earlier (the rule about time in `docs/DESIGN.md`), shown by look
//! number, day and how long before the look, never by time or block.
//!
//! `agent next` hands over what is new since the agent's earlier looks;
//! `agent peers` shows everything up to the look waiting for a decision.
//! The agent never opens the peers' files: what it may know of them is
//! what these print.

use std::collections::HashSet;
use std::fmt::Write as _;

use anyhow::Result;
use serde_json::{json, Value};

use crate::agent_log::{read_records, Expectation, Identity};

/// A record's identity across files: who, which kind, which look, and over
/// which horizon for a score.
pub type Key = (String, String, u64, String);

pub fn key(record: &Value) -> Key {
    let text = |name: &str| {
        record
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    (
        text("agent"),
        text("kind"),
        record.get("look").and_then(Value::as_u64).unwrap_or(0),
        text("horizon"),
    )
}

/// What a look was handed: the text shown and a reference to each record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Handover {
    pub text: String,
    pub shown: Vec<Value>,
}

/// The keys of the records an agent's earlier looks were handed, from its
/// own records' `shown`.
pub fn already_shown(own: &[Value]) -> HashSet<Key> {
    own.iter()
        .filter_map(|r| r.get("shown").and_then(Value::as_array))
        .flatten()
        .map(key)
        .collect()
}

fn span(seconds: u64) -> String {
    let hours = seconds as f64 / 3_600.0;
    if hours < 48.0 {
        format!("{hours:.1} h")
    } else {
        format!("{:.1} d", hours / 24.0)
    }
}

fn pct(value: &Value) -> String {
    value
        .as_f64()
        .map(|v| format!("{v:+.2}%"))
        .unwrap_or_else(|| "-".into())
}

/// A decision as one phrase: `enter -3.00% to -1.00%, next look in 4h`.
fn decision_phrase(decision: &Value) -> String {
    let action = decision
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let range = if action == "enter" {
        format!(
            " {} to {}",
            pct(&decision["lower_pct"]),
            pct(&decision["upper_pct"])
        )
    } else {
        String::new()
    };
    match decision.get("next").and_then(Value::as_str) {
        Some(next) => format!("{action}{range}, next look in {next}"),
        None => format!("{action}{range}"),
    }
}

/// The few numbers of a peer's look worth repeating: its price moves, its
/// volatility, its position and its result.
fn seen_line(seen: &Value) -> String {
    let scales = seen
        .get("scales")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let moves: Vec<String> = scales
        .iter()
        .filter(|s| ["1h", "4h", "1d", "7d"].contains(&s["label"].as_str().unwrap_or("")))
        .map(|s| {
            format!(
                "{} {}",
                s["label"].as_str().unwrap_or(""),
                pct(&s["change_pct"])
            )
        })
        .collect();
    let vol = scales
        .iter()
        .find(|s| s["label"] == "1d")
        .and_then(|s| s["vol_pct_per_day"].as_f64())
        .map(|v| format!("; volatility {v:.2}%/day over 1d"))
        .unwrap_or_default();
    let position = match seen.get("position") {
        Some(p) if p.is_object() => format!(
            "position {} to {}, {}, result {}",
            pct(&p["lower_pct"]),
            pct(&p["upper_pct"]),
            p["state"].as_str().unwrap_or("?"),
            pct(&p["result_pct"])
        ),
        _ => "no position".to_string(),
    };
    format!(
        "{}{vol}; {position}; its run {}",
        moves.join(", "),
        pct(&seen["result_pct"])
    )
}

fn quoted(text: &str) -> String {
    text.lines()
        .map(|line| format!("    > {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A peer's look as another agent is shown it.
fn render_look(record: &Value, model: &str, before: u64) -> String {
    let mut text = String::new();
    let agent = record["agent"].as_str().unwrap_or("?");
    let _ = writeln!(
        text,
        "{agent} ({model}), look {}, day {:.2} of its run, {} before this look: {}.",
        record["number"].as_u64().unwrap_or(0),
        record["day"].as_f64().unwrap_or(0.0),
        span(before),
        decision_phrase(&record["decision"])
    );
    let _ = writeln!(
        text,
        "  It saw (prices from its price then): {}.",
        seen_line(&record["seen"])
    );
    if let Some(expect) = Expectation::from_json(&record["expect"]) {
        let _ = writeln!(text, "  Expected: {}.", expect.describe());
    }
    if let Some(kind) = record["stance"]["kind"].as_str() {
        let toward = record["stance"]["toward"]
            .as_str()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        let _ = writeln!(text, "  Stance toward its own peers: {kind}{toward}.");
    }
    if let Some(note) = record["note"].as_str().filter(|n| !n.is_empty()) {
        let _ = writeln!(text, "  Note: {note}");
    }
    match record["reasoning"]
        .as_str()
        .filter(|r| !r.trim().is_empty())
    {
        Some(reasoning) => {
            let _ = writeln!(text, "  Reasoning:\n{}", quoted(reasoning.trim()));
        }
        None => {
            let _ = writeln!(text, "  (No reasoning written.)");
        }
    }
    text
}

/// A score of a peer's decision as another agent is shown it.
fn render_score(record: &Value, before: u64) -> String {
    let agent = record["agent"].as_str().unwrap_or("?");
    let alternatives = record["alternatives"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let change = |name: &str| {
        alternatives
            .iter()
            .find(|a| a["alt"] == name)
            .map(|a| {
                a["change_pct"]
                    .as_f64()
                    .map(|v| format!("{v:+.3}%"))
                    .unwrap_or_else(|| "-".into())
            })
            .unwrap_or_else(|| "-".into())
    };
    let horizon = record["horizon"].as_str().unwrap_or("?");
    let seconds = record["horizon_seconds"].as_u64().unwrap_or(0);
    let band = match record["expect"]["held"].as_bool() {
        Some(true) => "; the expected band held",
        Some(false) => "; the expected band did not hold",
        None => "",
    };
    format!(
        "Score of {agent}'s look {} ({}), over {horizon} ({}), known {} before this look: the \
         decision {}, cash {}, hold {}; best: {} (regret {}). Price moved {} (high {}, low {}){band}.\n",
        record["number"].as_u64().unwrap_or(0),
        decision_phrase(&record["decision"]),
        span(seconds),
        span(before),
        change("decision"),
        change("cash"),
        change("hold"),
        record["best"].as_str().unwrap_or("?"),
        record["regret_pct"]
            .as_f64()
            .map(|v| format!("{v:.3}%"))
            .unwrap_or_else(|| "-".into()),
        pct(&record["price"]["change_pct"]),
        pct(&record["price"]["high_pct"]),
        pct(&record["price"]["low_pct"]),
    )
}

/// The peers' records as of `as_of` or earlier and not among `skip`,
/// oldest first, as text for the agent and as references for its record.
pub fn handover(identity: &Identity, as_of: u64, skip: &HashSet<Key>) -> Result<Handover> {
    if identity.peers.is_empty() {
        return Ok(Handover::default());
    }
    let mut found: Vec<(u64, Value, String)> = Vec::new();
    let mut silent = Vec::new();
    for peer in &identity.peers {
        let looks = read_records(&peer.log())?;
        if looks.is_empty() {
            silent.push(peer.name.clone());
        }
        let model = looks
            .iter()
            .find_map(|r| r["model"].as_str())
            .unwrap_or("model not recorded")
            .to_string();
        let scores = read_records(&peer.scores())?;
        for record in looks
            .into_iter()
            .filter(|r| r["kind"] == "look" && r.get("decision").is_some())
            .chain(scores.into_iter().filter(|r| r["kind"] == "eval"))
        {
            let Some(at) = record.get("as_of").and_then(Value::as_u64) else {
                continue;
            };
            if at > as_of || skip.contains(&key(&record)) {
                continue;
            }
            found.push((at, record, model.clone()));
        }
    }
    found.sort_by_key(|(at, record, _)| (*at, record["kind"] != "look"));
    let names: Vec<&str> = identity.peers.iter().map(|p| p.name.as_str()).collect();
    let mut text = String::new();
    if found.is_empty() {
        let _ = writeln!(
            text,
            "Peers ({}): nothing new up to this look.",
            names.join(", ")
        );
    } else {
        let _ = writeln!(
            text,
            "Peers ({}): {} record(s) up to this look, oldest first. Their prices are from the \
             price at their own look.\n",
            names.join(", "),
            found.len()
        );
    }
    let mut shown = Vec::new();
    for (at, record, model) in &found {
        let before = as_of.saturating_sub(*at);
        if record["kind"] == "look" {
            text.push_str(&render_look(record, model, before));
            text.push('\n');
        } else {
            text.push_str(&render_score(record, before));
        }
        shown.push(json!({
            "agent": record["agent"],
            "kind": record["kind"],
            "look": record["look"],
            "horizon": record.get("horizon").cloned().unwrap_or(Value::Null),
            "as_of": at,
        }));
    }
    if !silent.is_empty() {
        let _ = writeln!(text, "No records yet from: {}.", silent.join(", "));
    }
    Ok(Handover { text, shown })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_log::{append_records, Peer};

    fn temp(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lpbot-peers-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn look(agent: &str, look: u64, as_of: u64, reasoning: &str) -> Value {
        json!({
            "schema": "lpbot.agent/1", "kind": "look", "agent": agent,
            "model": "claude-haiku-5-5", "look": look, "number": look / 100,
            "day": look as f64 / 86_400.0, "block": 123_456_789, "as_of": as_of,
            "seen": {"scales": [{"label": "1d", "change_pct": -2.5, "vol_pct_per_day": 3.1}],
                     "result_pct": 0.4, "position": null},
            "decision": {"action": "enter", "lower_pct": -3.0, "upper_pct": -1.0, "next": "4h"},
            "note": "after a fall", "reasoning": reasoning,
            "expect": {"low_pct": -2.0, "high_pct": 1.0, "confidence": 0.6, "text": ""},
            "stance": null,
        })
    }

    #[test]
    fn a_look_is_handed_only_the_peers_records_as_of_it_or_earlier() {
        let dir = temp("clock");
        let a = Peer {
            name: "a".into(),
            stem: dir.join("a"),
        };
        append_records(
            &a.log(),
            &[
                look("a", 100, 105, "first look"),
                look("a", 200, 205, "second look"),
                look("a", 300, 305, "after b's look: must not show"),
                json!({"kind": "look", "agent": "a", "look": 150, "as_of": 150}),
            ],
        )
        .unwrap();
        append_records(
            &a.scores(),
            &[
                json!({"kind": "eval", "agent": "a", "look": 100, "number": 1, "horizon": "next",
                       "horizon_seconds": 100, "as_of": 205,
                       "decision": {"action": "enter", "lower_pct": -3.0, "upper_pct": -1.0},
                       "alternatives": [{"alt": "decision", "change_pct": 0.05},
                                        {"alt": "cash", "change_pct": 0.0}],
                       "best": "decision", "regret_pct": 0.0,
                       "price": {"change_pct": 0.5, "high_pct": 1.2, "low_pct": -0.3},
                       "expect": {"held": true}}),
                json!({"kind": "eval", "agent": "a", "look": 200, "horizon": "1d",
                       "as_of": 86_600, "alternatives": []}),
            ],
        )
        .unwrap();
        let b = Identity {
            name: "b".into(),
            model: "claude-sonnet-5-5".into(),
            decisions: dir.join("b.jsonl"),
            peers: vec![a.clone()],
        };

        let first = handover(&b, 250, &HashSet::new()).unwrap();
        assert_eq!(first.shown.len(), 3, "{}", first.text);
        assert!(first.text.contains("first look"), "{}", first.text);
        assert!(first.text.contains("second look"), "{}", first.text);
        assert!(!first.text.contains("must not show"), "{}", first.text);
        assert!(
            first.text.contains("Score of a's look 1") && first.text.contains("band held"),
            "{}",
            first.text
        );
        assert!(
            !first.text.contains("123456789") && !first.text.contains("205"),
            "no block or time is shown: {}",
            first.text
        );
        assert!(
            first
                .text
                .contains("a (claude-haiku-5-5), look 1, day 0.00 of its run, 0.0 h before"),
            "{}",
            first.text
        );
        assert!(
            first
                .text
                .contains("enter -3.00% to -1.00%, next look in 4h"),
            "{}",
            first.text
        );
        assert!(first.text.contains("    > first look"), "{}", first.text);
        assert!(
            first
                .text
                .contains("Expected: price within -2.00% to +1.00%"),
            "{}",
            first.text
        );

        // What the earlier look was handed is not handed again; the record
        // as of after the earlier look is.
        let own = vec![json!({"shown": first.shown})];
        let later = handover(&b, 400, &already_shown(&own)).unwrap();
        assert_eq!(later.shown.len(), 1, "{}", later.text);
        assert!(later.text.contains("must not show"), "{}", later.text);
        let everything = handover(&b, 400, &HashSet::new()).unwrap();
        assert_eq!(everything.shown.len(), 4);
        assert_eq!(handover(&b, 104, &HashSet::new()).unwrap().shown.len(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_peer_without_records_is_named_and_no_peers_shows_nothing() {
        let dir = temp("silent");
        let b = Identity {
            name: "b".into(),
            model: "m".into(),
            decisions: dir.join("b.jsonl"),
            peers: vec![Peer {
                name: "a".into(),
                stem: dir.join("a"),
            }],
        };
        let got = handover(&b, 1_000, &HashSet::new()).unwrap();
        assert!(got.shown.is_empty());
        assert!(got.text.contains("nothing new"), "{}", got.text);
        assert!(got.text.contains("No records yet from: a."), "{}", got.text);
        let alone = Identity {
            peers: Vec::new(),
            ..b
        };
        assert_eq!(
            handover(&alone, 1_000, &HashSet::new()).unwrap(),
            Handover::default()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
