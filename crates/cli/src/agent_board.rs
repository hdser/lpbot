//! The advice board of a forward or live panel (`docs/DESIGN.md`, "Advice
//! in forward and live runs"): agents ask each other at a look and answer,
//! through one append-only file in the panel's folder, `board.jsonl`.
//!
//! Every record is one JSON line in the shared log's schema, of kind `ask`
//! or `answer`, and carries `as_of`, the latest market its author could
//! have seen through the tools. A question is as of the asker's look; an
//! answer is as of the answerer's latest look, or of the question when that
//! is later. A decision acts no earlier than the newest record it read, so
//! advice never carries a later market into an earlier action.
//!
//! Writers in several sessions append at once: each record is written as
//! one line, in one write, under an exclusive lock on the file, and readers
//! take a shared lock. Asking is refused in a backtest, where an answerer
//! at a later look would know what comes.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::experiment::ResolvedRun;
use serde_json::{json, Value};

use crate::agent_cmd::{clock, identity_of, read_pending, span, write_pending};
use crate::agent_log::{now_unix, read_records, records_path, Identity, SCHEMA};
use crate::run_cmd::run_window;

/// The board of the panel an agent decides in: `board.jsonl` beside its
/// decisions file, shared by every agent of the folder.
pub fn board_path(decisions: &Path) -> PathBuf {
    decisions
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("board.jsonl")
}

/// The records of a board's text. A last line cut short (a writer that
/// died mid-line) is left out; any other line that is not JSON is an error.
fn parse_board(content: &str, path: &Path) -> Result<Vec<Value>> {
    let complete = content.ends_with('\n');
    let lines: Vec<&str> = content.lines().collect();
    let mut records = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(record) => records.push(record),
            Err(_) if i + 1 == lines.len() && !complete => {}
            Err(e) => bail!("{} line {} is not JSON: {e}", path.display(), i + 1),
        }
    }
    Ok(records)
}

/// The board's records, read under a shared lock. A board not yet written
/// has none.
pub fn read_board(path: &Path) -> Result<Vec<Value>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    file.lock_shared()
        .with_context(|| format!("locking {}", path.display()))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("reading {}", path.display()))?;
    parse_board(&content, path)
}

/// Appends one record under an exclusive lock. `make` sees the board as it
/// is under the lock (to number the record, or refuse it), and what it
/// returns is written as one line in one write. A last line cut short by a
/// writer that died is removed first.
pub fn append(path: &Path, make: impl FnOnce(&[Value]) -> Result<Value>) -> Result<Value> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking {}", path.display()))?;
    let mut content = String::new();
    file.seek(SeekFrom::Start(0))?;
    file.read_to_string(&mut content)?;
    let record = make(&parse_board(&content, path)?)?;
    // A line a writer that died left cut short goes before the next one.
    if !content.is_empty() && !content.ends_with('\n') {
        let whole = content.rfind('\n').map_or(0, |i| i + 1);
        file.set_len(whole as u64)
            .with_context(|| format!("trimming the cut last line of {}", path.display()))?;
    }
    let mut line = record.to_string();
    line.push('\n');
    file.write_all(line.as_bytes())
        .and_then(|()| file.sync_data())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(record)
}

fn text(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

fn is_ask(r: &Value) -> bool {
    r["kind"] == "ask"
}

fn is_answer(r: &Value) -> bool {
    r["kind"] == "answer"
}

fn addressed_to(ask: &Value, agent: &str) -> bool {
    ask["to"]
        .as_array()
        .is_some_and(|to| to.iter().any(|a| a == agent))
}

fn answer_of<'a>(board: &'a [Value], ask: &str, agent: &str) -> Option<&'a Value> {
    board
        .iter()
        .find(|r| is_answer(r) && r["ask"] == ask && r["agent"] == agent)
}

fn deadline(ask: &Value) -> u64 {
    ask["deadline"].as_u64().unwrap_or(0)
}

/// The key a board record is noted by in a look's record.
pub fn key(record: &Value) -> String {
    if is_ask(record) {
        format!("ask:{}", text(&record["id"]))
    } else {
        format!("answer:{}:{}", text(&record["ask"]), text(&record["agent"]))
    }
}

/// Refuses asking and answering outside a forward or live run.
fn forward_only(run: &ResolvedRun) -> Result<()> {
    if run_window(&run.config)?.1.is_some() {
        bail!(
            "asking and answering are for forward and live panels, whose window is open at the \
             end (`to: open`). In a backtest an answerer at a later look would know what comes; \
             lockstep panels, where every agent looks at the same times and it would be safe, \
             are not built"
        );
    }
    Ok(())
}

