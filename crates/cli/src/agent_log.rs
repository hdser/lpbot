//! The shared record of an agent run (`docs/DESIGN.md`, "Several agents in
//! the agent mode"): who decides it, and one JSON record a look in
//! `<name>.log.jsonl`, next to the Markdown reasoning log, for other agents
//! and tools to read.
//!
//! Every record names its schema and kind and carries `as_of`, the latest
//! moment whose market its author could have seen. Agents are handed other
//! agents' records only up to their own look (`agent_peers`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::config::StrategyConfig;
use lpbot_sim::agent::Observation;
use serde_json::{json, Value};
use serde_yaml::Value as Yaml;

use crate::run_cmd::agent_file;

/// The schema every record of the shared log names.
pub const SCHEMA: &str = "lpbot.agent/1";

/// Who decides an agent run, from its strategy's `agent` entry.
#[derive(Debug, Clone, PartialEq)]
pub struct Identity {
    /// The agent's handle; its decisions file is `<name>.jsonl`.
    pub name: String,
    /// The Claude model id of the session deciding, or `person`.
    pub model: String,
    pub decisions: PathBuf,
    /// The agents whose records it is handed.
    pub peers: Vec<Peer>,
}

/// An agent whose records another reads: `stem` is its decisions file
/// without `.jsonl`, so its records are `<stem>.log.jsonl`.
#[derive(Debug, Clone, PartialEq)]
pub struct Peer {
    pub name: String,
    pub stem: PathBuf,
}

/// `<stem><suffix>`, keeping any dots in the stem.
fn with_suffix(stem: &Path, suffix: &str) -> PathBuf {
    let mut name = stem.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

impl Peer {
    pub fn log(&self) -> PathBuf {
        with_suffix(&self.stem, ".log.jsonl")
    }

    pub fn scores(&self) -> PathBuf {
        with_suffix(&self.stem, ".eval.jsonl")
    }
}

/// The decisions file without its `.jsonl`.
fn stem_of(decisions: &Path) -> PathBuf {
    match decisions.to_str().and_then(|s| s.strip_suffix(".jsonl")) {
        Some(stem) => PathBuf::from(stem),
        None => decisions.to_path_buf(),
    }
}

/// The shared log next to a decisions file: `b.jsonl` keeps it in `b.log.jsonl`.
pub fn records_path(decisions: &Path) -> PathBuf {
    with_suffix(&stem_of(decisions), ".log.jsonl")
}

/// The scores next to a decisions file: `b.jsonl` has them in `b.eval.jsonl`.
pub fn scores_path(decisions: &Path) -> PathBuf {
    with_suffix(&stem_of(decisions), ".eval.jsonl")
}

fn lookup<'a>(map: &'a BTreeMap<String, Yaml>, path: &[&str]) -> Option<&'a Yaml> {
    let (first, rest) = path.split_first()?;
    let mut value = map.get(*first)?;
    for key in rest {
        value = value.get(*key)?;
    }
    Some(value)
}

/// Who decides the run, if the strategy is decided by an agent.
pub fn identity(strategy: &StrategyConfig) -> Result<Option<Identity>> {
    let Some(file) = agent_file(strategy) else {
        return Ok(None);
    };
    let decisions = PathBuf::from(&file);
    let stem = stem_of(&decisions);
    let stem_name = stem
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let text = |key: &str| match lookup(&strategy.extra, &["agent", key]) {
        None | Some(Yaml::Null) => Ok(None),
        Some(Yaml::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => Err(anyhow!("agent.{key} must be a name, such as `b`")),
    };
    let name = text("name")?.unwrap_or_else(|| stem_name.clone());
    if name != stem_name {
        bail!(
            "agent.name is `{name}` but its decisions file is {file}; name the file `{name}.jsonl`, \
             so that its peers find its records"
        );
    }
    let model = text("model")?.unwrap_or_else(|| "unknown".into());
    let folder = stem.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut peers = Vec::new();
    match lookup(&strategy.extra, &["agent", "peers"]) {
        None | Some(Yaml::Null) => {}
        Some(Yaml::Sequence(items)) => {
            for item in items {
                let Some(text) = item.as_str().map(str::trim).filter(|t| !t.is_empty()) else {
                    bail!("agent.peers must be a list of names, such as [a], or paths");
                };
                let stem = if text.contains('/') {
                    stem_of(Path::new(text))
                } else {
                    folder.join(text)
                };
                let peer_name = stem
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(text)
                    .to_string();
                if stem == stem_of(&decisions) {
                    bail!("agent.peers names `{text}`, the agent itself; list other agents");
                }
                peers.push(Peer {
                    name: peer_name,
                    stem,
                });
            }
        }
        Some(_) => bail!("agent.peers must be a list of names, such as [a], or paths"),
    }
    Ok(Some(Identity {
        name,
        model,
        decisions,
        peers,
    }))
}

/// What the agent expects to happen until its next look.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expectation {
    /// The band of prices, in percent from now, it expects price to stay
    /// within until the next look.
    pub low_pct: Option<f64>,
    pub high_pct: Option<f64>,
    /// How sure it is that the band holds, from 0 to 1.
    pub confidence: Option<f64>,
    /// The expectation in words.
    pub text: String,
}

