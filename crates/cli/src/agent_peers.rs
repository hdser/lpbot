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
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::agent_log::{Expectation, Identity};
use crate::run_cmd::parse_duration;

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
        // Old references did not distinguish a score stopped at the head
        // from a completed horizon. Let a mature replacement through once.
        .filter(|r| r["kind"] != "eval" || mature_reference(r))
        .map(key)
        .collect()
}

/// The actual interval scored, never the requested duration alone. Newer
/// evaluators also record the interval explicitly; disagreement fails closed.
fn score_interval(record: &Value) -> Option<(u64, u64, u64)> {
    let start = record.get("decided_as_of")?.as_u64()?;
    let end = record.get("as_of")?.as_u64()?;
    let seconds = end.checked_sub(start).filter(|seconds| *seconds > 0)?;
    if let Some(scored) = record.get("scored_seconds") {
        if scored.as_u64() != Some(seconds) {
            return None;
        }
    }
    if let Some(complete) = record.get("complete") {
        if complete.as_bool() != Some(true) {
            return None;
        }
    }
    let requested = record.get("horizon_seconds")?.as_u64()?;
    match record.get("horizon")?.as_str()? {
        "next" if requested == seconds => {}
        "next" => return None,
        label => {
            let duration = parse_duration(label).filter(|duration| *duration > 0)?;
            if requested != duration || seconds < duration {
                return None;
            }
        }
    }
    Some((start, end, seconds))
}

/// A completed score's reference can suppress it at later looks. References
/// written before maturity was checked intentionally do not suppress it.
fn mature_reference(record: &Value) -> bool {
    if record["complete"].as_bool() != Some(true) || score_interval(record).is_none() {
        return false;
    }
    if record["horizon"] != "next" {
        return true;
    }
    matches!(
        (record["look"].as_u64(), record["next_look"].as_u64()),
        (Some(look), Some(next)) if next > look
    ) && record["next_as_of"].as_u64() == record["as_of"].as_u64()
}

/// A fixed score must span its requested duration. A `next` score must end
/// where the next committed decision was seen, rather than at an undecided
/// scheduled look or an earlier head where that look was provisionally shown.
/// Its successor is returned so the handover can retain that proof.
fn mature_score<'a>(record: &Value, looks: &'a [Value]) -> Option<Option<&'a Value>> {
    let (_, end, _) = score_interval(record)?;
    if record["horizon"] != "next" {
        return Some(None);
    }
    let look = record.get("look")?.as_u64()?;
    let number = record
        .get("number")?
        .as_u64()
        .filter(|number| *number > 0)?
        .checked_add(1)?;
    let agent = record.get("agent")?.as_str()?;
    let next = looks
        .iter()
        .filter(|r| r["kind"] == "look" && r["agent"].as_str() == Some(agent))
        .filter_map(|r| Some((r["look"].as_u64()?, r)))
        .filter(|(next, _)| *next > look)
        .min_by_key(|(next, _)| *next)?
        .1;
    if next["number"].as_u64() != Some(number)
        || !next["decision"].is_object()
        || !matches!(
            next["decision"]["action"].as_str(),
            Some("enter" | "stay_out" | "hold" | "exit")
        )
        || next["decision"]["next"]
            .as_str()
            .and_then(parse_duration)
            .is_none_or(|seconds| seconds == 0)
        || next["as_of"].as_u64() != Some(end)
        || record["ended"] != "look"
    {
        return None;
    }
    for (field, expected) in [("next_look", &next["look"]), ("next_as_of", &next["as_of"])] {
        if record.get(field).is_some_and(|value| value != expected) {
            return None;
        }
    }
    Some(Some(next))
}