/// Who `--to` names: `all` (every other agent of the panel) or a list of
/// names. Every addressee decides in the same folder, so reads this board.
pub fn addressees(who: &Identity, panel: &[Identity], to: &str) -> Result<Vec<String>> {
    let board = board_path(&who.decisions);
    let others: Vec<&Identity> = panel
        .iter()
        .filter(|p| p.name != who.name && board_path(&p.decisions) == board)
        .collect();
    let names: Vec<&str> = others.iter().map(|p| p.name.as_str()).collect();
    let wanted: Vec<String> = if to.trim() == "all" {
        names.iter().map(|n| n.to_string()).collect()
    } else {
        let mut list = Vec::new();
        for name in to.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if name == who.name {
                bail!("--to {name}: that is you; ask the other agents of the panel");
            }
            if !names.contains(&name) {
                bail!(
                    "--to {name}: the panel's other agents are {}",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                );
            }
            if !list.iter().any(|n| n == name) {
                list.push(name.to_string());
            }
        }
        list
    };
    if wanted.is_empty() {
        bail!("--to {to}: no one to ask; name agents of the panel, or `all`");
    }
    Ok(wanted)
}

/// The latest market an agent has seen through the tools: its waiting
/// look, or else its last decided look.
fn knowledge(who: &Identity) -> Result<Option<u64>> {
    if let Ok(waiting) = read_pending(&who.decisions) {
        if let Some(at) = waiting.at {
            return Ok(Some(at));
        }
    }
    Ok(read_records(&records_path(&who.decisions))?
        .iter()
        .filter_map(|r| r["as_of"].as_u64())
        .max())
}

/// Notes board records an agent was shown while a look waits for it, with
/// the market each is as of: its decision acts no earlier than the newest.
fn note_read(who: &Identity, records: &[&Value]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    let Ok(mut waiting) = read_pending(&who.decisions) else {
        return Ok(());
    };
    let Some(record) = waiting.record.as_mut() else {
        return Ok(());
    };
    let mut read: Vec<Value> = record["board_read"].as_array().cloned().unwrap_or_default();
    for r in records {
        let k = key(r);
        if !read.iter().any(|x| x["key"] == k.as_str()) {
            read.push(json!({"key": k, "as_of": r["as_of"]}));
        }
    }
    record["board_read"] = Value::Array(read);
    write_pending(&who.decisions, &waiting)
}

/// Asks the panel at the waiting look, the deadline `wait` after the look
/// was shown, by the wall clock. Returns the question as written.
pub fn ask_at(
    run: &ResolvedRun,
    panel: &[Identity],
    to: &str,
    question: &str,
    wait: u64,
    now: u64,
) -> Result<Value> {
    let who = identity_of(run)?;
    forward_only(run)?;
    let waiting = read_pending(&who.decisions)?;
    let at = waiting
        .at
        .ok_or_else(|| anyhow!("run `lpbot agent next` again: the waiting look has no time"))?;
    let question = question.trim();
    if question.is_empty() {
        bail!("--question is empty: say what you want to know");
    }
    if wait == 0 {
        bail!("--wait 0 leaves no time to answer; give a duration such as 10m");
    }
    let due = at.saturating_add(wait);
    if due <= now {
        bail!(
            "the look was shown {} ago, so answers due {} after it are already late; run `lpbot \
             agent next` again to see the look at the head, or give a longer --wait",
            span(now.saturating_sub(at)),
            span(wait)
        );
    }
    let to = addressees(&who, panel, to)?;
    let number = waiting.number.unwrap_or(0);
    append(&board_path(&who.decisions), |board| {
        let k = board
            .iter()
            .filter(|r| is_ask(r) && r["agent"] == who.name.as_str() && r["look"] == waiting.look)
            .count()
            + 1;
        Ok(json!({
            "schema": SCHEMA,
            "kind": "ask",
            "id": format!("{}-{number}-{k}", who.name),
            "agent": who.name,
            "model": who.model,
            "experiment": run.experiment,
            "run_id": run.run_id,
            "to": to,
            "question": question,
            "look": waiting.look,
            "number": number,
            "day": waiting.day,
            "at": at,
            "as_of": at,
            "deadline": due,
            "asked_unix": now,
            "seen_text": waiting.seen,
        }))
    })
}

/// What an answerer would do in the asker's place.
#[derive(Debug, Clone, PartialEq)]
pub struct Would {
    pub action: String,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
}