impl Expectation {
    /// Refuses an expectation that cannot be scored as given.
    pub fn check(&self) -> Result<()> {
        match (self.low_pct, self.high_pct) {
            (Some(low), Some(high)) if !(low.is_finite() && high.is_finite()) || low >= high => {
                bail!(
                    "--expect-low {low} and --expect-high {high}: the band runs from a lower \
                     price to a higher one, in percent from now"
                )
            }
            (Some(_), None) | (None, Some(_)) => {
                bail!("an expected band needs both --expect-low and --expect-high")
            }
            (None, None) if self.confidence.is_some() => {
                bail!("--expect-confidence is how sure the band is; give the band with --expect-low and --expect-high")
            }
            _ => {}
        }
        if let Some(p) = self.confidence {
            if !(0.0..=1.0).contains(&p) {
                bail!("--expect-confidence {p}: give it from 0 to 1");
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.low_pct.is_none() && self.text.trim().is_empty()
    }

    pub fn to_json(&self) -> Value {
        if self.is_empty() {
            return Value::Null;
        }
        json!({
            "low_pct": self.low_pct,
            "high_pct": self.high_pct,
            "confidence": self.confidence,
            "text": self.text,
        })
    }

    pub fn from_json(value: &Value) -> Option<Expectation> {
        let object = value.as_object()?;
        let number = |key: &str| object.get(key).and_then(Value::as_f64);
        Some(Expectation {
            low_pct: number("low_pct"),
            high_pct: number("high_pct"),
            confidence: number("confidence"),
            text: object
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
    }

    /// One line for people and agents.
    pub fn describe(&self) -> String {
        let band = match (self.low_pct, self.high_pct) {
            (Some(low), Some(high)) => {
                let sure = self
                    .confidence
                    .map(|p| format!(" ({:.0}% sure)", p * 100.0))
                    .unwrap_or_default();
                format!("price within {low:+.2}% to {high:+.2}% until the next look{sure}")
            }
            _ => String::new(),
        };
        match (band.is_empty(), self.text.trim()) {
            (true, text) => text.to_string(),
            (false, "") => band,
            (false, text) => format!("{band}; {text}"),
        }
    }
}

/// How a decision relates to the peers' records the look showed.
#[derive(Debug, Clone, PartialEq)]
pub struct Stance {
    /// `own`, `copy`, `adapt` or `depart`.
    pub kind: String,
    pub toward: Option<String>,
}

impl Stance {
    pub fn to_json(&self) -> Value {
        json!({ "kind": self.kind, "toward": self.toward })
    }

    pub fn describe(&self) -> String {
        match &self.toward {
            Some(peer) => format!("{} ({peer})", self.kind),
            None => self.kind.clone(),
        }
    }
}

/// Reads `--stance` and `--toward` against the peers' records the look
/// showed: a look that showed some needs a stance, and one toward a peer
/// names a peer it reads.
pub fn stance(
    kind: Option<&str>,
    toward: Option<&str>,
    shown_from: &[String],
    peers: &[Peer],
) -> Result<Option<Stance>> {
    let Some(kind) = kind else {
        if toward.is_some() {
            bail!("--toward names the peer of a --stance; give --stance own|copy|adapt|depart");
        }
        if !shown_from.is_empty() {
            bail!(
                "this look showed records of {}; say how your decision relates to them with \
                 --stance own|copy|adapt|depart (and --toward <peer>), and why in --reasoning",
                shown_from.join(", ")
            );
        }
        return Ok(None);
    };
    if !["own", "copy", "adapt", "depart"].contains(&kind) {
        bail!("--stance {kind}: use own, copy, adapt or depart");
    }
    let names: Vec<&str> = peers.iter().map(|p| p.name.as_str()).collect();
    let toward = match (kind, toward) {
        ("own", None) => None,
        ("own", Some(_)) => {
            bail!("--stance own decides without regard to the peers; leave out --toward")
        }
        (_, Some(peer)) if names.contains(&peer) => Some(peer.to_string()),
        (_, Some(peer)) => bail!(
            "--toward {peer}: the run reads {}",
            if names.is_empty() {
                "no peers".to_string()
            } else {
                names.join(", ")
            }
        ),
        (_, None) => match shown_from {
            [only] => Some(only.clone()),
            _ => bail!("--stance {kind}: say which peer with --toward <peer>"),
        },
    };
    Ok(Some(Stance {
        kind: kind.to_string(),
        toward,
    }))
}

/// The observation as numbers, as the record keeps it.
pub fn observation_json(o: &Observation) -> Value {
    let scales: Vec<Value> = o
        .scales
        .iter()
        .map(|s| {
            json!({
                "label": s.label,
                "seconds": s.seconds,
                "change_pct": s.change_pct,
                "high_pct": s.high_pct,
                "low_pct": s.low_pct,
                "vol_pct_per_day": s.vol_pct_per_day,
                "swaps": s.swaps,
                "volume_vs_30d": s.volume_vs_30d,
            })
        })
        .collect();
    let position = o.position.as_ref().map(|p| {
        json!({
            "lower_pct": p.lower_pct,
            "upper_pct": p.upper_pct,
            "state": p.state,
            "held_seconds": p.held_seconds,
            "result_pct": p.result_pct,
            "fees_pct": p.fees_pct,
            "value_share_pct": p.value_share_pct,
        })
    });
    json!({
        "scales": scales,
        "fee_pct": o.fee_pct,
        "spacing_pct": o.spacing_pct,
        "gas_gwei": o.gas_gwei,
        "round_trip_gas_pct": o.round_trip_gas_pct,
        "result_pct": o.result_pct,
        "campaigns": o.campaigns,
        "since_last_close": o.since_last_close,
        "position": position,
    })
}

/// Where a record comes from: the run and its agent.
pub struct Source<'a> {
    pub identity: &'a Identity,
    pub experiment: &'a str,
    pub run_id: &'a str,
    pub pool: &'a str,
    pub value_token: &'a str,
}

/// The first half of a look's record, written when the look is shown:
/// who, when, what it saw and which peers' records it was handed. `decide`
/// adds the decision.
pub fn look_record(
    source: &Source,
    o: &Observation,
    as_of: u64,
    seen_text: &str,
    shown: &[Value],
) -> Value {
    json!({
        "schema": SCHEMA,
        "kind": "look",
        "agent": source.identity.name,
        "model": source.identity.model,
        "experiment": source.experiment,
        "run_id": source.run_id,
        "pool": source.pool,
        "value_token": source.value_token,
        "look": o.look,
        "number": o.looks_before as u64 + 1,
        "day": o.day,
        "block": o.block,
        "as_of": as_of,
        "seen": observation_json(o),
        "seen_text": seen_text,
        "shown": shown,
    })
}

/// The decision's half of a look's record.
pub struct Decided<'a> {
    pub action: &'a str,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub next: &'a str,
    pub note: &'a str,
    pub reasoning: &'a str,
    pub expect: &'a Expectation,
    pub stance: Option<&'a Stance>,
}

/// Completes a look's record with its decision.
pub fn complete(record: &mut Value, decided: &Decided, written_unix: u64) {
    let mut decision = json!({ "action": decided.action, "next": decided.next });
    if decided.action == "enter" {
        decision["lower_pct"] = json!(decided.lower_pct);
        decision["upper_pct"] = json!(decided.upper_pct);
    }
    record["decision"] = decision;
    record["note"] = json!(decided.note);
    record["reasoning"] = json!(decided.reasoning);
    record["expect"] = decided.expect.to_json();
    record["stance"] = decided.stance.map_or(Value::Null, Stance::to_json);
    record["written_unix"] = json!(written_unix);
}

/// The agents whose records a look's record says it was handed.
pub fn shown_from(record: &Value) -> Vec<String> {
    let mut names: Vec<String> = record
        .get("shown")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("agent").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// The records of a shared log file, in order; a missing file has none.
pub fn read_records(path: &Path) -> Result<Vec<Value>> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            serde_json::from_str(line)
                .with_context(|| format!("{} line {} is not JSON", path.display(), i + 1))
        })
        .collect()
}

