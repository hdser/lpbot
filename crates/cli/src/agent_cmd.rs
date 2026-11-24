//! `lpbot agent next` and `lpbot agent decide`: a run decided one look at
//! a time by a person or a session (the agent mode of `lpbot_sim::agent`).
//!
//! `next` replays the run from its decisions up to the first look without
//! one and shows how the pool looks there; `decide` writes the decision for
//! that look and shows the next one. The decisions file is the run: `lpbot
//! run` on the same experiment writes its six tables.
//!
//! `decide` also appends to the run's reasoning log, next to the decisions
//! file: what the look showed, what was decided and why. The run never
//! reads it; it is there for whoever decides, and for reading the run
//! afterwards. The same goes, one JSON record a look, to the shared log
//! (`agent_log`) that other agents are handed (`agent_peers`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::experiment::ResolvedRun;
use lpbot_sim::agent::Observation;

use crate::agent_log::{self, Expectation, Identity, Source, Stance};
use crate::agent_peers;
use crate::run_cmd::{agent_file, prepare_one};

/// What a look asks of the agent, as it is written next to the decisions.
fn pending_path(decisions: &Path) -> PathBuf {
    let mut name = decisions.as_os_str().to_owned();
    name.push(".pending");
    PathBuf::from(name)
}

/// The reasoning log next to the decisions file: `my-run.jsonl` keeps its
/// reasoning in `my-run.log.md`.
pub fn log_path(decisions: &Path) -> PathBuf {
    decisions.with_extension("log.md")
}

/// The look waiting for a decision: its time (the key of the decisions
/// file), its number and day in the run, the text it showed, and the first
/// half of its shared record (`agent_log::look_record`).
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub look: u64,
    pub number: Option<u64>,
    pub day: Option<f64>,
    pub seen: Option<String>,
    pub record: Option<serde_json::Value>,
}

impl Pending {
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "look": self.look,
            "number": self.number,
            "day": self.day,
            "seen": self.seen,
            "record": self.record,
        })
        .to_string()
    }

    /// Reads a pending file; one written before the log existed holds only
    /// the look's time.
    pub fn parse(text: &str) -> Option<Pending> {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        Some(Pending {
            look: v.get("look")?.as_u64()?,
            number: v.get("number").and_then(serde_json::Value::as_u64),
            day: v.get("day").and_then(serde_json::Value::as_f64),
            seen: v
                .get("seen")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            record: v.get("record").filter(|r| r.is_object()).cloned(),
        })
    }

    /// The look as the agent knows it: by number and day, never by time.
    fn label(&self) -> String {
        match (self.number, self.day) {
            (Some(n), Some(d)) => format!("Look {n}, day {d:.2}"),
            _ => "Look (number not recorded)".into(),
        }
    }
}

/// The log's entry for one decision, in Markdown.
pub fn log_entry(pending: &Pending, decision: &Decision, stance: Option<&Stance>) -> String {
    let mut text = String::new();
    let action = match (decision.action, decision.lower_pct, decision.upper_pct) {
        ("enter", Some(lower), Some(upper)) => format!("enter {lower:+.2}% to {upper:+.2}%"),
        (other, _, _) => other.to_string(),
    };
    let _ = writeln!(
        text,
        "## {}: {action}, next look in {}\n",
        pending.label(),
        decision.next
    );
    if let Some(seen) = &pending.seen {
        let _ = writeln!(text, "```text\n{}\n```\n", seen.trim_end());
    }
    if !decision.note.is_empty() {
        let _ = writeln!(text, "Note: {}\n", decision.note.trim());
    }
    if !decision.expect.is_empty() {
        let _ = writeln!(text, "Expected: {}.\n", decision.expect.describe());
    }
    if let Some(stance) = stance {
        let _ = writeln!(text, "Stance toward the peers: {}.\n", stance.describe());
    }
    let reasoning = decision.reasoning.trim();
    if reasoning.is_empty() {
        let _ = writeln!(text, "(No reasoning written.)\n");
    } else {
        let _ = writeln!(text, "{reasoning}\n");
    }
    text
}

