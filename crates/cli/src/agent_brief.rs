//! `lpbot agent brief`: what a session acting as one agent of a panel is
//! told before its first look: who it is, the commands, what its decisions
//! mean and the rules that keep the test honest. It names no dates.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use lpbot_core::experiment::ResolvedRun;

use crate::agent_log::Identity;
use crate::run_cmd::run_window;

/// The brief for agent `who` of the experiment at `experiment`; `others`
/// are the panel's other agents, who share its board.
pub fn brief(
    experiment: &Path,
    run: &ResolvedRun,
    who: &Identity,
    others: &[String],
) -> Result<String> {
    let (from, end) = run_window(&run.config)?;
    let strategy = &run.config.strategies[0];
    let exp = experiment.display();
    let name = &who.name;
    let lpbot = "target/release/lpbot";
    let capital = run
        .config
        .run
        .capital_usd
        .map(|c| format!("${c:.0}"))
        .unwrap_or_else(|| "the run's capital".into());
    let loss = run
        .config
        .run
        .loss_limit_pct
        .map(|l| format!("{l}%"))
        .unwrap_or_else(|| "its loss limit".into());
    let span = match end {
        // Both days of the window are traded: it ends where `to` ends.
        Some(to) => format!(
            "The run covers {} days of stored pool history. You are not told which days, and \
             you must not try to find out.",
            to.end().saturating_sub(from.start()) / 86_400
        ),
        None => "The run is forward, at the head of the chain: every look shows the pool as it \
                 is now, on blocks no model has seen. Before each look `next` brings the stored \
                 history up to the node's head; when no look is due yet it says when by the \
                 wall clock, and `next --wait <duration>`, run in the background, waits for it \
                 and shows it. A look is refused while the node is behind the chain: try again \
                 later."
            .to_string(),
    };
    let mut text = String::new();
    let _ = writeln!(
        text,
        "# Brief for agent {name} ({})\n\nYou decide one run of lpbot's agent mode, look by \
         look: run `{}` of experiment `{exp}`, as agent `{name}`. {capital} in {}; the run \
         stops if it loses {loss}. {span}\n",
        who.model, run.run_id, strategy.value_token
    );
    let _ = writeln!(
        text,
        "## Commands\n\nFrom the repository root. Build once with `cargo build --release -p \
         lpbot`.\n\n\
         - `{lpbot} agent next {exp} --agent {name}` shows the look waiting for a decision: \
         price moves, highs, lows, volatility and volume from 15 minutes to 90 days, gas, the \
         run so far and any open position, all in percent from the price now.\n\
         - `{lpbot} agent decide {exp} --agent {name} --action stay_out|hold|exit|enter \
         [--lower <pct> --upper <pct>] --next <duration> --note \"<why, one sentence>\" \
         --reasoning \"<what you saw and weighed>\" --expect-low <pct> --expect-high <pct> \
         --expect-confidence <0-1> [--expect \"<what you expect, in words>\"]` writes the \
         decision and shows the next look, so keep calling `decide`.\n"
    );
    if !who.peers.is_empty() {
        let names: Vec<&str> = who.peers.iter().map(|p| p.name.as_str()).collect();
        let _ = writeln!(
            text,
            "- `{lpbot} agent peers {exp} --agent {name}` shows again every record of your \
             peers up to the look waiting for you.\n"
        );
        let _ = writeln!(
            text,
            "## Your peers\n\nYou read {}. After each look, `next` prints what they decided, \
             saw and expected up to that moment, with their reasoning, and the scores of their \
             earlier decisions once the outcome is known. Nothing later is shown. Decide your \
             own way: copy, adapt or depart from them, or decide without regard to them. When \
             a look shows their records, say which with `--stance own|copy|adapt|depart` and \
             `--toward <peer>`, and say why in `--reasoning`.\n",
            names.join(", ")
        );
    }
    if end.is_none() && !others.is_empty() {
        let _ = writeln!(
            text,
            "## Asking the others\n\nThe panel's other agents are {others}. At a look you may \
             ask them before you decide, and they may ask you. Questions and answers go through \
             the panel's board, and only the board counts.\n\n\
             - `{lpbot} agent ask {exp} --agent {name} --to <names|all> --question \"...\" \
             [--wait 10m]` asks at the waiting look; the answers are due that long after the \
             look was shown.\n\
             - `{lpbot} agent answers {exp} --agent {name} --wait` returns once all have \
             answered or the deadline has passed. Then decide, with or without them: no answer \
             in time means you decide alone, and a late one is shown at your next look.\n\
             - `{lpbot} agent inbox {exp} --agent {name}` lists the questions to you. Between \
             your looks, run `{lpbot} agent inbox {exp} --agent {name} --wait <duration>` in \
             the background: it returns as soon as a question arrives, so you can answer in \
             time, and otherwise when the time is up. Look at it at each of your looks too.\n\
             - `{lpbot} agent answer {exp} --agent {name} --ask <id> --answer \"...\" [--would \
             stay_out|hold|exit|enter [--lower <pct> --upper <pct>]] [--confidence <0-1>]` \
             answers. Your answers are counted, on time or late, and read by the asker and \
             later by tools.\n\
             - A decision acts no earlier than the newest answer or question you read, so ask \
             for what you need, not for what happened since.\n",
            others = others.join(", ")
        );
    }
    let _ = writeln!(
        text,
        "## What a decision means\n\n\
         - An entry is a range between two prices of the volatile token, in percent from now \
         (negative is below). A range wholly {side} the price holds only {value} and buys the \
         other token as price moves into it; a range around the price is entered by swapping \
         into its mix.\n\
         - Entering another range while one is open moves it: closed, then opened. `exit` \
         closes back to {value}; `hold` keeps what is open; `stay_out` stays flat.\n\
         - Pick your own cadence with `--next`, from minutes to days. The run's stops and loss \
         limit still apply between looks.\n\
         - At every look, give the band you expect price to stay within until your next look \
         and how sure you are (`--expect-low`, `--expect-high`, `--expect-confidence`). Every \
         decision and every band is scored afterwards against what happened and against the \
         actions you did not take.\n\
         - Other agents read your records: write your reasoning so another trader can follow \
         it and disagree with it.\n",
        value = strategy.value_token,
        side = if ["USDC", "USDT"].contains(&strategy.value_token.as_str()) {
            "below"
        } else {
            "above"
        }
    );
    let _ = writeln!(
        text,
        "## Rules, so the test stays honest\n\n\
         - Read only what the `lpbot agent` commands print for your run.\n\
         - Do not open the experiment file, `config/`, `docs/RESEARCH.md`, anything under \
         `runs/` or `data/`, the decision, log or score files of any agent, or the result \
         tables.\n\
         - Do not look up prices or news.\n\
         - Do not edit code, commit or switch branches.\n{}",
        if end.is_none() {
            "- Do not pass `--no-sync`, `--max-lag`, `--rpc` or `--data-dir`.\n"
        } else {
            ""
        }
    );
    let _ = writeln!(
        text,
        "## At the end\n\n{}, report the result it prints and your campaigns, and in a few \
         lines the strategy you followed, what earned, what cost, and what you would change.",
        if end.is_none() {
            "When the owner stops the run, or `next` says the loss limit stopped it, run `lpbot \
             run` on the experiment"
        } else {
            "When `next` says the window is over"
        }
    );
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_log::Peer;
    use lpbot_core::experiment::{resolve_experiment, RunOverrides};

    #[test]
    fn a_forward_run_is_briefed_as_one_at_the_head_with_no_dates() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let experiment = repo.join("experiments/agent-usdc-weth-forward.yml");
        let runs = resolve_experiment(&experiment, &RunOverrides::default()).unwrap();
        let who = crate::agent_log::identity(&runs[0].config.strategies[0])
            .unwrap()
            .unwrap();
        let text = brief(
            Path::new("experiments/forward.yml"),
            &runs[0],
            &who,
            &["b".to_string()],
        )
        .unwrap();
        assert!(text.contains("forward, at the head of the chain"), "{text}");
        assert!(text.contains("`next --wait <duration>`"), "{text}");
        assert!(text.contains("Do not pass `--no-sync`"), "{text}");
        assert!(!text.contains("days of stored pool history"), "{text}");
        assert!(!text.contains("2026"), "no dates: {text}");
        assert!(text.contains("## Asking the others"), "{text}");
        assert!(text.contains("The panel's other agents are b."), "{text}");
        assert!(
            text.contains("agent inbox experiments/forward.yml --agent"),
            "{text}"
        );
        assert!(
            text.contains("--wait <duration>` in the background"),
            "{text}"
        );
        let alone = brief(Path::new("experiments/forward.yml"), &runs[0], &who, &[]).unwrap();
        assert!(!alone.contains("Asking the others"), "{alone}");
    }

    #[test]
    fn a_brief_names_the_agent_its_peers_and_no_dates() {
        let dir = std::env::temp_dir().join(format!("lpbot-brief-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/mainnet.yml");
        let file = dir.join("panel.yml");
        std::fs::write(
            &file,
            format!(
                "experiment: panel\nbase: {}\nmode: backtest\n\
                 window: {{ from: 2026-07-01, to: 2026-07-15 }}\n\
                 run: {{ capital_usd: 10000, loss_limit_pct: 20, autonomy: auto }}\n\
                 strategy: eth-zone-roundtrip\nsweep:\n  value_token: [USDC]\n  agent:\n    \
                 - {{ name: b, decisions: data/agent/p/b.jsonl, peers: [a] }}\n",
                base.display()
            ),
        )
        .unwrap();
        let runs = resolve_experiment(&file, &RunOverrides::default()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let who = Identity {
            name: "b".into(),
            model: "claude-sonnet-5-5".into(),
            decisions: "data/agent/p/b.jsonl".into(),
            peers: vec![Peer {
                name: "a".into(),
                stem: "data/agent/p/a".into(),
            }],
        };
        let text = brief(
            Path::new("experiments/panel.yml"),
            &runs[0],
            &who,
            &["a".to_string()],
        )
        .unwrap();
        assert!(
            text.starts_with("# Brief for agent b (claude-sonnet-5-5)"),
            "{text}"
        );
        assert!(text.contains("15 days of stored pool history"), "{text}");
        assert!(!text.contains("2026"), "no dates: {text}");
        assert!(
            text.contains("agent next experiments/panel.yml --agent b"),
            "{text}"
        );
        assert!(text.contains("You read a."), "{text}");
        assert!(text.contains("--stance own|copy|adapt|depart"), "{text}");
        assert!(text.contains("$10000 in USDC"), "{text}");
        assert!(text.contains("Do not open the experiment file"), "{text}");
        assert!(
            !text.contains("Asking the others"),
            "no asking in a backtest: {text}"
        );
        assert!(
            text.contains("A range wholly below the price holds only USDC"),
            "{text}"
        );

        let alone = Identity {
            peers: Vec::new(),
            ..who
        };
        let text = brief(Path::new("experiments/panel.yml"), &runs[0], &alone, &[]).unwrap();
        assert!(
            !text.contains("Your peers") && !text.contains("agent peers"),
            "{text}"
        );
    }
}