impl Would {
    pub fn to_json(&self) -> Result<Value> {
        match (self.action.as_str(), self.lower_pct, self.upper_pct) {
            ("enter", Some(lower), Some(upper)) if lower.is_finite() && upper.is_finite() => {
                Ok(json!({"action": "enter", "lower_pct": lower, "upper_pct": upper}))
            }
            ("enter", _, _) => {
                bail!("--would enter needs --lower and --upper, in percent from the asker's price")
            }
            ("stay_out" | "hold" | "exit", None, None) => Ok(json!({"action": self.action})),
            ("stay_out" | "hold" | "exit", _, _) => {
                bail!("--lower and --upper go with --would enter only")
            }
            (other, _, _) => bail!("--would {other}: use stay_out, hold, exit or enter"),
        }
    }
}

/// Answers a question, as of the answerer's latest look or the question,
/// whichever is later. After the deadline it is kept and marked late.
pub fn answer_at(
    run: &ResolvedRun,
    ask_id: &str,
    answer: &str,
    would: Option<&Would>,
    confidence: Option<f64>,
    now: u64,
) -> Result<Value> {
    let who = identity_of(run)?;
    forward_only(run)?;
    let answer = answer.trim();
    if answer.is_empty() {
        bail!("--answer is empty: say what you think");
    }
    if let Some(p) = confidence {
        if !(0.0..=1.0).contains(&p) {
            bail!("--confidence {p}: give it from 0 to 1");
        }
    }
    let would = would.map(Would::to_json).transpose()?;
    let seen = knowledge(&who)?;
    append(&board_path(&who.decisions), |board| {
        let ask = board
            .iter()
            .find(|r| is_ask(r) && r["id"] == ask_id)
            .ok_or_else(|| {
                anyhow!(
                    "no question {ask_id} on the board; `lpbot agent inbox` lists the ones to you"
                )
            })?;
        if !addressed_to(ask, &who.name) {
            bail!(
                "question {ask_id} is to {}, not to {}",
                ask["to"]
                    .as_array()
                    .map(|to| to.iter().map(text).collect::<Vec<_>>().join(", "))
                    .unwrap_or_default(),
                who.name
            );
        }
        if answer_of(board, ask_id, &who.name).is_some() {
            bail!("you answered {ask_id} already; an answer is not changed once written");
        }
        let asked_as_of = ask["as_of"].as_u64().unwrap_or(0);
        Ok(json!({
            "schema": SCHEMA,
            "kind": "answer",
            "ask": ask_id,
            "asker": ask["agent"],
            "agent": who.name,
            "model": who.model,
            "experiment": run.experiment,
            "run_id": run.run_id,
            "answer": answer,
            "would": would,
            "confidence": confidence,
            "as_of": seen.unwrap_or(0).max(asked_as_of),
            "answered_unix": now,
            "late": now > deadline(ask),
        }))
    })
}

/// Questions to `me` still open: before their deadline, not answered by me.
pub fn open_questions<'a>(board: &'a [Value], me: &str, now: u64) -> Vec<&'a Value> {
    board
        .iter()
        .filter(|r| is_ask(r) && addressed_to(r, me) && deadline(r) > now)
        .filter(|r| answer_of(board, text(&r["id"]), me).is_none())
        .collect()
}

/// Questions to `me` whose deadline passed without my answer.
pub fn missed_questions<'a>(board: &'a [Value], me: &str, now: u64) -> Vec<&'a Value> {
    board
        .iter()
        .filter(|r| is_ask(r) && addressed_to(r, me) && deadline(r) <= now)
        .filter(|r| answer_of(board, text(&r["id"]), me).is_none())
        .collect()
}

/// How each agent answered what it was asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rate {
    /// Questions it asked.
    pub asked: usize,
    /// Questions to it, and how they went.
    pub to_it: usize,
    pub on_time: usize,
    pub late: usize,
    pub missed: usize,
    pub open: usize,
}

impl Rate {
    pub fn describe(&self) -> String {
        format!(
            "asked {}; of {} question(s) to it, answered {} on time and {} late, missed {}, {} \
             open",
            self.asked, self.to_it, self.on_time, self.late, self.missed, self.open
        )
    }
}

/// Every agent's questions and answers, by name.
pub fn rates(board: &[Value], now: u64) -> BTreeMap<String, Rate> {
    let mut rates: BTreeMap<String, Rate> = BTreeMap::new();
    for ask in board.iter().filter(|r| is_ask(r)) {
        rates
            .entry(text(&ask["agent"]).to_string())
            .or_default()
            .asked += 1;
        for to in ask["to"].as_array().into_iter().flatten() {
            let rate = rates.entry(text(to).to_string()).or_default();
            rate.to_it += 1;
            match answer_of(board, text(&ask["id"]), text(to)) {
                Some(a) if a["late"] == true => rate.late += 1,
                Some(_) => rate.on_time += 1,
                None if deadline(ask) <= now => rate.missed += 1,
                None => rate.open += 1,
            }
        }
    }
    rates
}