/// Appends records to a shared log file, one JSON object a line. The file
/// is replaced whole (written aside, synced, renamed), so a reader in
/// another process sees it before or after, never in between; and the
/// read-modify-write holds an exclusive lock on `<file>.lock`, so two
/// writers never lose each other's records.
pub fn append_records(path: &Path, records: &[Value]) -> Result<()> {
    let _writer = crate::agent_lock::exclusive(&crate::agent_lock::sibling(path, ".lock"), false)?;
    let mut content = crate::agent_lock::read_optional(path)?;
    if !content.is_empty() && !content.ends_with('\n') {
        anyhow::bail!("shared log has an unterminated final record; inspect it before appending");
    }
    for record in records {
        content.push_str(&record.to_string());
        content.push('\n');
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::agent_lock::atomic(path, content.as_bytes())
}

/// Replaces a shared log file with `records`, whole: a reader sees the old
/// records or the new ones, never none in between.
pub fn write_records(path: &Path, records: &[Value]) -> Result<()> {
    let _writer = crate::agent_lock::exclusive(&crate::agent_lock::sibling(path, ".lock"), false)?;
    let mut content = String::new();
    for record in records {
        content.push_str(&record.to_string());
        content.push('\n');
    }
    crate::agent_lock::atomic(path, content.as_bytes())
}

/// Seconds since 1970 on the wall clock: when a record was written, as
/// against `as_of`, the market time it knows.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stress_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lpbot-records-{label}-{}-{}",
            std::process::id(),
            now_unix()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[ignore = "a writer process of records_written_by_many_processes_are_all_kept"]
    fn records_writer_child() {
        let Ok(path) = std::env::var("LPBOT_STRESS_RECORDS") else {
            return;
        };
        let writer: u64 = std::env::var("LPBOT_STRESS_WRITER")
            .unwrap()
            .parse()
            .unwrap();
        for i in 0..25u64 {
            append_records(
                Path::new(&path),
                &[json!({"kind": "look", "writer": writer, "i": i, "as_of": i})],
            )
            .unwrap();
        }
    }

    #[test]
    fn records_written_by_many_processes_are_all_kept_and_reads_are_always_whole() {
        let dir = stress_dir("many");
        let path = dir.join("a.log.jsonl");
        let mut children: Vec<_> = (0..5u64)
            .map(|w| {
                crate::agent_board::tests::child(
                    "agent_log::tests::records_writer_child",
                    &[
                        ("LPBOT_STRESS_RECORDS", path.display().to_string()),
                        ("LPBOT_STRESS_WRITER", w.to_string()),
                    ],
                )
            })
            .collect();
        let mut seen = 0;
        loop {
            // Never a half file and never fewer records than before.
            let now = read_records(&path).unwrap().len();
            assert!(now >= seen, "records went from {seen} to {now}");
            seen = now;
            if children.iter_mut().all(|c| c.try_wait().unwrap().is_some()) {
                break;
            }
        }
        for mut c in children {
            assert!(c.wait().unwrap().success());
        }
        assert_eq!(read_records(&path).unwrap().len(), 5 * 25, "no record lost");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scores_replaced_while_a_peer_reads_are_never_missing() {
        let dir = stress_dir("scores");
        let path = dir.join("a.eval.jsonl");
        let scores = |n: u64| -> Vec<Value> {
            (0..20)
                .map(|i| json!({"kind": "eval", "n": n, "i": i}))
                .collect()
        };
        write_records(&path, &scores(0)).unwrap();
        let reader_path = path.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut reads = 0;
            while !stopping.load(std::sync::atomic::Ordering::Relaxed) {
                let read = read_records(&reader_path).unwrap();
                assert_eq!(read.len(), 20, "a peer saw {} scores", read.len());
                reads += 1;
            }
            reads
        });
        for n in 1..300 {
            write_records(&path, &scores(n)).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        std::fs::remove_dir_all(dir).unwrap();
    }
    use lpbot_sim::agent::{PositionView, ScaleView};

    fn strategy(agent: &str) -> StrategyConfig {
        let yaml = format!("id: s\nkind: zone_round_trip\nvalue_token: USDC\nagent: {agent}\n");
        serde_yaml::from_str(&yaml).unwrap()
    }

    #[test]
    fn an_agent_is_named_after_its_decisions_file_and_finds_its_peers() {
        let s = strategy(
            "{ decisions: data/agent/panel/b.jsonl, model: claude-sonnet-5-5, \
             peers: [a, data/agent/older/c.jsonl] }",
        );
        let who = identity(&s).unwrap().unwrap();
        assert_eq!(who.name, "b");
        assert_eq!(who.model, "claude-sonnet-5-5");
        assert_eq!(who.peers.len(), 2);
        assert_eq!(who.peers[0].name, "a");
        assert_eq!(
            who.peers[0].log(),
            PathBuf::from("data/agent/panel/a.log.jsonl")
        );
        assert_eq!(
            who.peers[1].scores(),
            PathBuf::from("data/agent/older/c.eval.jsonl")
        );
        assert_eq!(
            records_path(&who.decisions),
            PathBuf::from("data/agent/panel/b.log.jsonl")
        );

        let unnamed = identity(&strategy("{ decisions: data/agent/run.v2.jsonl }"))
            .unwrap()
            .unwrap();
        assert_eq!(
            (unnamed.name.as_str(), unnamed.model.as_str()),
            ("run.v2", "unknown")
        );
        assert!(unnamed.peers.is_empty());
        assert_eq!(
            scores_path(&unnamed.decisions),
            PathBuf::from("data/agent/run.v2.eval.jsonl")
        );

        assert!(identity(&strategy("{ decisions: null }"))
            .unwrap()
            .is_none());
        let wrong = identity(&strategy("{ decisions: data/agent/x.jsonl, name: b }"));
        assert!(wrong
            .unwrap_err()
            .to_string()
            .contains("name the file `b.jsonl`"));
        let itself = identity(&strategy("{ decisions: data/agent/p/b.jsonl, peers: [b] }"));
        assert!(itself.unwrap_err().to_string().contains("the agent itself"));
        assert!(identity(&strategy("{ decisions: data/agent/p/b.jsonl, peers: a }")).is_err());
    }

    #[test]
    fn an_expectation_must_be_scorable_as_given() {
        let band = Expectation {
            low_pct: Some(-2.0),
            high_pct: Some(1.5),
            confidence: Some(0.7),
            text: "chop".into(),
        };
        band.check().unwrap();
        assert_eq!(
            band.describe(),
            "price within -2.00% to +1.50% until the next look (70% sure); chop"
        );
        assert_eq!(Expectation::from_json(&band.to_json()), Some(band.clone()));
        let upside_down = Expectation {
            low_pct: Some(1.0),
            high_pct: Some(-1.0),
            ..band.clone()
        };
        assert!(upside_down.check().is_err());
        let half = Expectation {
            high_pct: None,
            ..band.clone()
        };
        assert!(half.check().is_err());
        let too_sure = Expectation {
            confidence: Some(1.5),
            ..band.clone()
        };
        assert!(too_sure.check().is_err());
        let sure_of_nothing = Expectation {
            low_pct: None,
            high_pct: None,
            ..band
        };
        assert!(sure_of_nothing.check().is_err());
        let none = Expectation::default();
        none.check().unwrap();
        assert_eq!(none.to_json(), Value::Null);
        let words = Expectation {
            text: "a slow drift down".into(),
            ..none
        };
        assert_eq!(words.describe(), "a slow drift down");
    }

    #[test]
    fn a_look_that_showed_peers_needs_a_stance() {
        let peers = vec![
            Peer {
                name: "a".into(),
                stem: "p/a".into(),
            },
            Peer {
                name: "c".into(),
                stem: "p/c".into(),
            },
        ];
        let shown = vec!["a".to_string()];
        assert!(stance(None, None, &[], &peers).unwrap().is_none());
        let err = stance(None, None, &shown, &peers).unwrap_err().to_string();
        assert!(
            err.contains("records of a") && err.contains("--stance"),
            "{err}"
        );
        let adapt = stance(Some("adapt"), None, &shown, &peers)
            .unwrap()
            .unwrap();
        assert_eq!(adapt.toward.as_deref(), Some("a"), "the one peer shown");
        assert_eq!(adapt.describe(), "adapt (a)");
        let depart = stance(Some("depart"), Some("c"), &shown, &peers)
            .unwrap()
            .unwrap();
        assert_eq!(depart.toward.as_deref(), Some("c"));
        let two = vec!["a".to_string(), "c".to_string()];
        assert!(stance(Some("copy"), None, &two, &peers).is_err());
        assert!(stance(Some("copy"), Some("z"), &shown, &peers).is_err());
        assert!(stance(Some("own"), Some("a"), &shown, &peers).is_err());
        assert!(stance(Some("follow"), None, &shown, &peers).is_err());
        assert!(stance(None, Some("a"), &shown, &peers).is_err());
        assert_eq!(
            stance(Some("own"), None, &shown, &peers)
                .unwrap()
                .unwrap()
                .toward,
            None
        );
    }

    #[test]
    fn a_look_record_is_written_in_two_halves_and_reads_back() {
        let who = Identity {
            name: "b".into(),
            model: "claude-sonnet-5-5".into(),
            decisions: "p/b.jsonl".into(),
            peers: Vec::new(),
        };
        let source = Source {
            identity: &who,
            experiment: "panel",
            run_id: "panel-001-abcd",
            pool: "USDC/WETH 0.05",
            value_token: "USDC",
        };
        let o = Observation {
            look: 1_000,
            looks_before: 4,
            day: 1.25,
            block: 77,
            time: 1_012,
            scales: vec![ScaleView {
                label: "1h",
                seconds: 3_600,
                change_pct: -1.0,
                high_pct: 0.5,
                low_pct: -1.2,
                vol_pct_per_day: None,
                swaps: 10,
                volume_vs_30d: Some(1.5),
            }],
            fee_pct: 0.05,
            spacing_pct: 0.1,
            gas_gwei: 0.3,
            round_trip_gas_pct: 0.01,
            result_pct: 0.2,
            campaigns: 1,
            since_last_close: None,
            position: Some(PositionView {
                lower_pct: -2.0,
                upper_pct: -1.0,
                state: "resting".into(),
                held_seconds: 600,
                result_pct: 0.0,
                fees_pct: 0.0,
                value_share_pct: 100.0,
            }),
        };
        let shown = vec![json!({"agent": "a", "kind": "look", "look": 900, "as_of": 905})];
        let mut record = look_record(&source, &o, 1_004, "Look 5", &shown);
        assert_eq!(record["schema"], SCHEMA);
        assert_eq!(record["number"], 5);
        assert_eq!(record["as_of"], 1_004);
        assert_eq!(record["seen"]["scales"][0]["vol_pct_per_day"], Value::Null);
        assert_eq!(record["seen"]["position"]["state"], "resting");
        assert_eq!(shown_from(&record), vec!["a".to_string()]);
        let expect = Expectation {
            low_pct: Some(-1.0),
            high_pct: Some(1.0),
            confidence: Some(0.6),
            text: String::new(),
        };
        let stance = Stance {
            kind: "adapt".into(),
            toward: Some("a".into()),
        };
        complete(
            &mut record,
            &Decided {
                action: "enter",
                lower_pct: Some(-3.0),
                upper_pct: Some(-1.0),
                next: "4h",
                note: " after a fall ",
                reasoning: "why",
                expect: &expect,
                stance: Some(&stance),
            },
            42,
        );
        assert_eq!(record["decision"]["lower_pct"], -3.0);
        assert_eq!(record["note"], " after a fall ");
        assert_eq!(record["stance"]["toward"], "a");
        assert_eq!(record["expect"]["confidence"], 0.6);

        let dir = std::env::temp_dir().join(format!("lpbot-agent-records-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("b.log.jsonl");
        assert!(read_records(&path).unwrap().is_empty());
        append_records(&path, std::slice::from_ref(&record)).unwrap();
        append_records(&path, &[json!({"kind": "look", "as_of": 2_000})]).unwrap();
        let back = read_records(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], record);
        let uncommitted = r#"{"kind":"look","as_of":2000}"#;
        std::fs::write(&path, uncommitted).unwrap();
        assert!(append_records(&path, &[json!({"kind":"look","as_of":3000})]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), uncommitted);
        std::fs::write(&path, "{\"ok\": 1}\nnot json\n").unwrap();
        let err = read_records(&path).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
