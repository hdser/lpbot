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
//! afterwards.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::experiment::ResolvedRun;
use lpbot_sim::agent::Observation;

use crate::run_cmd::{agent_file, simulate_one};

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
/// file), its number and day in the run, and the text it showed.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub look: u64,
    pub number: Option<u64>,
    pub day: Option<f64>,
    pub seen: Option<String>,
}

impl Pending {
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "look": self.look,
            "number": self.number,
            "day": self.day,
            "seen": self.seen,
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
pub fn log_entry(pending: &Pending, decision: &Decision) -> String {
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
    let reasoning = decision.reasoning.trim();
    if reasoning.is_empty() {
        let _ = writeln!(text, "(No reasoning written.)\n");
    } else {
        let _ = writeln!(text, "{reasoning}\n");
    }
    text
}

/// Appends an entry to the log, starting the file with a header when it is new.
pub fn append_log(log: &Path, decisions: &Path, entry: &str) -> Result<()> {
    let mut content = std::fs::read_to_string(log).unwrap_or_default();
    if content.is_empty() {
        let _ = writeln!(
            content,
            "# Reasoning log for {}\n\nOne entry a decision: what the look showed, what was \
             decided and why. The run does not read it. Local only: keep it under data/, \
             which is not committed.\n",
            decisions.display()
        );
    }
    content.push_str(entry);
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(log, content).with_context(|| format!("writing {}", log.display()))
}

fn decisions_path(run: &ResolvedRun) -> Result<PathBuf> {
    let strategy = run
        .config
        .strategies
        .first()
        .ok_or_else(|| anyhow!("the run has no strategy"))?;
    agent_file(strategy).map(PathBuf::from).ok_or_else(|| {
        anyhow!(
            "the experiment's strategy has no agent.decisions file; set it, such as \
             `agent.decisions: [data/agent/my-run.jsonl]` under `sweep`"
        )
    })
}

/// Replays the run to its first look without a decision and shows it.
/// `None` once the window is over.
pub fn next(run: &ResolvedRun, root: &Path, chain: &str) -> Result<Option<Observation>> {
    let decisions = decisions_path(run)?;
    let (out, pool_name) = simulate_one(run, root, chain)?;
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
            let waiting = Pending {
                look: observation.look,
                number: Some(observation.looks_before as u64 + 1),
                day: Some(observation.day),
                seen: Some(seen.clone()),
            };
            std::fs::write(&pending, format!("{}\n", waiting.to_json()))
                .with_context(|| format!("writing {}", pending.display()))?;
            print!("{seen}");
            println!();
            println!(
                "Decide with: lpbot agent decide <experiment> --action stay_out|hold|exit|enter \
                 [--lower <pct> --upper <pct>] --next <duration> --note \"why, in a sentence\" \
                 --reasoning \"what you saw and weighed\""
            );
            println!("Your reasoning so far: {}", log_path(&decisions).display());
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
pub struct Decision<'a> {
    pub action: &'a str,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub next: &'a str,
    pub note: &'a str,
    /// The reasoning, as long as it needs to be; it goes to the log only.
    pub reasoning: &'a str,
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
    let decisions = decisions_path(run)?;
    let pending = pending_path(&decisions);
    let waiting = match std::fs::read_to_string(&pending) {
        Ok(text) => Pending::parse(&text).ok_or_else(|| {
            anyhow!(
                "{} is damaged; run `lpbot agent next` again",
                pending.display()
            )
        })?,
        Err(_) => bail!(
            "no look is waiting for a decision; run `lpbot agent next <experiment>` first to see it"
        ),
    };
    let look = waiting.look;
    let line = decision_line(look, decision)?;
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
    append_log(&log, &decisions, &log_entry(&waiting, decision)).with_context(|| {
        format!(
            "the decision is written to {}, but not to the reasoning log",
            decisions.display()
        )
    })?;
    println!("{}: {}", waiting.label(), decision.action);
    println!();
    next(run, root, chain).map(|_| ())
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
        };
        let line = decision_line(1_800_000_000, &enter).unwrap();
        let back = crate::run_cmd::parse_decisions(&line).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].look, 1_800_000_000);
        assert_eq!(back[0].next_look, 4 * 3_600);
        assert_eq!(back[0].note, "after a 4% fall");
        let missing = Decision {
            lower_pct: None,
            ..enter
        };
        assert!(decision_line(1, &missing).is_err());
        let wrong = Decision {
            action: "buy",
            ..enter
        };
        assert!(decision_line(1, &wrong).is_err());
        let late = Decision {
            next: "soon",
            ..enter
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
        };
        assert_eq!(Pending::parse(&p.to_json()), Some(p));
        let old = Pending::parse("{\"look\": 7}\n").unwrap();
        assert_eq!(old.look, 7);
        assert_eq!((old.number, old.day, old.seen), (None, None, None));
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
        };
        let enter = Decision {
            action: "enter",
            lower_pct: Some(-3.0),
            upper_pct: Some(1.5),
            next: "1d",
            note: "quiet week",
            reasoning: "Daily volatility is 1.8%, well under the band.\n\nCentred, a little low.",
        };
        let entry = log_entry(&p, &enter);
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
            ..enter
        };
        let old = Pending {
            number: None,
            day: None,
            seen: None,
            ..p
        };
        let entry = log_entry(&old, &bare);
        assert!(
            entry.starts_with("## Look (number not recorded): hold, next look in 1d"),
            "{entry}"
        );
        assert!(entry.contains("(No reasoning written.)"), "{entry}");
        assert!(
            !entry.contains("Note:") && !entry.contains("```"),
            "{entry}"
        );
    }

    #[test]
    fn the_log_starts_with_a_header_and_grows_entry_by_entry() {
        let dir = std::env::temp_dir().join(format!("lpbot-agent-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let decisions = dir.join("agent/run.jsonl");
        let log = log_path(&decisions);
        append_log(&log, &decisions, "## Look 1\n\nfirst\n\n").unwrap();
        append_log(&log, &decisions, "## Look 2\n\nsecond\n\n").unwrap();
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.starts_with("# Reasoning log for "), "{text}");
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