fn would_text(would: &Value) -> Option<String> {
    let action = would["action"].as_str()?;
    Some(
        match (would["lower_pct"].as_f64(), would["upper_pct"].as_f64()) {
            (Some(lower), Some(upper)) => format!("would enter {lower:+.2}% to {upper:+.2}%"),
            _ => format!("would {action}"),
        },
    )
}

fn quoted(text: &str) -> String {
    text.lines()
        .map(|line| format!("    > {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A question as its addressee is shown it, with the asker's look.
pub fn question_text(ask: &Value, now: u64) -> String {
    let mut out = String::new();
    let due = deadline(ask);
    let _ = writeln!(
        out,
        "Question {} from {} ({}), asked {} ago; answer by {} ({}):",
        text(&ask["id"]),
        text(&ask["agent"]),
        text(&ask["model"]),
        span(now.saturating_sub(ask["asked_unix"].as_u64().unwrap_or(now))),
        clock(due),
        if due > now {
            format!("in {}", span(due - now))
        } else {
            format!("{} ago, an answer now is late", span(now - due))
        }
    );
    let _ = writeln!(out, "{}", quoted(text(&ask["question"])));
    if let Some(seen) = ask["seen_text"].as_str() {
        let _ = writeln!(
            out,
            "  Its look, as {} saw it:\n```text\n{}\n```",
            text(&ask["agent"]),
            seen.trim_end()
        );
    }
    out
}

/// An answer as its asker is shown it.
pub fn answer_text(answer: &Value) -> String {
    let mut out = String::new();
    let mut said = vec![if answer["late"] == true {
        "late".to_string()
    } else {
        "on time".to_string()
    }];
    if let Some(would) = would_text(&answer["would"]) {
        said.push(would);
    }
    if let Some(p) = answer["confidence"].as_f64() {
        said.push(format!("{:.0}% sure", p * 100.0));
    }
    let _ = writeln!(
        out,
        "  {} ({}), {}:",
        text(&answer["agent"]),
        text(&answer["model"]),
        said.join(", ")
    );
    let _ = writeln!(out, "{}", quoted(text(&answer["answer"])));
    out
}

/// The agent's questions at a look, with their answers so far and who has
/// not answered.
fn answers_text(board: &[Value], asks: &[&Value], now: u64) -> String {
    let mut out = String::new();
    for ask in asks {
        let id = text(&ask["id"]);
        let _ = writeln!(out, "Question {id}: {}", text(&ask["question"]));
        for to in ask["to"].as_array().into_iter().flatten() {
            match answer_of(board, id, text(to)) {
                Some(answer) => out.push_str(&answer_text(answer)),
                None if deadline(ask) > now => {
                    let _ = writeln!(
                        out,
                        "  {}: no answer yet; due in {}",
                        text(to),
                        span(deadline(ask) - now)
                    );
                }
                None => {
                    let _ = writeln!(
                        out,
                        "  {}: no answer by the deadline; decide without it",
                        text(to)
                    );
                }
            }
        }
    }
    out
}

/// The agent's questions at the look `look`.
fn asks_at<'a>(board: &'a [Value], me: &str, look: u64) -> Vec<&'a Value> {
    board
        .iter()
        .filter(|r| is_ask(r) && r["agent"] == me && r["look"] == look)
        .collect()
}

/// Lists the open questions to the agent; with `wait`, waits up to that
/// long for one to arrive, asking the board every `poll`. Returns how many
/// are open.
pub fn inbox(run: &ResolvedRun, wait: u64, poll: Duration) -> Result<usize> {
    let who = identity_of(run)?;
    let path = board_path(&who.decisions);
    let started = now_unix();
    let mut said_waiting = false;
    loop {
        let board = read_board(&path)?;
        let now = now_unix();
        let open = open_questions(&board, &who.name, now);
        let until = started.saturating_add(wait);
        if !open.is_empty() || now >= until {
            print!("{}", inbox_text(&board, &who.name, now));
            note_read(&who, &open)?;
            return Ok(open.len());
        }
        if !said_waiting {
            println!(
                "No open question to {} yet; waiting up to {} for one.",
                who.name,
                span(wait)
            );
            said_waiting = true;
        }
        let left = Duration::from_secs(until - now);
        std::thread::sleep(poll.min(left).max(Duration::from_millis(10)));
    }
}

/// What `inbox` prints: the open questions, those missed, and the agent's
/// answers so far.
pub fn inbox_text(board: &[Value], me: &str, now: u64) -> String {
    let mut out = String::new();
    let open = open_questions(board, me, now);
    if open.is_empty() {
        let _ = writeln!(out, "No open question to {me}.");
    }
    for ask in &open {
        out.push_str(&question_text(ask, now));
        let _ = writeln!(
            out,
            "Answer with: lpbot agent answer <experiment> --agent {me} --ask {} --answer \"...\" \
             [--would stay_out|hold|exit|enter [--lower <pct> --upper <pct>]] [--confidence \
             <0-1>]\n",
            text(&ask["id"])
        );
    }
    let missed = missed_questions(board, me, now);
    if !missed.is_empty() {
        let ids: Vec<&str> = missed.iter().map(|a| text(&a["id"])).collect();
        let _ = writeln!(
            out,
            "Past their deadline without your answer: {}. An answer now is kept, marked late.",
            ids.join(", ")
        );
    }
    if let Some(rate) = rates(board, now).get(me) {
        let _ = writeln!(out, "So far, {me}: {}.", rate.describe());
    }
    out
}

/// Shows the answers to the agent's questions at its waiting look; with
/// `wait`, first waits until every addressee has answered or the last
/// deadline has passed, asking the board every `poll`.
pub fn answers(run: &ResolvedRun, wait: bool, poll: Duration) -> Result<()> {
    let who = identity_of(run)?;
    let waiting = read_pending(&who.decisions)?;
    let path = board_path(&who.decisions);
    let mut said_waiting = false;
    loop {
        let board = read_board(&path)?;
        let now = now_unix();
        let asks = asks_at(&board, &who.name, waiting.look);
        if asks.is_empty() {
            println!("You asked nothing at the waiting look; `lpbot agent ask` asks the panel.");
            return Ok(());
        }
        let missing = asks.iter().any(|ask| {
            ask["to"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|to| answer_of(&board, text(&ask["id"]), text(to)).is_none())
        });
        let last = asks.iter().map(|a| deadline(a)).max().unwrap_or(0);
        if !wait || !missing || now >= last {
            print!("{}", answers_text(&board, &asks, now));
            let read: Vec<&Value> = board
                .iter()
                .filter(|r| is_answer(r) && asks.iter().any(|a| a["id"] == r["ask"]))
                .collect();
            return note_read(&who, &read);
        }
        if !said_waiting {
            println!(
                "Waiting for answers until every addressee has answered, {} at most.",
                span(last - now)
            );
            said_waiting = true;
        }
        let left = Duration::from_secs(last - now);
        std::thread::sleep(poll.min(left).max(Duration::from_millis(10)));
    }
}

/// What a decision records of the board: the questions its look asked, the
/// answers there were (and which it read), the addressees that did not
/// answer, the late answers its look was shown, and the newest market any
/// of the records it read is as of.
#[derive(Debug, Clone, PartialEq)]
pub struct Advice {
    pub json: Value,
    pub newest_read: Option<u64>,
}

impl Advice {
    pub fn is_empty(&self) -> bool {
        self.json.is_null()
    }
}

/// The advice of the look `look`, from the board and the look's record.
pub fn advice_at_decide(board: &[Value], me: &str, look: u64, record: Option<&Value>) -> Advice {
    let read: Vec<&Value> = record
        .and_then(|r| r["board_read"].as_array())
        .map(|a| a.iter().collect())
        .unwrap_or_default();
    let newest_read = read.iter().filter_map(|r| r["as_of"].as_u64()).max();
    let was_read = |k: &str| read.iter().any(|r| r["key"] == k);
    let asks = asks_at(board, me, look);
    let mut answers = Vec::new();
    let mut unanswered = Vec::new();
    for ask in &asks {
        let id = text(&ask["id"]);
        for to in ask["to"].as_array().into_iter().flatten() {
            match answer_of(board, id, text(to)) {
                Some(a) => answers.push(json!({
                    "ask": id,
                    "agent": a["agent"],
                    "model": a["model"],
                    "late": a["late"],
                    "read": was_read(&key(a)),
                    "would": a["would"],
                    "confidence": a["confidence"],
                    "answer": a["answer"],
                })),
                None => unanswered.push(json!({"ask": id, "agent": to})),
            }
        }
    }
    let late_shown = record
        .and_then(|r| r["late_shown"].as_array())
        .cloned()
        .unwrap_or_default();
    if asks.is_empty() && late_shown.is_empty() && read.is_empty() {
        return Advice {
            json: Value::Null,
            newest_read,
        };
    }
    let asked: Vec<Value> = asks
        .iter()
        .map(|a| {
            json!({"id": a["id"], "to": a["to"], "question": a["question"], "deadline": a["deadline"]})
        })
        .collect();
    let questions_read: Vec<&str> = read
        .iter()
        .filter_map(|r| r["key"].as_str()?.strip_prefix("ask:"))
        .collect();
    Advice {
        json: json!({
            "asked": asked,
            "answers": answers,
            "unanswered": unanswered,
            "late_shown": late_shown,
            "questions_read": questions_read,
        }),
        newest_read,
    }
}

/// The advice as lines of the Markdown reasoning log.
pub fn advice_markdown(advice: &Value) -> String {
    let mut out = String::new();
    for ask in advice["asked"].as_array().into_iter().flatten() {
        let to: Vec<&str> = ask["to"]
            .as_array()
            .into_iter()
            .flatten()
            .map(text)
            .collect();
        let _ = writeln!(
            out,
            "Asked {} ({}): {}\n",
            to.join(", "),
            text(&ask["id"]),
            text(&ask["question"])
        );
    }
    for a in advice["answers"].as_array().into_iter().flatten() {
        let mut said = vec![if a["late"] == true { "late" } else { "on time" }.to_string()];
        if a["read"] != true {
            said.push("not read before deciding".into());
        }
        if let Some(would) = would_text(&a["would"]) {
            said.push(would);
        }
        let _ = writeln!(
            out,
            "Answer from {} ({}): {}\n",
            text(&a["agent"]),
            said.join(", "),
            text(&a["answer"]).trim()
        );
    }
    for u in advice["unanswered"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "No answer from {} to {}.\n",
            text(&u["agent"]),
            text(&u["ask"])
        );
    }
    out
}