/// Appends an entry to the log, starting the file with a header naming the
/// agent and its model when it is new.
pub fn append_log(log: &Path, who: &Identity, entry: &str) -> Result<()> {
    let mut content = std::fs::read_to_string(log).unwrap_or_default();
    if content.is_empty() {
        let _ = writeln!(
            content,
            "# Reasoning log of agent {} ({})\n\nDecisions: {}. One entry a decision: what the \
             look showed, what was decided and why; the same, one JSON record a look, is in {} \
             for other agents and tools. The run does not read either. Local only: keep them \
             under data/, which is not committed.\n",
            who.name,
            who.model,
            who.decisions.display(),
            agent_log::records_path(&who.decisions).display()
        );
    }
    content.push_str(entry);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(log, content).with_context(|| format!("writing {}", log.display()))
}

/// Who decides the run: its agent entry, with its decisions file.
pub fn identity_of(run: &ResolvedRun) -> Result<Identity> {
    let strategy = run
        .config
        .strategies
        .first()
        .ok_or_else(|| anyhow!("the run has no strategy"))?;
    if agent_file(strategy).is_none() {
        bail!(
            "the experiment's strategy has no agent.decisions file; set it, such as \
             `agent.decisions: [data/agent/my-run.jsonl]` under `sweep`"
        );
    }
    agent_log::identity(strategy)?.ok_or_else(|| anyhow!("the run has no agent.decisions file"))
}

fn read_pending(decisions: &Path) -> Result<Pending> {
    let pending = pending_path(decisions);
    match std::fs::read_to_string(&pending) {
        Ok(text) => Pending::parse(&text).ok_or_else(|| {
            anyhow!(
                "{} is damaged; run `lpbot agent next` again",
                pending.display()
            )
        }),
        Err(_) => bail!(
            "no look is waiting for a decision; run `lpbot agent next <experiment>` first to see it"
        ),
    }
}

/// Replays the run to its first look without a decision and shows it.
/// `None` once the window is over.
pub fn next(run: &ResolvedRun, root: &Path, chain: &str) -> Result<Option<Observation>> {
    let who = identity_of(run)?;
    let decisions = who.decisions.clone();
    let prepared = prepare_one(run, root, chain)?;
    let out = prepared.replay(&prepared.settings)?;
    let pool_name = prepared.pool_name.clone();
    if out.unused_decisions > 0 {
        println!(
            "note: {} decision(s) at the end of {} answer looks the run did not ask for (the \
             settings or the history changed since); they are left out",
            out.unused_decisions,
            decisions.display()
        );
    }
    let strategy = &run.config.strategies[0];
    let pending = pending_path(&decisions);
    match out.pending {
        Some(observation) => {
            if let Some(parent) = pending.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let seen = describe(&observation, &pool_name, &strategy.value_token);
            // What the look knows: the market up to the block it is shown at.
            let as_of = prepared
                .block_time(observation.block)
                .unwrap_or(observation.look);
            let own = agent_log::read_records(&agent_log::records_path(&decisions))?;
            let handover = agent_peers::handover(&who, as_of, &agent_peers::already_shown(&own))?;
            let source = Source {
                identity: &who,
                experiment: &run.experiment,
                run_id: &run.run_id,
                pool: &pool_name,
                value_token: &strategy.value_token,
            };
            let record =
                agent_log::look_record(&source, &observation, as_of, &seen, &handover.shown);
            let waiting = Pending {
                look: observation.look,
                number: Some(observation.looks_before as u64 + 1),
                day: Some(observation.day),
                seen: Some(seen.clone()),
                record: Some(record),
            };
            std::fs::write(&pending, format!("{}\n", waiting.to_json()))
                .with_context(|| format!("writing {}", pending.display()))?;
            print!("{seen}");
            if !who.peers.is_empty() {
                println!();
                print!("{}", handover.text);
            }
            println!();
            println!(
                "Decide with: lpbot agent decide <experiment> --action stay_out|hold|exit|enter \
                 [--lower <pct> --upper <pct>] --next <duration> --note \"why, in a sentence\" \
                 --reasoning \"what you saw and weighed\" [--expect-low <pct> --expect-high <pct> \
                 --expect-confidence <0-1>] [--expect \"what you expect\"]"
            );
            if !handover.shown.is_empty() {
                println!(
                    "This look showed your peers' records: add --stance own|copy|adapt|depart \
                     [--toward <peer>]."
                );
            }
            println!(
                "Agent {} ({}). Your reasoning so far: {}",
                who.name,
                who.model,
                log_path(&decisions).display()
            );
            Ok(Some(observation))
        }
        None => {
            let _ = std::fs::remove_file(&pending);
            let s = &out.summary;
            println!(
                "The window is over: {:+.2}% in {} ({:+.2}% in dollars; holding it made \
                 {:+.2}% in dollars), {} campaign(s), fees {:.4}, gas {:.4}. `lpbot run` on the \
                 experiment writes the tables.",
                s.result_pct,
                strategy.value_token,
                s.result_pct_usd,
                s.hold_value_pct_usd,
                s.campaigns,
                s.fees,
                s.gas
            );
            Ok(None)
        }
    }
}

