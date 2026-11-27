//! `lpbot agent brief`: what a session acting as one agent of a panel is
//! told before its first look: who it is, the commands, what its decisions
//! mean and the rules that keep the test honest. It names no dates.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{bail, Result};
use lpbot_core::config::StrategyConfig;
use lpbot_core::experiment::ResolvedRun;

use crate::agent_log::Identity;
use crate::run_cmd::run_window;

/// A way of trading an agent is told to follow (`agent.mandate`), so that
/// a panel can set the same model on different mandates, or different
/// models on the same one. `open`, or none, leaves the agent to itself.
pub const MANDATES: [&str; 5] = ["open", "trend", "range", "fade", "patient"];

/// The mandate of a run's agent, checked against `MANDATES`. `None` for
/// `open` or none.
pub fn mandate(strategy: &StrategyConfig) -> Result<Option<&'static str>> {
    let value = strategy
        .extra
        .get("agent")
        .and_then(|agent| agent.get("mandate"));
    match value {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(value) => {
            let name = value.as_str().unwrap_or_default().trim();
            match MANDATES.iter().find(|known| **known == name) {
                Some(&"open") => Ok(None),
                Some(known) => Ok(Some(known)),
                None => bail!(
                    "agent.mandate `{}` is not one of {}",
                    value.as_str().unwrap_or("that"),
                    MANDATES.join(", ")
                ),
            }
        }
    }
}