/// A final line without its newline may still be in the process of being
/// written. It cannot establish a peer's committed decision or outcome.
fn read_committed_records(path: &Path) -> Result<Vec<Value>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    content
        .split_inclusive('\n')
        .enumerate()
        .filter(|(_, line)| line.ends_with('\n') && !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("{} line {} is not JSON", path.display(), index + 1))
        })
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
    let mut found: Vec<(u64, Value, String, Option<Value>)> = Vec::new();
    let mut silent = Vec::new();
    for peer in &identity.peers {
        let looks = read_committed_records(&peer.log())?;
        if looks.is_empty() {
            silent.push(peer.name.clone());
        }
        let model = looks
            .iter()
            .find_map(|r| r["model"].as_str())
            .unwrap_or("model not recorded")
            .to_string();
        let scores = read_committed_records(&peer.scores())?;
        for record in looks
            .iter()
            .filter(|r| r["kind"] == "look" && r.get("decision").is_some())
            .chain(scores.iter().filter(|r| r["kind"] == "eval"))
        {
            let Some(at) = record.get("as_of").and_then(Value::as_u64) else {
                continue;
            };
            if at > as_of || skip.contains(&key(record)) {
                continue;
            }
            let next = if record["kind"] == "eval" {
                let Some(next) = mature_score(record, &looks) else {
                    continue;
                };
                next.cloned()
            } else {
                None
            };
            found.push((at, record.clone(), model.clone(), next));
        }
    }
    found.sort_by_key(|(at, record, _, _)| (*at, record["kind"] != "look"));
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
    for (at, record, model, next) in &found {
        let before = as_of.saturating_sub(*at);
        if record["kind"] == "look" {
            text.push_str(&render_look(record, model, before));
            text.push('\n');
        } else {
            text.push_str(&render_score(record, before));
        }
        let mut reference = json!({
            "agent": record["agent"],
            "kind": record["kind"],
            "look": record["look"],
            "horizon": record.get("horizon").cloned().unwrap_or(Value::Null),
            "as_of": at,
        });
        if record["kind"] == "eval" {
            reference["complete"] = json!(true);
            reference["decided_as_of"] = record["decided_as_of"].clone();
            reference["horizon_seconds"] = record["horizon_seconds"].clone();
            reference["scored_seconds"] = json!(*at - record["decided_as_of"].as_u64().unwrap());
            if let Some(next) = next {
                reference["next_look"] = next["look"].clone();
                reference["next_as_of"] = next["as_of"].clone();
            }
        }
        shown.push(reference);
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

    fn score(look: u64, start: u64, end: u64, horizon: &str) -> Value {
        json!({
            "kind": "eval", "agent": "a", "look": look, "number": look / 100,
            "decided_as_of": start, "as_of": end, "horizon": horizon,
            "horizon_seconds": if horizon == "next" { end - start } else { parse_duration(horizon).unwrap() },
            "ended": "look",
        })
    }

    fn reader(dir: &Path) -> Identity {
        Identity {
            name: "b".into(),
            model: "test".into(),
            decisions: dir.join("b.jsonl"),
            peers: vec![Peer {
                name: "a".into(),
                stem: dir.join("a"),
            }],
        }
    }

    fn evals(handover: &Handover) -> Vec<&Value> {
        handover
            .shown
            .iter()
            .filter(|r| r["kind"] == "eval")
            .collect()
    }

    #[test]
    fn fixed_scores_need_the_full_actual_interval_and_the_receivers_boundary() {
        let dir = temp("fixed-maturity");
        let who = reader(&dir);
        let peer = &who.peers[0];
        // Both records advertise the requested hour; only one actually spans it.
        append_records(
            &peer.scores(),
            &[score(100, 100, 3_699, "1h"), score(200, 100, 3_700, "1h")],
        )
        .unwrap();
        assert!(evals(&handover(&who, 3_699, &HashSet::new()).unwrap()).is_empty());
        let mature = handover(&who, 3_700, &HashSet::new()).unwrap();
        assert_eq!(evals(&mature).len(), 1, "{}", mature.text);
        assert_eq!(evals(&mature)[0]["look"], 200);
        assert_eq!(evals(&mature)[0]["scored_seconds"], 3_600);
        assert_eq!(evals(&mature)[0]["complete"], true);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_or_malformed_maturity_metadata_fails_closed() {
        let valid = score(100, 100, 3_700, "1h");
        assert!(
            score_interval(&valid).is_some(),
            "valid legacy scores still work"
        );
        for field in ["decided_as_of", "as_of", "horizon", "horizon_seconds"] {
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(score_interval(&missing).is_none(), "missing {field}");
            let mut malformed = valid.clone();
            malformed[field] = json!("invalid");
            assert!(score_interval(&malformed).is_none(), "malformed {field}");
        }
        for (field, value) in [
            ("horizon_seconds", json!(3_599)),
            ("as_of", json!(99)),
            ("scored_seconds", json!(3_601)),
            ("complete", json!(false)),
            ("complete", json!("true")),
        ] {
            let mut malformed = valid.clone();
            malformed[field] = value;
            assert!(score_interval(&malformed).is_none(), "{malformed}");
        }
        let valid_next = score(100, 100, 200, "next");
        let next = look("a", 200, 200, "committed");
        assert!(mature_score(&valid_next, std::slice::from_ref(&next)).is_some());
        for field in ["number", "ended"] {
            let mut missing = valid_next.clone();
            missing.as_object_mut().unwrap().remove(field);
            assert!(mature_score(&missing, std::slice::from_ref(&next)).is_none());
        }
        for (field, value) in [
            ("as_of", json!(null)),
            ("number", json!(3)),
            ("decision", json!(null)),
            ("decision", json!({"action": "hold"})),
        ] {
            let mut malformed = next.clone();
            malformed[field] = value;
            assert!(mature_score(&valid_next, &[malformed]).is_none());
        }
    }

    #[test]
    fn next_scores_need_a_committed_successor_at_the_exact_scored_boundary() {
        let dir = temp("next-maturity");
        let who = reader(&dir);
        let peer = &who.peers[0];
        let first = look("a", 100, 100, "first");
        append_records(&peer.log(), std::slice::from_ref(&first)).unwrap();
        append_records(&peer.scores(), &[score(100, 100, 200, "next")]).unwrap();
        assert!(evals(&handover(&who, 400, &HashSet::new()).unwrap()).is_empty());

        // A pending look cannot establish maturity, even in a newline-committed log.
        let mut pending = look("a", 200, 200, "pending");
        pending.as_object_mut().unwrap().remove("decision");
        append_records(&peer.log(), &[pending]).unwrap();
        assert!(evals(&handover(&who, 400, &HashSet::new()).unwrap()).is_empty());

        // Re-showing the next look moves its boundary. An older provisional
        // score stays hidden after that decision commits and must be rescored.
        let next = look("a", 200, 300, "next committed after being re-shown");
        std::fs::remove_file(peer.log()).unwrap();
        append_records(&peer.log(), &[first.clone(), next.clone()]).unwrap();
        assert!(evals(&handover(&who, 400, &HashSet::new()).unwrap()).is_empty());
        std::fs::remove_file(peer.scores()).unwrap();
        append_records(&peer.scores(), &[score(100, 100, 300, "next")]).unwrap();
        assert!(evals(&handover(&who, 299, &HashSet::new()).unwrap()).is_empty());
        let mature = handover(&who, 300, &HashSet::new()).unwrap();
        assert_eq!(evals(&mature).len(), 1, "{}", mature.text);
        assert_eq!(evals(&mature)[0]["next_look"], 200);
        assert_eq!(evals(&mature)[0]["next_as_of"], 300);
        let skipped = already_shown(&[json!({"shown": mature.shown})]);
        assert!(evals(&handover(&who, 400, &skipped).unwrap()).is_empty());
        let mut malformed_reference = evals(&mature)[0].clone();
        malformed_reference["next_as_of"] = json!(299);
        assert!(already_shown(&[json!({"shown": [malformed_reference]})]).is_empty());

        // A successor written late is eligible by its information boundary,
        // provided the whole record was committed with its final newline.
        let mut late = next;
        late["written_unix"] = json!(10_000);
        let unterminated = format!("{}\n{}", first, late);
        std::fs::write(peer.log(), &unterminated).unwrap();
        assert!(evals(&handover(&who, 400, &HashSet::new()).unwrap()).is_empty());
        std::fs::write(peer.log(), format!("{unterminated}\n")).unwrap();
        assert_eq!(
            evals(&handover(&who, 300, &HashSet::new()).unwrap()).len(),
            1
        );
        std::fs::write(peer.scores(), score(100, 100, 300, "next").to_string()).unwrap();
        assert!(evals(&handover(&who, 400, &HashSet::new()).unwrap()).is_empty());
        std::fs::write(peer.log(), format!("{first}\n{{\"incomplete\":")).unwrap();
        assert!(handover(&who, 400, &HashSet::new()).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn provisional_and_legacy_shown_keys_do_not_suppress_a_mature_replacement() {
        let dir = temp("replacement");
        let who = reader(&dir);
        append_records(&who.peers[0].scores(), &[score(100, 100, 3_700, "1h")]).unwrap();
        let old = json!({
            "shown": [
                {"agent": "a", "kind": "look", "look": 100, "as_of": 100},
                {"agent": "a", "kind": "eval", "look": 100, "horizon": "1h", "as_of": 200},
            ]
        });
        let skip = already_shown(&[old]);
        assert_eq!(skip.len(), 1, "look deduplication is preserved");
        let replacement = handover(&who, 3_700, &skip).unwrap();
        assert_eq!(evals(&replacement).len(), 1);
        let skip = already_shown(&[json!({"shown": replacement.shown})]);
        assert!(evals(&handover(&who, 4_000, &skip).unwrap()).is_empty());
        let mut malformed = replacement.shown[0].clone();
        malformed["scored_seconds"] = json!(20);
        assert!(already_shown(&[json!({"shown": [malformed]})]).is_empty());
        let _ = std::fs::remove_dir_all(dir);
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
            ],
        )
        .unwrap();
        append_records(
            &a.scores(),
            &[
                json!({"kind": "eval", "agent": "a", "look": 100, "number": 1, "horizon": "next",
                       "horizon_seconds": 100, "decided_as_of": 105, "as_of": 205, "ended": "look",
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