/// A decision for the look `lpbot agent next` last showed.
#[derive(Clone)]
pub struct Decision<'a> {
    pub action: &'a str,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub next: &'a str,
    pub note: &'a str,
    /// The reasoning, as long as it needs to be; it goes to the logs only.
    pub reasoning: &'a str,
    /// What the agent expects until its next look; it goes to the logs only.
    pub expect: Expectation,
    /// How the decision relates to the peers' records the look showed:
    /// `own`, `copy`, `adapt` or `depart`, and toward which peer.
    pub stance: Option<&'a str>,
    pub toward: Option<&'a str>,
}

/// The decision's line in the decisions file.
pub fn decision_line(look: u64, decision: &Decision) -> Result<String> {
    if crate::run_cmd::parse_duration(decision.next).is_none() {
        bail!(
            "--next {} is not a duration such as 30m, 4h or 2d",
            decision.next
        );
    }
    let mut row = serde_json::json!({
        "look": look,
        "action": decision.action,
        "next": decision.next,
        "note": decision.note,
    });
    match decision.action {
        "enter" => {
            let (Some(lower), Some(upper)) = (decision.lower_pct, decision.upper_pct) else {
                bail!("an entry needs --lower and --upper, in percent from the price now");
            };
            row["lower_pct"] = serde_json::json!(lower);
            row["upper_pct"] = serde_json::json!(upper);
        }
        "stay_out" | "hold" | "exit" => {}
        other => bail!("--action {other}: use stay_out, hold, exit or enter"),
    }
    // The line must read back as it was meant.
    crate::run_cmd::parse_decisions(&row.to_string())?;
    Ok(row.to_string())
}