/// What a mandate tells the agent, in terms of what a look shows. `dips`
/// is whether the value token is the dollar: its one-sided ranges then sit
/// below the price and buy the volatile token as it falls; otherwise they
/// sit above and sell it as it rises.
fn mandate_text(name: &str, value: &str, dips: bool) -> String {
    let (favoured, against, entry_move, one_sided) = if dips {
        (
            "rising or holding",
            "falling",
            "a fall",
            format!("below the price, buying the volatile token with {value} as it dips"),
        )
    } else {
        (
            "falling or holding",
            "rising",
            "a rise",
            format!("above the price, selling {value} as it rises"),
        )
    };
    match name {
        "trend" => format!(
            "Long timeframe first, then the short one. Read every look from the bottom of the \
             table up.\n\n\
             1. The 30- and 90-day lines are the trend: how far price moved over them and where \
             it sits between their highs and lows. Your ranges sit {one_sided}, so they are \
             paid when the long timeframe is {favoured}, and left holding the wrong token when \
             it is {against}. While the 30- and 90-day lines are clearly {against}, stay out.\n\
             2. The day and week lines say whether price is going back and forth or running. \
             Prefer a market that goes back and forth inside the long trend.\n\
             3. The 15-minute to 4-hour lines only time the entry: enter after {entry_move} \
             against the long trend has slowed, not while it runs, in a range wide enough for \
             the day's volatility.\n\n\
             Give a position up when the long timeframe turns, not because of an hour's move. \
             In every `--reasoning`, say what the long, the middle and the short timeframe each \
             told you."
        ),
        "range" => format!(
            "Provide liquidity only in a market that is going sideways. The week and the 30-day \
             lines must show price going back and forth: a small change over them against the \
             distance between their high and their low, with both visited. Place the range \
             inside the band the 30 days held, {one_sided}, wide enough for the day's \
             volatility, and leave it in place while price stays in that band. When price \
             leaves the band, or the week turns into a run, exit and stay out until it is \
             sideways again. Most looks in a running market should end in `stay_out`. In every \
             `--reasoning`, give the band you see and why the market is or is not sideways."
        ),
        "fade" => format!(
            "Stay out by default. Enter only after price has moved fast, several percent within \
             hours on the 1-hour and 4-hour lines, and only as {entry_move}: a range \
             {one_sided}, next to the price, a bet that the move stalls and comes back. One \
             entry a move: after an exit, wait for a new one. Leave after about a day, or as \
             soon as the position is back wholly in {value} with its fees. Look often enough \
             to see a fast move, and expect most looks to end in `stay_out`. In every \
             `--reasoning`, say how fast price moved and why you think it stalls."
        ),
        _ => format!(
            "Few, wide and slow. Look once or twice a day. Enter rarely, in a range several \
             days of volatility wide, {one_sided}, where the 30- and 90-day lines say price has \
             turned before, and leave it for days. Do not move a range because price moved; \
             exit only when the reason you entered no longer holds. Gas and swaps are paid on \
             every change, so count them in. In every `--reasoning`, say what would make you \
             exit."
        ),
    }
}

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
    let dips = ["USDC", "USDT"].contains(&strategy.value_token.as_str());
    if let Some(name) = mandate(strategy)? {
        let _ = writeln!(
            text,
            "## Your mandate: {name}\n\n{}\n\nThe mandate is how you trade this run. Staying \
             out is always allowed under it; trading another way is not. It is part of what is \
             being measured, so follow it even when you would do otherwise.\n",
            mandate_text(name, &strategy.value_token, dips)
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
        side = if dips { "below" } else { "above" }
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
    fn a_mandate_is_told_in_the_brief_for_the_side_the_value_token_trades() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let experiment = repo.join("experiments/agent-mandates-practice.yml");
        let runs = resolve_experiment(&experiment, &RunOverrides::default()).unwrap();
        let mut seen = Vec::new();
        for run in &runs {
            let strategy = &run.config.strategies[0];
            let who = crate::agent_log::identity(strategy).unwrap().unwrap();
            let text = brief(Path::new("experiments/x.yml"), run, &who, &[]).unwrap();
            // The first line is what start-agent.sh reads the model from.
            assert!(
                text.starts_with(&format!("# Brief for agent {} ({})\n", who.name, who.model)),
                "{text}"
            );
            assert!(!text.contains("2026"), "no dates: {text}");
            match mandate(strategy).unwrap() {
                None => assert!(!text.contains("## Your mandate"), "{text}"),
                Some(name) => {
                    assert!(
                        text.contains(&format!("## Your mandate: {name}\n")),
                        "{text}"
                    );
                    // Dollars are the value token: ranges below the price.
                    assert!(text.contains("below the price, buying the volatile token with USDC"));
                    assert!(text.contains("Staying out is always allowed"), "{text}");
                    seen.push(name);
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, ["fade", "range", "trend"]);

        // With the volatile token as the value token the ranges sit above.
        for name in MANDATES {
            let eth = mandate_text(name, "WETH", false);
            assert!(
                eth.contains("above the price, selling WETH"),
                "{name}: {eth}"
            );
            assert!(!eth.contains("USDC"), "{name}");
        }
        assert!(mandate_text("trend", "WETH", false).contains("clearly rising, stay out"));
        assert!(mandate_text("trend", "USDC", true).contains("clearly falling, stay out"));

        // `open` and none are no mandate; anything else is refused by name.
        let strategy = |agent: &str| -> StrategyConfig {
            serde_yaml::from_str(&format!(
                "id: s\nkind: zone_round_trip\nvalue_token: USDC\nagent: {agent}\n"
            ))
            .unwrap()
        };
        assert_eq!(mandate(&strategy("{ mandate: open }")).unwrap(), None);
        assert_eq!(mandate(&strategy("{ mandate: null }")).unwrap(), None);
        assert_eq!(mandate(&strategy("{ name: a }")).unwrap(), None);
        assert_eq!(
            mandate(&strategy("{ mandate: patient }")).unwrap(),
            Some("patient")
        );
        let refused = mandate(&strategy("{ mandate: scalper }"))
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("`scalper` is not one of open, trend"),
            "{refused}"
        );
        assert!(mandate(&strategy("{ mandate: 3 }")).is_err());
        let with_file = strategy("{ mandate: scalper, decisions: data/agent/x/a.jsonl }");
        assert!(crate::agent_log::identity(&with_file).is_err());
    }

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