/// Late answers to the agent's earlier questions that no look of it was
/// shown yet: shown at its next look. `own` are its decided looks.
pub fn late_unseen<'a>(board: &'a [Value], me: &str, own: &[Value]) -> Vec<&'a Value> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for r in own {
        let advice = &r["advice"];
        for a in advice["answers"].as_array().into_iter().flatten() {
            if a["read"] == true {
                seen.insert(format!("answer:{}:{}", text(&a["ask"]), text(&a["agent"])));
            }
        }
        for k in advice["late_shown"].as_array().into_iter().flatten() {
            seen.insert(text(k).to_string());
        }
    }
    board
        .iter()
        .filter(|r| is_answer(r) && r["asker"] == me && r["late"] == true)
        .filter(|r| !seen.contains(&key(r)))
        .collect()
}

/// Shows the late answers a look hands over and notes them in its record:
/// what `next` adds to a look of a panel with a board.
pub fn late_section(who: &Identity, own: &[Value], record: &mut Value) -> Result<String> {
    let board = read_board(&board_path(&who.decisions))?;
    let late = late_unseen(&board, &who.name, own);
    if late.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::from("Answers that came after their deadline:\n");
    for answer in &late {
        let ask = board
            .iter()
            .find(|r| is_ask(r) && r["id"] == answer["ask"])
            .map(|a| text(&a["question"]))
            .unwrap_or_default();
        let _ = writeln!(out, "To your question {}: {ask}", text(&answer["ask"]));
        out.push_str(&answer_text(answer));
    }
    let keys: Vec<Value> = late.iter().map(|a| json!(key(a))).collect();
    record["late_shown"] = Value::Array(keys);
    let read: Vec<Value> = late
        .iter()
        .map(|a| json!({"key": key(a), "as_of": a["as_of"]}))
        .collect();
    record["board_read"] = Value::Array(read);
    Ok(out)
}