/// Writes the decision for the pending look, adds it to the reasoning log
/// and shows the next look.
pub fn decide(run: &ResolvedRun, root: &Path, chain: &str, decision: &Decision) -> Result<()> {
    let who = identity_of(run)?;
    let decisions = who.decisions.clone();
    let pending = pending_path(&decisions);
    let waiting = read_pending(&decisions)?;
    let look = waiting.look;
    // Everything is checked before anything is written.
    let line = decision_line(look, decision)?;
    decision.expect.check()?;
    let shown_from = waiting
        .record
        .as_ref()
        .map(agent_log::shown_from)
        .unwrap_or_default();
    let stance = agent_log::stance(decision.stance, decision.toward, &shown_from, &who.peers)?;
    if let Some(parent) = decisions.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = std::fs::read_to_string(&decisions).unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(&line);
    content.push('\n');
    std::fs::write(&decisions, content)
        .with_context(|| format!("writing {}", decisions.display()))?;
    std::fs::remove_file(&pending).ok();
    let log = log_path(&decisions);
    append_log(&log, &who, &log_entry(&waiting, decision, stance.as_ref())).with_context(|| {
        format!(
            "the decision is written to {}, but not to the reasoning log",
            decisions.display()
        )
    })?;
    if let Some(mut record) = waiting.record.clone() {
        agent_log::complete(
            &mut record,
            &agent_log::Decided {
                action: decision.action,
                lower_pct: decision.lower_pct,
                upper_pct: decision.upper_pct,
                next: decision.next,
                note: decision.note,
                reasoning: decision.reasoning,
                expect: &decision.expect,
                stance: stance.as_ref(),
            },
            agent_log::now_unix(),
        );
        let records = agent_log::records_path(&decisions);
        agent_log::append_records(&records, &[record]).with_context(|| {
            format!(
                "the decision is written to {}, but not to the shared log {}",
                decisions.display(),
                records.display()
            )
        })?;
    }
    println!("{}: {}", waiting.label(), decision.action);
    println!();
    next(run, root, chain).map(|_| ())
}

/// Shows again every record of the run's peers up to the look waiting for
/// a decision.
pub fn peers(run: &ResolvedRun) -> Result<()> {
    let who = identity_of(run)?;
    if who.peers.is_empty() {
        println!(
            "Agent {} reads no peers: its entry names none in agent.peers.",
            who.name
        );
        return Ok(());
    }
    let waiting = read_pending(&who.decisions)?;
    let as_of = waiting
        .record
        .as_ref()
        .and_then(|r| r.get("as_of"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            anyhow!("run `lpbot agent next` again: the waiting look predates the shared log")
        })?;
    let handover = agent_peers::handover(&who, as_of, &Default::default())?;
    println!("{}", waiting.label());
    print!("{}", handover.text);
    Ok(())
}

fn hours(seconds: u64) -> String {
    let h = seconds as f64 / 3_600.0;
    if h < 48.0 {
        format!("{h:.1} h")
    } else {
        format!("{:.1} d", h / 24.0)
    }
}