/// The board of the panel: every agent's questions and answers so far.
pub fn summary(panel: &[Identity]) -> Result<String> {
    let Some(first) = panel.first() else {
        bail!("no run of the experiment is decided by an agent (agent.decisions)");
    };
    let path = board_path(&first.decisions);
    let board = read_board(&path)?;
    let now = now_unix();
    let mut out = String::new();
    let asks = board.iter().filter(|r| is_ask(r)).count();
    let answers = board.iter().filter(|r| is_answer(r)).count();
    let _ = writeln!(
        out,
        "{}: {asks} question(s), {answers} answer(s).",
        path.display()
    );
    let rates = rates(&board, now);
    for who in panel {
        let rate = rates.get(&who.name).cloned().unwrap_or_default();
        let _ = writeln!(out, "  {} ({}): {}", who.name, who.model, rate.describe());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lpbot-board-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ask(id: &str, from: &str, to: &[&str], look: u64, deadline: u64) -> Value {
        json!({"kind": "ask", "id": id, "agent": from, "model": "m", "to": to,
               "question": format!("what about {id}?"), "look": look, "as_of": look,
               "deadline": deadline, "asked_unix": look})
    }

    fn answer(ask: &str, asker: &str, from: &str, late: bool, as_of: u64) -> Value {
        json!({"kind": "answer", "ask": ask, "asker": asker, "agent": from, "model": "m",
               "answer": format!("{from} on {ask}"), "would": {"action": "hold"},
               "confidence": 0.7, "as_of": as_of, "late": late})
    }

    #[test]
    fn writers_in_several_threads_append_whole_lines_and_number_them_in_turn() {
        let dir = temp("threads");
        let path = dir.join("board.jsonl");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        append(&path, |board| {
                            // The number is taken under the lock: two writers
                            // never take the same one.
                            Ok(json!({"kind": "ask", "id": board.len() + 1, "writer": t,
                                      "padding": "x".repeat(200 + i)}))
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let board = read_board(&path).unwrap();
        assert_eq!(board.len(), 200);
        let ids: Vec<u64> = board.iter().map(|r| r["id"].as_u64().unwrap()).collect();
        assert_eq!(
            ids,
            (1..=200).collect::<Vec<_>>(),
            "numbered in turn, none lost"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 200);
        assert!(content.ends_with('\n'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_line_cut_short_at_the_end_is_left_out_and_the_next_write_starts_a_line() {
        let dir = temp("cut");
        let path = dir.join("board.jsonl");
        assert!(read_board(&path).unwrap().is_empty(), "no board yet");
        std::fs::write(&path, "{\"kind\": \"ask\", \"id\": 1}\n{\"kind\": \"as").unwrap();
        assert_eq!(read_board(&path).unwrap().len(), 1);
        append(&path, |_| Ok(json!({"kind": "answer"}))).unwrap();
        let board = read_board(&path).unwrap();
        assert_eq!(board.len(), 2, "the cut line is gone, the new one reads");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"kind\": \"ask\", \"id\": 1}\n{\"kind\":\"answer\"}\n"
        );
        std::fs::write(
            &path,
            "{\"kind\": \"ask\"}\nnot json\n{\"kind\": \"ask\"}\n",
        )
        .unwrap();
        let err = read_board(&path).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        let refused = append(&dir.join("other.jsonl"), |_| bail!("no"));
        assert!(refused.is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("other.jsonl")).unwrap(),
            ""
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_and_missed_questions_and_answer_rates_follow_the_deadlines() {
        let board = vec![
            ask("a-1-1", "a", &["b", "c"], 100, 700),
            answer("a-1-1", "a", "b", false, 120),
            ask("a-2-1", "a", &["b"], 1_000, 1_600),
            ask("c-1-1", "c", &["a"], 200, 300),
            answer("c-1-1", "c", "a", true, 400),
        ];
        let open: Vec<&str> = open_questions(&board, "b", 650)
            .iter()
            .map(|a| text(&a["id"]))
            .collect();
        assert_eq!(open, vec!["a-2-1"], "b answered a-1-1 already");
        assert_eq!(open_questions(&board, "c", 650).len(), 1);
        assert!(open_questions(&board, "c", 700).is_empty(), "due at 700");
        assert_eq!(missed_questions(&board, "c", 700).len(), 1);
        assert_eq!(open_questions(&board, "b", 1_100).len(), 1);
        assert!(
            open_questions(&board, "a", 250).is_empty(),
            "a answered, late"
        );

        let r = rates(&board, 800);
        assert_eq!(
            r["b"],
            Rate {
                to_it: 2,
                on_time: 1,
                open: 1,
                ..Rate::default()
            }
        );
        assert_eq!(
            r["c"],
            Rate {
                asked: 1,
                to_it: 1,
                missed: 1,
                ..Rate::default()
            }
        );
        assert_eq!((r["a"].asked, r["a"].late), (2, 1));
        assert!(r["c"].describe().contains("missed 1"));

        let text = inbox_text(&board, "c", 650);
        assert!(text.contains("Question a-1-1 from a"), "{text}");
        assert!(text.contains("> what about a-1-1?"), "{text}");
        assert!(text.contains("--ask a-1-1"), "{text}");
        let text = inbox_text(&board, "c", 900);
        assert!(text.contains("No open question to c."), "{text}");
        assert!(
            text.contains("Past their deadline without your answer: a-1-1"),
            "{text}"
        );
    }

    #[test]
    fn a_decision_records_its_questions_the_answers_read_and_who_did_not_answer() {
        let board = vec![
            ask("a-3-1", "a", &["b", "c"], 500, 1_100),
            answer("a-3-1", "a", "b", false, 520),
            ask("a-2-1", "a", &["b"], 300, 900),
        ];
        let record = json!({"board_read": [{"key": "answer:a-3-1:b", "as_of": 520},
                                            {"key": "ask:c-1-1", "as_of": 480}]});
        let advice = advice_at_decide(&board, "a", 500, Some(&record));
        assert_eq!(advice.newest_read, Some(520));
        let json = &advice.json;
        assert_eq!(
            json["asked"].as_array().unwrap().len(),
            1,
            "only this look's"
        );
        assert_eq!(json["answers"][0]["agent"], "b");
        assert_eq!(json["answers"][0]["read"], true);
        assert_eq!(json["unanswered"][0]["agent"], "c");
        assert_eq!(json["questions_read"][0], "c-1-1");
        let md = advice_markdown(json);
        assert!(md.contains("Asked b, c (a-3-1): what about a-3-1?"), "{md}");
        assert!(
            md.contains("Answer from b (on time, would hold): b on a-3-1"),
            "{md}"
        );
        assert!(md.contains("No answer from c to a-3-1."), "{md}");

        let nothing = advice_at_decide(&board, "a", 999, None);
        assert!(nothing.is_empty() && nothing.newest_read.is_none());
    }

    #[test]
    fn late_answers_are_shown_once_at_the_askers_next_look() {
        let board = vec![
            ask("a-1-1", "a", &["b"], 100, 200),
            answer("a-1-1", "a", "b", true, 300),
            ask("a-2-1", "a", &["b"], 400, 500),
            answer("a-2-1", "a", "b", false, 450),
        ];
        let late = late_unseen(&board, "a", &[]);
        assert_eq!(late.len(), 1);
        assert_eq!(late[0]["ask"], "a-1-1");
        let own = vec![json!({"advice": {"late_shown": ["answer:a-1-1:b"]}})];
        assert!(late_unseen(&board, "a", &own).is_empty(), "shown once");
        let read =
            vec![json!({"advice": {"answers": [{"ask": "a-1-1", "agent": "b", "read": true}]}})];
        assert!(
            late_unseen(&board, "a", &read).is_empty(),
            "read before deciding"
        );
        assert!(late_unseen(&board, "b", &[]).is_empty(), "only the asker's");
    }

    #[test]
    fn what_an_answerer_would_do_must_be_a_decision() {
        let w = |action: &str, lower: Option<f64>, upper: Option<f64>| Would {
            action: action.into(),
            lower_pct: lower,
            upper_pct: upper,
        };
        assert_eq!(
            w("enter", Some(-3.0), Some(-1.0)).to_json().unwrap()["upper_pct"],
            -1.0
        );
        assert_eq!(
            w("hold", None, None).to_json().unwrap(),
            json!({"action": "hold"})
        );
        assert!(w("enter", Some(-3.0), None).to_json().is_err());
        assert!(w("exit", Some(1.0), Some(2.0)).to_json().is_err());
        assert!(w("buy", None, None).to_json().is_err());
        let text = answer_text(&answer("a-1-1", "a", "b", false, 1));
        assert!(
            text.contains("b (m), on time, would hold, 70% sure:"),
            "{text}"
        );
    }

    #[test]
    fn questions_go_to_other_agents_of_the_same_panel() {
        let agent = |name: &str, folder: &str| Identity {
            name: name.into(),
            model: "m".into(),
            decisions: PathBuf::from(format!("data/agent/{folder}/{name}.jsonl")),
            peers: Vec::new(),
        };
        let panel = vec![
            agent("a", "p"),
            agent("b", "p"),
            agent("c", "p"),
            agent("d", "other"),
        ];
        let a = &panel[0];
        assert_eq!(addressees(a, &panel, "all").unwrap(), vec!["b", "c"]);
        assert_eq!(addressees(a, &panel, "c, b, c").unwrap(), vec!["c", "b"]);
        assert!(addressees(a, &panel, "a")
            .unwrap_err()
            .to_string()
            .contains("that is you"));
        let err = addressees(a, &panel, "d").unwrap_err().to_string();
        assert!(err.contains("other agents are b, c"), "{err}");
        assert!(
            addressees(&panel[3], &panel, "all").is_err(),
            "alone in its folder"
        );
        assert_eq!(
            board_path(&a.decisions),
            PathBuf::from("data/agent/p/board.jsonl")
        );
    }
}

#[cfg(test)]
#[path = "agent_board_tests.rs"]
mod board_tests;