/// The observation as text for the agent: what it sees and nothing else.
pub fn describe(o: &Observation, pool: &str, value_token: &str) -> String {
    let mut text = String::new();
    let _ = writeln!(
        text,
        "Look {}, day {:.2} of the run, on {pool}. Prices are the volatile token's, in percent from now.",
        o.looks_before + 1,
        o.day
    );
    let _ = writeln!(
        text,
        "Fee {:.2}%, one tick spacing {:.3}%. Gas {:.3} gwei: entering and leaving cost {:.3}% of the run.",
        o.fee_pct, o.spacing_pct, o.gas_gwei, o.round_trip_gas_pct
    );
    let _ = writeln!(
        text,
        "Run so far: {:+.2}% in {value_token}, {} campaign(s){}.",
        o.result_pct,
        o.campaigns,
        o.since_last_close
            .map(|s| format!(", the last ended {} ago", hours(s)))
            .unwrap_or_default()
    );
    match &o.position {
        Some(p) => {
            let _ = writeln!(
                text,
                "Position: {:+.2}% to {:+.2}%, {}, held {}, result {:+.3}%, fees {:+.3}%, {:.0}% in {value_token}.",
                p.lower_pct,
                p.upper_pct,
                p.state,
                hours(p.held_seconds),
                p.result_pct,
                p.fees_pct,
                p.value_share_pct
            );
        }
        None => {
            let _ = writeln!(text, "Position: none.");
        }
    }
    let _ = writeln!(
        text,
        "{:>5} {:>8} {:>8} {:>8} {:>10} {:>7} {:>9}",
        "scale", "change", "high", "low", "vol/day", "swaps", "volume"
    );
    for s in &o.scales {
        let _ = writeln!(
            text,
            "{:>5} {:>+8.2} {:>+8.2} {:>+8.2} {:>10} {:>7} {:>9}",
            s.label,
            s.change_pct,
            s.high_pct,
            s.low_pct,
            s.vol_pct_per_day
                .map(|v| format!("{v:.2}"))
                .unwrap_or_else(|| "-".into()),
            s.swaps,
            s.volume_vs_30d
                .map(|v| format!("{v:.2}x"))
                .unwrap_or_else(|| "-".into())
        );
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpbot_sim::agent::{PositionView, ScaleView};

    #[test]
    fn a_decision_line_reads_back_and_bad_ones_are_refused() {
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-3.0),
            upper_pct: Some(-1.0),
            next: "4h",
            note: "after a 4% fall",
            reasoning: "",
            expect: Expectation::default(),
            stance: None,
            toward: None,
        };
        let line = decision_line(1_800_000_000, &enter).unwrap();
        let back = crate::run_cmd::parse_decisions(&line).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].look, 1_800_000_000);
        assert_eq!(back[0].next_look, 4 * 3_600);
        assert_eq!(back[0].note, "after a 4% fall");
        let missing = Decision {
            lower_pct: None,
            ..enter.clone()
        };
        assert!(decision_line(1, &missing).is_err());
        let wrong = Decision {
            action: "buy",
            ..enter.clone()
        };
        assert!(decision_line(1, &wrong).is_err());
        let late = Decision {
            next: "soon",
            ..enter.clone()
        };
        assert!(decision_line(1, &late).is_err());
        assert!(
            !line.contains("reasoning"),
            "the reasoning stays out of the decisions file: {line}"
        );
    }

    #[test]
    fn the_log_sits_next_to_the_decisions_file() {
        assert_eq!(
            log_path(Path::new("data/agent/usdc-weth-2026-07.jsonl")),
            PathBuf::from("data/agent/usdc-weth-2026-07.log.md")
        );
        assert_eq!(
            log_path(Path::new("data/agent/run")),
            PathBuf::from("data/agent/run.log.md")
        );
    }

    #[test]
    fn a_pending_look_reads_back_and_an_old_one_still_reads() {
        let p = Pending {
            look: 1_800_000_000,
            number: Some(12),
            day: Some(3.25),
            seen: Some("Look 12, day 3.25\n  1h  -1.00\n".into()),
            record: Some(serde_json::json!({"kind": "look", "as_of": 1_800_000_004u64})),
        };
        assert_eq!(Pending::parse(&p.to_json()), Some(p));
        let old = Pending::parse("{\"look\": 7}\n").unwrap();
        assert_eq!(old.look, 7);
        assert_eq!((old.number, old.day, old.seen), (None, None, None));
        assert_eq!(old.record, None);
        assert!(Pending::parse("{\"number\": 3}").is_none());
        assert!(Pending::parse("not json").is_none());
    }

    #[test]
    fn a_log_entry_shows_what_was_seen_what_was_done_and_why() {
        let p = Pending {
            look: 1_800_000_000,
            number: Some(12),
            day: Some(3.25),
            seen: Some("Look 12, day 3.25, on USDC/WETH 0.05.\n".into()),
            record: None,
        };
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-3.0),
            upper_pct: Some(1.5),
            next: "1d",
            note: "quiet week",
            reasoning: "Daily volatility is 1.8%, well under the band.\n\nCentred, a little low.",
            expect: Expectation {
                low_pct: Some(-2.0),
                high_pct: Some(2.0),
                confidence: Some(0.65),
                text: String::new(),
            },
            stance: Some("adapt"),
            toward: None,
        };
        let stance = Stance {
            kind: "adapt".into(),
            toward: Some("a".into()),
        };
        let entry = log_entry(&p, &enter, Some(&stance));
        assert!(
            entry.starts_with("## Look 12, day 3.25: enter -3.00% to +1.50%, next look in 1d\n"),
            "{entry}"
        );
        assert!(
            entry.contains("```text\nLook 12, day 3.25, on USDC/WETH 0.05.\n```"),
            "{entry}"
        );
        assert!(entry.contains("Note: quiet week"), "{entry}");
        assert!(
            entry.contains(
                "Expected: price within -2.00% to +2.00% until the next look (65% sure)."
            ),
            "{entry}"
        );
        assert!(
            entry.contains("Stance toward the peers: adapt (a)."),
            "{entry}"
        );
        assert!(
            entry.contains("1.8%, well under the band.\n\nCentred"),
            "{entry}"
        );
        assert!(
            !entry.contains("1800000000"),
            "the look's time is not shown: {entry}"
        );

        let bare = Decision {
            action: "hold",
            lower_pct: None,
            upper_pct: None,
            note: "",
            reasoning: "  ",
            expect: Expectation::default(),
            ..enter
        };
        let old = Pending {
            number: None,
            day: None,
            seen: None,
            ..p
        };
        let entry = log_entry(&old, &bare, None);
        assert!(
            entry.starts_with("## Look (number not recorded): hold, next look in 1d"),
            "{entry}"
        );
        assert!(entry.contains("(No reasoning written.)"), "{entry}");
        assert!(
            !entry.contains("Note:")
                && !entry.contains("```")
                && !entry.contains("Expected:")
                && !entry.contains("Stance"),
            "{entry}"
        );
    }

    #[test]
    fn the_log_starts_with_a_header_and_grows_entry_by_entry() {
        let dir = std::env::temp_dir().join(format!("lpbot-agent-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let who = Identity {
            name: "run".into(),
            model: "claude-opus-5-5".into(),
            decisions: dir.join("agent/run.jsonl"),
            peers: Vec::new(),
        };
        let log = log_path(&who.decisions);
        append_log(&log, &who, "## Look 1\n\nfirst\n\n").unwrap();
        append_log(&log, &who, "## Look 2\n\nsecond\n\n").unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(
            text.starts_with("# Reasoning log of agent run (claude-opus-5-5)"),
            "{text}"
        );
        assert!(text.contains("run.log.jsonl"), "{text}");
        assert_eq!(text.matches("# Reasoning log").count(), 1, "{text}");
        let first = text.find("## Look 1").unwrap();
        let second = text.find("## Look 2").unwrap();
        assert!(first < second, "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_observation_reads_as_text_with_every_scale() {
        let o = Observation {
            look: 42,
            looks_before: 2,
            day: 1.5,
            block: 7,
            scales: vec![ScaleView {
                label: "1h",
                seconds: 3_600,
                change_pct: -1.25,
                high_pct: 0.5,
                low_pct: -0.25,
                vol_pct_per_day: Some(3.0),
                swaps: 120,
                volume_vs_30d: Some(1.8),
            }],
            fee_pct: 0.05,
            spacing_pct: 0.1,
            gas_gwei: 0.2,
            round_trip_gas_pct: 0.004,
            result_pct: 1.0,
            campaigns: 3,
            since_last_close: Some(7_200),
            position: Some(PositionView {
                lower_pct: -2.0,
                upper_pct: -0.5,
                state: "in_range".into(),
                held_seconds: 3_600,
                result_pct: 0.1,
                fees_pct: 0.2,
                value_share_pct: 60.0,
            }),
        };
        let text = describe(&o, "USDC/WETH 0.05", "USDC");
        assert!(text.contains("Look 3, day 1.50"), "{text}");
        assert!(!text.contains("42"), "the look's time is not shown: {text}");
        assert!(text.contains("-2.00% to -0.50%, in_range"), "{text}");
        assert!(text.contains("   1h    -1.25"), "{text}");
        assert!(text.contains("1.80x"), "{text}");
        assert!(text.contains("the last ended 2.0 h ago"), "{text}");
    }
}
