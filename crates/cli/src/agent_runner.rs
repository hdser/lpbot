//! Durable server coordinator. Models receive only brief + exact CLI output.
//! Operational state, scores and blind-first reviews never enter that prompt.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{agent_cmd, agent_lock, agent_log, codex_worker, run_cmd};

#[derive(ClapArgs)]
pub struct Args {
    pub config: PathBuf,
    /// Validate configuration, checksums, prerequisites and auth without inference or decisions.
    #[arg(long, conflicts_with_all = ["once", "status"])]
    pub check: bool,
    /// Perform one due cycle; otherwise keep polling on the server.
    #[arg(long, conflicts_with = "status")]
    pub once: bool,
    /// Print persisted coordinator state, without starting workers.
    #[arg(long)]
    pub status: bool,
    /// Release persistent ownership only while the server runner is stopped.
    #[arg(long, conflicts_with_all = ["check", "once", "status"])]
    pub release: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub experiment: PathBuf,
    pub experiment_sha256: String,
    pub agents: Vec<Agent>,
    pub state_dir: PathBuf,
    pub codex: PathBuf,
    pub codex_home: PathBuf,
    #[serde(default = "results_default")]
    pub results_dir: PathBuf,
    #[serde(default = "poll_default")]
    pub poll_seconds: u64,
    #[serde(default = "timeout_default")]
    pub timeout_seconds: u64,
    #[serde(default = "disk_default")]
    pub min_disk_gb: u64,
    #[serde(default = "load_default")]
    pub max_load: f64,
    #[serde(default = "temp_default")]
    pub max_cpu_c: f64,
}
fn poll_default() -> u64 {
    30
}
fn timeout_default() -> u64 {
    300
}
fn disk_default() -> u64 {
    150
}
fn load_default() -> f64 {
    4.0
}
fn temp_default() -> f64 {
    80.0
}
fn results_default() -> PathBuf {
    "runs/codex-panel-forward".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    pub name: String,
    pub decisions: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct AgentState {
    decisions: usize,
    next_due: u64,
    stopped: bool,
    phase: String,
    last_job: String,
    error: Option<String>,
    reviewed_day: Option<String>,
    /// Persisted before decide: reconcile canonical decisions after uncertain success.
    attempt: Option<Attempt>,
    pending_review: Option<ReviewTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Attempt {
    count_before: usize,
    look: u64,
    as_of: u64,
    response: Value,
    review: Option<ReviewTask>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReviewTask {
    directory: PathBuf,
    input: String,
    author: Value,
    #[serde(default)]
    observation_day: String,
}

fn mark_reviewed_other(state: &mut AgentState, task: &ReviewTask) {
    if !matches!(task.author["action"].as_str(), Some("enter" | "exit"))
        && !task.observation_day.is_empty()
    {
        state.reviewed_day = Some(task.observation_day.clone());
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    schema: String,
    experiment_sha256: String,
    activated_unix: u64,
    context: String,
    resolved_config_hashes: BTreeMap<String, String>,
    evaluation_day: String,
    evaluation: Option<Value>,
    agents: BTreeMap<String, AgentState>,
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    count: usize,
    last_look: Option<u64>,
    next_due: u64,
}

fn now() -> u64 {
    agent_log::now_unix()
}
fn day() -> String {
    lpbot_data::day::Day::of_time(now()).to_string()
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn fresh_name(label: &str) -> String {
    format!(
        "{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

/// Read canonical decisions, never a cached due time after an interrupted write.
fn snapshot(path: &Path, initial: u64) -> Result<Snapshot> {
    let text = agent_lock::read_optional(path)?;
    if !text.is_empty() && !text.ends_with('\n') {
        bail!("canonical decisions have an unterminated final line; inspection required");
    }
    let decisions = run_cmd::parse_decisions(&text)?;
    if decisions.windows(2).any(|w| w[1].look <= w[0].look) {
        bail!("canonical decisions are not strictly ordered; inspection required");
    }
    Ok(Snapshot {
        count: decisions.len(),
        last_look: decisions.last().map(|d| d.look),
        next_due: decisions.last().map_or(initial, |d| {
            d.at.unwrap_or(d.look).saturating_add(d.next_look)
        }),
    })
}

fn verify_due_ack(text: &str, due: u64) -> Result<()> {
    let expected = format!("Next look due: {}Z", agent_cmd::iso(due));
    if !text.lines().any(|line| line == expected) {
        bail!("decision committed but next-due acknowledgement does not match canonical state; inspect exact output");
    }
    Ok(())
}

fn verify_recorded_output(output: &CliOutput, due: u64) -> Result<bool> {
    if !output.success {
        bail!("decision job failed; inspect its exact output before resuming");
    }
    let stopped = output.text.contains("The loss limit stopped the run")
        || output.text.contains("The window is over");
    if !stopped {
        verify_due_ack(&output.text, due)?;
    }
    Ok(stopped)
}

fn recovered_recording(
    config: &Config,
    agent: &Agent,
    attempt: &Attempt,
    due: u64,
) -> Result<bool> {
    let last = config
        .state_dir
        .join("last")
        .join(format!("{}.json", agent.name));
    let job: Job = serde_json::from_slice(&std::fs::read(last).context(
        "committed attempt lacks its durable last-job pointer; inspect before resuming",
    )?)?;
    let response: codex_worker::Response = serde_json::from_value(attempt.response.clone())?;
    let waiting = agent_cmd::Pending {
        look: attempt.look,
        at: Some(attempt.as_of),
        number: None,
        day: None,
        seen: None,
        record: None,
    };
    if job.args != recording_args(config, agent, &waiting, &response)? {
        bail!("committed attempt's retained job does not match its exact recording arguments");
    }
    let output = wait_job(&job, Duration::from_secs(1))?;
    verify_recorded_output(&output, due)
}

fn save(path: &Path, state: &State) -> Result<()> {
    agent_lock::atomic(path, &serde_json::to_vec_pretty(state)?)
}

fn resolved_hashes(
    config: &Config,
    runs: &[lpbot_core::ResolvedRun],
) -> Result<BTreeMap<String, String>> {
    config
        .agents
        .iter()
        .map(|a| {
            let r = runs
                .iter()
                .find(|r| agent_cmd::identity_of(r).is_ok_and(|i| i.name == a.name))
                .ok_or_else(|| anyhow!("configured identity is missing"))?;
            Ok((a.name.clone(), r.config_hash.clone()))
        })
        .collect()
}

fn verify_resolved(config: &Config, state: &State) -> Result<()> {
    verify_resolved_runs(config, state, &validate(config)?)
}

fn verify_resolved_runs(
    config: &Config,
    state: &State,
    runs: &[lpbot_core::ResolvedRun],
) -> Result<()> {
    if resolved_hashes(config, runs)? != state.resolved_config_hashes {
        bail!("resolved experiment inputs changed, including its base config; refusing to replay or decide the active run");
    }
    Ok(())
}

fn committed_audit(path: &Path, attempt: &Attempt) -> Result<bool> {
    let canonical = agent_lock::read_optional(path)?;
    let Some(line) = canonical.lines().last() else {
        return Ok(false);
    };
    let value: Value = serde_json::from_str(line)?;
    for name in ["action", "next", "note", "lower_pct", "upper_pct"] {
        if value[name] != attempt.response[name] {
            return Ok(false);
        }
    }
    if value["look"].as_u64() != Some(attempt.look) || value["at"].as_u64() != Some(attempt.as_of) {
        return Ok(false);
    }
    let log = agent_lock::read_optional(&agent_log::records_path(path))?;
    if !log.ends_with('\n') {
        return Ok(false);
    }
    let records = agent_log::read_records(&agent_log::records_path(path))?;
    Ok(records.iter().any(|r| {
        r["kind"] == "look"
            && r["look"].as_u64() == Some(attempt.look)
            && r["as_of"].as_u64() == Some(attempt.as_of)
            && r["decision"]["action"] == attempt.response["action"]
            && r["decision"]["next"] == attempt.response["next"]
            && r["decision"]["lower_pct"] == attempt.response["lower_pct"]
            && r["decision"]["upper_pct"] == attempt.response["upper_pct"]
            && r["note"] == attempt.response["note"]
            && r["reasoning"] == attempt.response["reasoning"]
            && r["expect"]["low_pct"] == attempt.response["expect_low"]
            && r["expect"]["high_pct"] == attempt.response["expect_high"]
            && r["expect"]["confidence"] == attempt.response["expect_confidence"]
            && r["expect"]["text"] == attempt.response["expect_text"]
            && r["stance"]["kind"] == attempt.response["stance"]
            && r["stance"]["toward"] == attempt.response["toward"]
    }))
}

fn validate(config: &Config) -> Result<Vec<lpbot_core::ResolvedRun>> {
    if config.agents.is_empty()
        || config.poll_seconds == 0
        || config.timeout_seconds == 0
        || !config.max_load.is_finite()
        || config.max_load <= 0.0
        || !config.max_cpu_c.is_finite()
        || config.max_cpu_c <= 0.0
    {
        bail!("runner needs agents and positive polling, timeout and resource limits");
    }
    if digest(&std::fs::read(&config.experiment)?) != config.experiment_sha256 {
        bail!("immutable experiment checksum mismatch; runner refuses to proceed");
    }
    let runs =
        lpbot_core::resolve_experiment(&config.experiment, &lpbot_core::RunOverrides::default())?;
    let mut names = std::collections::HashSet::new();
    let mut paths = std::collections::HashSet::new();
    for agent in &config.agents {
        if agent.name.is_empty()
            || !agent
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            || !names.insert(&agent.name)
            || !paths.insert(&agent.decisions)
        {
            bail!("runner agent names and decisions paths must be unique and names must be simple handles");
        }
        let run = runs
            .iter()
            .find(|r| agent_cmd::identity_of(r).is_ok_and(|i| i.name == agent.name))
            .ok_or_else(|| anyhow!("agent {} is missing from experiment", agent.name))?;
        let who = agent_cmd::identity_of(run)?;
        if who.decisions != agent.decisions
            || run_cmd::run_window(&run.config)?.1.is_some()
            || run.config.mode != lpbot_core::Mode::Backtest
        {
            bail!("runner requires an open-ended virtual backtest and the exact immutable decisions path for {}", agent.name);
        }
    }
    if std::env::var("RAYON_NUM_THREADS").as_deref() != Ok("2") {
        bail!("set RAYON_NUM_THREADS=2 for the server runner");
    }
    if std::env::var_os("LPBOT_DATA_DIR").is_none() {
        bail!("set LPBOT_DATA_DIR explicitly for the server runner");
    }
    Ok(runs)
}

fn prerequisites(config: &Config) -> Result<()> {
    if !config.codex.is_absolute() {
        bail!("codex must be an absolute, reviewed executable path");
    }
    let version = Command::new(&config.codex)
        .arg("--version")
        .output()
        .context("Codex is not installed at the configured path")?;
    if !version.status.success() {
        bail!("Codex --version failed");
    }
    let features = Command::new(&config.codex)
        .args(["features", "list"])
        .env("CODEX_HOME", &config.codex_home)
        .output()?;
    if !features.status.success() {
        bail!("cannot inspect Codex feature capabilities without inference");
    }
    let supported = String::from_utf8(features.stdout)?;
    for required in [
        "shell_tool",
        "apps",
        "plugins",
        "multi_agent",
        "memories",
        "hooks",
        "goals",
        "view_image",
        "browser_use",
        "computer_use",
        "image_generation",
        "remote_plugin",
        "skill_search",
        "skip_host_skill_discovery",
    ] {
        if !supported
            .lines()
            .any(|l| l.split_whitespace().next() == Some(required))
        {
            bail!("configured Codex CLI lacks required isolation feature {required}; install a compatible pinned version");
        }
    }
    if !Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success()
    {
        bail!("tmux is required for owned long jobs");
    }
    let login = Command::new(&config.codex)
        .args(["login", "status"])
        .env("CODEX_HOME", &config.codex_home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if !login.success() {
        bail!("dedicated server Codex authentication is missing; sign in with CODEX_HOME set to the configured directory, then rerun --check");
    }
    Ok(())
}

/// Fail closed on missing Linux measurements; never use alarm flags as temperatures.
fn resources(config: &Config, root: &Path) -> Result<()> {
    legacy_jobs()?;
    let load: f64 = std::fs::read_to_string("/proc/loadavg")?
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("missing load measurement"))?
        .parse()?;
    if load > config.max_load {
        bail!("deferred: server load {load:.2} exceeds resource gate");
    }
    let disk = Command::new("df").args(["-Pk"]).arg(root).output()?;
    if !disk.status.success() {
        bail!("cannot measure available disk");
    }
    let text = String::from_utf8(disk.stdout)?;
    let free: u64 = text
        .lines()
        .last()
        .and_then(|l| l.split_whitespace().nth(3))
        .ok_or_else(|| anyhow!("missing disk measurement"))?
        .parse()?;
    if free / 1_048_576 < config.min_disk_gb {
        bail!("deferred: available disk is below runner gate");
    }
    let mut temperatures = Vec::new();
    for entry in std::fs::read_dir("/sys/class/hwmon")? {
        let path = entry?.path();
        if std::fs::read_to_string(path.join("name"))
            .unwrap_or_default()
            .trim()
            != "coretemp"
        {
            continue;
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("temp") && name.ends_with("_input") {
                temperatures.push(
                    std::fs::read_to_string(entry.path())?
                        .trim()
                        .parse::<f64>()?
                        / 1000.0,
                );
            }
        }
    }
    if temperatures.is_empty()
        || temperatures
            .iter()
            .any(|t| !t.is_finite() || *t >= config.max_cpu_c)
    {
        bail!("deferred: CPU temperature unavailable or above runner gate");
    }
    let _idle = agent_lock::exclusive(&root.join(".lpbot-operations.lock"), true)
        .context("deferred: another catch-up or heavy history job is running")?;
    Ok(())
}

/// An already-running older CLI cannot acquire locks added by this deployment.
/// Let it finish naturally; never type into or kill Claude's sessions.
#[cfg(target_os = "linux")]
fn legacy_jobs() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let current = std::fs::metadata(std::env::current_exe()?)?;
    for entry in std::fs::read_dir("/proc")? {
        let path = entry?.path();
        if !path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        if std::fs::read_to_string(path.join("comm"))
            .unwrap_or_default()
            .trim()
            != "lpbot"
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path.join("cmdline")) else {
            continue;
        };
        let args: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
        let heavy = matches!(
            args.get(1).copied(),
            Some(
                b"run"
                    | b"calibrate"
                    | b"gates"
                    | b"backfill"
                    | b"gas-history"
                    | b"pool-stats"
                    | b"verify-fees"
            )
        ) || (args.get(1) == Some(&b"agent".as_slice())
            && matches!(
                args.get(2).copied(),
                Some(b"next" | b"decide" | b"evaluate")
            ));
        if !heavy {
            continue;
        }
        let Ok(exe) = std::fs::metadata(path.join("exe")) else {
            continue;
        };
        if (exe.dev(), exe.ino()) != (current.dev(), current.ino()) {
            bail!("deferred: an older lpbot history/agent job is still running; allow existing sessions to drain naturally before shared-lock operation");
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn legacy_jobs() -> Result<()> {
    Ok(())
}

struct CliOutput {
    text: String,
    success: bool,
}

#[derive(Serialize, Deserialize)]
struct Job {
    args: Vec<String>,
    token: String,
    directory: PathBuf,
    session: String,
    data_root: PathBuf,
}

/// tmux children outlive their coordinator. Refuse release until every owned
/// active command, model or maintenance job has a durable exit record.
fn verify_jobs_drained(state_dir: &Path) -> Result<()> {
    let active = state_dir.join("active");
    let entries = match std::fs::read_dir(&active) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("reading owned active jobs"),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let spec: Job = serde_json::from_slice(&std::fs::read(&path)?)?;
        let exit = spec.directory.join("exit.json");
        if !exit.exists() {
            bail!("owned tmux job {} has no durable exit status; drain or inspect it before releasing ownership", spec.session);
        }
        let value: Value = serde_json::from_slice(&std::fs::read(exit)?)?;
        if value["success"].as_bool().is_none() {
            bail!("owned tmux job {} has a damaged exit status", spec.session);
        }
    }
    Ok(())
}

/// Internal tmux worker. No arbitrary executable: only this reviewed lpbot CLI.
pub fn job(path: &Path) -> Result<()> {
    let spec: Job = serde_json::from_slice(&std::fs::read(path)?)?;
    let allowed = match spec.args.first().map(String::as_str) {
        Some("agent") => matches!(
            spec.args.get(1).map(String::as_str),
            Some("next" | "decide" | "brief" | "evaluate")
        ),
        Some("run" | "load-results" | "runner-model-job") => true,
        _ => false,
    };
    if !allowed {
        bail!("runner job command is outside the simulation allowlist");
    }
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(&spec.args)
        .env("LPBOT_RUNNER_TOKEN", &spec.token)
        .env("LPBOT_DATA_DIR", &spec.data_root)
        .env("RAYON_NUM_THREADS", "2")
        .stdout(File::create(spec.directory.join("stdout.txt"))?)
        .stderr(File::create(spec.directory.join("stderr.txt"))?);
    // tmux's pre-existing server does not inherit arbitrary client environment.
    // Read the private coordinator-only database environment without putting
    // credentials in command arguments, job JSON or model environments.
    if spec.args.first().is_some_and(|s| s == "load-results") {
        let env = agent_lock::read_optional(Path::new("data/codex/coordinator.env"))?;
        if let Some(value) = env.lines().find_map(|l| l.strip_prefix("LPBOT_DB_URL=")) {
            command.env("LPBOT_DB_URL", value.trim_matches(['\'', '"']));
        }
    }
    let status = command.status();
    let value = match status {
        Ok(status) => json!({"code":status.code(),"success":status.success()}),
        Err(_) => json!({"code":null,"success":false,"error":"spawn failed"}),
    };
    agent_lock::atomic(
        &spec.directory.join("exit.json"),
        &serde_json::to_vec(&value)?,
    )?;
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn wait_job(spec: &Job, timeout: Duration) -> Result<CliOutput> {
    let started = Instant::now();
    while !spec.directory.join("exit.json").exists() {
        if started.elapsed() > timeout {
            bail!("owned tmux job {} is still running; preserve it and reconcile before another command",spec.session);
        }
        let alive = Command::new("tmux")
            .args(["has-session", "-t", &spec.session])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !alive.success() && !spec.directory.join("exit.json").exists() {
            bail!("owned tmux job {} ended without exit status; inspect its exact output before continuing",spec.session);
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let exit: Value = serde_json::from_slice(&std::fs::read(spec.directory.join("exit.json"))?)?;
    Ok(CliOutput {
        text: agent_lock::read_optional(&spec.directory.join("stdout.txt"))?,
        success: exit["success"] == true,
    })
}

fn cli(
    config: &Config,
    agent: &Agent,
    token: &str,
    label: &str,
    args: &[String],
) -> Result<CliOutput> {
    let active = config
        .state_dir
        .join("active")
        .join(format!("{}.json", agent.name));
    let timeout = Duration::from_secs(config.timeout_seconds.saturating_mul(4).saturating_add(120));
    if active.exists() {
        let previous: Job = serde_json::from_slice(&std::fs::read(&active)?)?;
        let result = wait_job(&previous, timeout)?;
        agent_lock::atomic(
            &config
                .state_dir
                .join("last")
                .join(format!("{}.json", agent.name)),
            &serde_json::to_vec(&previous)?,
        )?;
        std::fs::remove_file(&active)?;
        if previous.args == args {
            return Ok(result);
        }
    }
    let session = fresh_name(&format!("codex-runner-{}-{label}", agent.name));
    let directory = config.state_dir.join("jobs").join(&session);
    std::fs::create_dir_all(&directory)?;
    let directory = std::fs::canonicalize(directory)?;
    let spec = Job {
        args: args.to_vec(),
        token: token.into(),
        directory: directory.clone(),
        session,
        data_root: crate::backfill_cmd::data_dir(None),
    };
    let spec_path = directory.join("job.json");
    agent_lock::atomic(&spec_path, &serde_json::to_vec(&spec)?)?;
    agent_lock::atomic(&active, &serde_json::to_vec(&spec)?)?;
    let binary = std::env::current_exe()?;
    let command = format!(
        "{} runner-job {}",
        shell_quote(&binary),
        shell_quote(&spec_path)
    );
    let started = Command::new("tmux")
        .args(["new-session", "-d", "-s", &spec.session, "-c"])
        .arg(std::env::current_dir()?)
        .arg(command)
        .status()?;
    if !started.success() {
        bail!(
            "could not create owned tmux job {}; inspect its active record",
            spec.session
        );
    }
    let result = wait_job(&spec, timeout)?;
    agent_lock::atomic(
        &config
            .state_dir
            .join("last")
            .join(format!("{}.json", agent.name)),
        &serde_json::to_vec(&spec)?,
    )?;
    std::fs::remove_file(active)?;
    Ok(result)
}

fn agent_args(config: &Config, agent: &Agent, command: &str) -> Vec<String> {
    vec![
        "agent".into(),
        command.into(),
        config.experiment.to_string_lossy().into_owned(),
        "--agent".into(),
        agent.name.clone(),
    ]
}

#[derive(Serialize, Deserialize)]
struct ModelJob {
    codex: PathBuf,
    home: PathBuf,
    artifacts: PathBuf,
    prompt: String,
    timeout: u64,
    schema: Option<Value>,
}

pub fn model_job(path: &Path) -> Result<()> {
    let spec: ModelJob = serde_json::from_slice(&std::fs::read(path)?)?;
    let timeout = Duration::from_secs(spec.timeout);
    let value = if let Some(schema) = &spec.schema {
        codex_worker::run_json(
            &spec.codex,
            &spec.home,
            &spec.artifacts,
            &spec.prompt,
            timeout,
            schema,
        )?
    } else {
        response_json(&codex_worker::run(
            &spec.codex,
            &spec.home,
            &spec.artifacts,
            &spec.prompt,
            timeout,
        )?)
    };
    println!("{value}");
    Ok(())
}

fn infer(config: &Config, prompt: &str, artifacts: &Path, schema: Option<Value>) -> Result<Value> {
    let path = config
        .state_dir
        .join("model-inputs")
        .join(format!("{}.json", fresh_name("model")));
    agent_lock::atomic(
        &path,
        &serde_json::to_vec(&ModelJob {
            codex: config.codex.clone(),
            home: config.codex_home.clone(),
            artifacts: artifacts.into(),
            prompt: prompt.into(),
            timeout: config.timeout_seconds,
            schema,
        })?,
    )?;
    let path = std::fs::canonicalize(path)?;
    let dummy = Agent {
        name: "model".into(),
        decisions: PathBuf::new(),
    };
    let out = cli(
        config,
        &dummy,
        "",
        "inference",
        &[
            "runner-model-job".into(),
            path.to_string_lossy().into_owned(),
        ],
    )?;
    if !out.success {
        bail!(
            "isolated model job failed; inspect its retained stdout, stderr and worker artifacts"
        );
    }
    serde_json::from_str(&out.text).context("model job returned malformed final JSON")
}

fn infer_decision(
    config: &Config,
    input: &str,
    artifacts: &Path,
) -> Result<codex_worker::Response> {
    Ok(serde_json::from_value(infer(
        config, input, artifacts, None,
    )?)?)
}

fn prompt(brief: &str, look: &str) -> String {
    format!("{brief}\n\nThis panel uses passive peer records only. Never ask/inbox/answer/answers or use board advice. You receive no tools. Decide only from the exact permitted output below; do not seek files, prices, news or reviews. Cadence is your choice, at least 15m. Return the requested JSON; retain your note and reasoning exactly.\n\n{look}")
}

fn response_json(r: &codex_worker::Response) -> Value {
    json!({"action":r.action,"next":r.next,"note":r.note,"reasoning":r.reasoning,"expect_low":r.expect_low,"expect_high":r.expect_high,"expect_confidence":r.expect_confidence,"expect_text":r.expect_text,"lower_pct":r.lower_pct,"upper_pct":r.upper_pct,"stance":r.stance,"toward":r.toward})
}

fn recording_args(
    config: &Config,
    agent: &Agent,
    waiting: &agent_cmd::Pending,
    r: &codex_worker::Response,
) -> Result<Vec<String>> {
    let mut args = agent_args(config, agent, "decide");
    for (key, value) in [
        ("--action", r.action.clone()),
        ("--next", r.next.clone()),
        ("--expected-look", waiting.look.to_string()),
        (
            "--expected-as-of",
            waiting
                .at
                .ok_or_else(|| anyhow!("pending look has no information boundary"))?
                .to_string(),
        ),
    ] {
        args.push(key.into());
        args.push(value);
    }
    args.extend([
        format!("--note={}", r.note),
        format!("--reasoning={}", r.reasoning),
        format!("--expect={}", r.expect_text),
    ]);
    args.extend([
        format!("--expect-low={}", r.expect_low),
        format!("--expect-high={}", r.expect_high),
        format!("--expect-confidence={}", r.expect_confidence),
    ]);
    if let Some(v) = r.lower_pct {
        args.push(format!("--lower={v}"));
    }
    if let Some(v) = r.upper_pct {
        args.push(format!("--upper={v}"));
    }
    if let Some(v) = &r.stance {
        args.extend(["--stance".into(), v.clone()]);
    }
    if let Some(v) = &r.toward {
        args.extend(["--toward".into(), v.clone()]);
    }
    Ok(args)
}

/// Blind first, durably sealed before the author's response enters stage two.
fn review(config: &Config, task: &ReviewTask) -> Result<()> {
    let dir = &task.directory;
    let input = &task.input;
    std::fs::create_dir_all(dir)?;
    let stage1 = if dir.join("stage1.json").exists() {
        let bytes = std::fs::read(dir.join("stage1.json"))?;
        if agent_lock::read_optional(&dir.join("stage1.sha256"))? != digest(&bytes) {
            bail!("blind review seal is missing or changed; inspection required");
        }
        bytes
    } else {
        let own = infer_decision(config,&format!("Manual independent blind review, stage 1 (not a built-in agent review command). Form your own decision before any author response or outcome is disclosed.\n\n{input}"),dir)?;
        let bytes = serde_json::to_vec_pretty(&response_json(&own))?;
        agent_lock::atomic(&dir.join("stage1.json"), &bytes)?;
        agent_lock::atomic(&dir.join("stage1.sha256"), digest(&bytes).as_bytes())?;
        bytes
    };
    let schema = json!({"type":"object","properties":{"assessment":{"type":"string"},"justification":{"type":"string"},"limitations":{"type":"string"}},"required":["assessment","justification","limitations"],"additionalProperties":false});
    if !dir.join("stage2.json").exists() {
        let stage2 = infer(config,&format!("Manual independent review, stage 2. No outcome is provided. Assess the author's reasoning against the permitted look and your preserved independent assessment.\n\n{input}\n\nYour preserved stage 1:\n{}\n\nAuthor response:\n{}",String::from_utf8(stage1)?,task.author),dir,Some(schema))?;
        agent_lock::atomic(
            &dir.join("stage2.json"),
            &serde_json::to_vec_pretty(&stage2)?,
        )?;
    }
    Ok(())
}

fn cycle(
    config: &Config,
    agent: &Agent,
    token: &str,
    initial: u64,
    state: &mut AgentState,
    state_path: &Path,
    all: &mut State,
) -> Result<()> {
    // A durable tmux child can still be committing after its daemon died.
    // Drain it before inspecting counts or discarding an uncertain attempt.
    let active = config
        .state_dir
        .join("active")
        .join(format!("{}.json", agent.name));
    if active.exists() {
        let previous: Job = serde_json::from_slice(&std::fs::read(&active)?)?;
        let output = wait_job(
            &previous,
            Duration::from_secs(config.timeout_seconds.saturating_mul(4).saturating_add(120)),
        )?;
        agent_lock::atomic(
            &config
                .state_dir
                .join("last")
                .join(format!("{}.json", agent.name)),
            &serde_json::to_vec(&previous)?,
        )?;
        std::fs::remove_file(active)?;
        if state.attempt.is_some()
            && previous
                .args
                .get(1)
                .is_some_and(|command| command == "decide")
            && !output.success
        {
            state.stopped = true;
            state.phase = "interrupted_decide_failed_inspection_required".into();
            bail!("interrupted decide has failed; inspect its retained output before resuming");
        }
    }
    let canonical = snapshot(&agent.decisions, initial)?;
    if let Some(attempt) = &state.attempt {
        if canonical.count == attempt.count_before + 1 && canonical.last_look == Some(attempt.look)
        {
            if !committed_audit(&agent.decisions, attempt)? {
                state.stopped = true;
                state.phase = "committed_without_complete_audit".into();
                bail!("decision committed but its matching reasoning record is missing; preserve the attempt and inspect before resuming");
            }
            match recovered_recording(config, agent, attempt, canonical.next_due) {
                Ok(stopped) => state.stopped |= stopped,
                Err(error) => {
                    state.stopped = true;
                    state.phase = "recovered_decide_inspection_required".into();
                    return Err(error);
                }
            }
            state.pending_review = attempt.review.clone();
            state.attempt = None;
            state.phase = "reconciled_committed_decision".into();
        } else if canonical.count != attempt.count_before {
            bail!("canonical decisions changed unexpectedly after an attempted recording; inspect before continuing");
        } else {
            // Never reuse a response after an uncertain failure. The next command
            // moves the pending observation to the current head.
            state.attempt = None;
            state.phase = "refresh_after_uncommitted_attempt".into();
        }
    }
    state.decisions = canonical.count;
    state.next_due = canonical.next_due;
    if let Some(task) = &state.pending_review {
        review(config, task)?;
        let task = task.clone();
        mark_reviewed_other(state, &task);
        state.pending_review = None;
        state.phase = "review_completed".into();
        all.agents.insert(agent.name.clone(), state.clone());
        save(state_path, all)?;
    }
    if state.stopped {
        return Ok(());
    }
    if now() < state.next_due {
        return Ok(());
    }
    resources(config, &crate::backfill_cmd::data_dir(None))?;
    let next = cli(
        config,
        agent,
        token,
        "next",
        &agent_args(config, agent, "next"),
    )?;
    if !next.success {
        bail!("agent next failed; exact output is retained in runner jobs");
    }
    if next.text.contains("The loss limit stopped the run")
        || next.text.contains("The window is over")
    {
        state.stopped = true;
        state.phase = "stopped".into();
        return Ok(());
    }
    let waiting = match agent_cmd::read_pending(&agent.decisions) {
        Ok(pending) => pending,
        Err(_)
            if next.text.contains("No look is due yet")
                || next.text.contains("does not reach its block yet") =>
        {
            state.phase = "waiting_for_head".into();
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if !next
        .text
        .lines()
        .any(|l| l.starts_with("Look ") && l.contains(", day "))
    {
        bail!("next returned no verified sanitized look; refusing a model call");
    }
    let brief = cli(
        config,
        agent,
        token,
        "brief",
        &agent_args(config, agent, "brief"),
    )?;
    if !brief.success {
        bail!("agent brief failed");
    }
    let input = prompt(&brief.text, &next.text);
    let answer = infer_decision(config, &input, &config.state_dir.join("workers"))?;
    let at = waiting
        .at
        .ok_or_else(|| anyhow!("pending information boundary missing"))?;
    let observation_day = lpbot_data::day::Day::of_time(at).to_string();
    let needs_review = matches!(answer.action.as_str(), "enter" | "exit")
        || state.reviewed_day.as_deref() != Some(&observation_day);
    let task = needs_review.then(|| ReviewTask {
        directory: Path::new("data/codex/reviews").join(fresh_name(&format!(
            "server-{}-look{}",
            agent.name,
            waiting.number.unwrap_or(0)
        ))),
        input,
        author: response_json(&answer),
        observation_day,
    });
    // Peer stance validation happens before the durable attempt or write.
    let run =
        lpbot_core::resolve_experiment(&config.experiment, &lpbot_core::RunOverrides::default())?
            .into_iter()
            .find(|r| agent_cmd::identity_of(r).is_ok_and(|i| i.name == agent.name))
            .ok_or_else(|| anyhow!("identity disappeared"))?;
    let who = agent_cmd::identity_of(&run)?;
    let shown = waiting
        .record
        .as_ref()
        .map(agent_log::shown_from)
        .unwrap_or_default();
    agent_log::stance(
        answer.stance.as_deref(),
        answer.toward.as_deref(),
        &shown,
        &who.peers,
    )?;
    state.attempt = Some(Attempt {
        count_before: canonical.count,
        look: waiting.look,
        as_of: at,
        response: response_json(&answer),
        review: task.clone(),
    });
    state.phase = "recording".into();
    all.agents.insert(agent.name.clone(), state.clone());
    save(state_path, all)?;
    // Check the immutable bytes again after model/review latency.
    verify_resolved(config, all)?;
    if now().saturating_sub(at) > 300 {
        bail!("response became stale; obtain a new head look on the next cycle");
    }
    let recorded = cli(
        config,
        agent,
        token,
        "decide",
        &recording_args(config, agent, &waiting, &answer)?,
    )?;
    let after = snapshot(&agent.decisions, initial)?;
    if after.count != canonical.count + 1 || after.last_look != Some(waiting.look) {
        bail!(
            "decide was not verified as committed; preserve attempt and resume through agent next"
        );
    }
    if !committed_audit(&agent.decisions, state.attempt.as_ref().unwrap())? {
        state.stopped = true;
        state.phase = "committed_without_complete_audit".into();
        bail!("committed decision has no matching complete reasoning record");
    }
    let stopped = match verify_recorded_output(&recorded, after.next_due) {
        Ok(stopped) => stopped,
        Err(error) => {
            state.stopped = true;
            state.phase = "committed_decide_inspection_required".into();
            return Err(error);
        }
    };
    state.attempt = None;
    state.pending_review = task;
    state.decisions = after.count;
    state.next_due = after.next_due;
    state.phase = "waiting".into();
    state.error = None;
    state.stopped = stopped;
    all.agents.insert(agent.name.clone(), state.clone());
    save(state_path, all)?;
    if let Some(task) = &state.pending_review {
        review(config, task)?;
        let task = task.clone();
        mark_reviewed_other(state, &task);
        state.pending_review = None;
        state.phase = "waiting".into();
    }
    Ok(())
}

/// Canonical decisions are enough to decide whether scoring can be useful;
/// prior score files must never be a prerequisite for producing those scores.
fn daily_solo_decisions(config: &Config) -> Result<Vec<lpbot_sim::agent::AgentDecision>> {
    let solo = config
        .agents
        .iter()
        .find(|agent| agent.name == "solo")
        .ok_or_else(|| anyhow!("daily evaluation requires the configured solo identity"))?;
    let text = agent_lock::read_optional(&solo.decisions)?;
    if !text.is_empty() && !text.ends_with('\n') {
        bail!("canonical solo decisions have an unterminated final line; inspection required");
    }
    let decisions = run_cmd::parse_decisions(&text)?;
    if decisions
        .windows(2)
        .any(|pair| pair[1].look <= pair[0].look)
    {
        bail!("canonical solo decisions are not strictly ordered; inspection required");
    }
    Ok(decisions)
}

/// Wall-clock age is only a cheap necessary condition. The scoring pass
/// below proves that the stored history actually covers the full interval.
fn daily_candidates(decisions: &[lpbot_sim::agent::AgentDecision], at: u64) -> (usize, bool) {
    let mut count = 0;
    let mut entry = false;
    for decision in decisions {
        if at
            .checked_sub(decision.at.unwrap_or(decision.look))
            .is_some_and(|age| age >= 86_400)
        {
            count += 1;
            entry |= matches!(decision.action, lpbot_sim::agent::AgentAction::Enter { .. });
        }
    }
    (count, entry)
}

fn daily_score_complete(score: &Value) -> bool {
    if score["kind"] != "eval" || score["complete"].as_bool() != Some(true) {
        return false;
    }
    let Some((start, end)) = score["decided_as_of"].as_u64().zip(score["as_of"].as_u64()) else {
        return false;
    };
    let Some(seconds) = end.checked_sub(start).filter(|seconds| *seconds > 0) else {
        return false;
    };
    if score["scored_seconds"].as_u64() != Some(seconds) {
        return false;
    }
    match score["horizon"].as_str() {
        Some("1d") => score["horizon_seconds"].as_u64() == Some(86_400) && seconds >= 86_400,
        Some("next") => {
            score["horizon_seconds"].as_u64() == Some(seconds)
                && score["ended"] == "look"
                && score["next_as_of"].as_u64() == Some(end)
                && score["look"]
                    .as_u64()
                    .zip(score["next_look"].as_u64())
                    .is_some_and(|(look, next)| next > look)
        }
        _ => false,
    }
}

/// Count each canonical solo decision once, including an entry only if
/// that entry's own day is complete. Advertised duration alone is no proof.
fn daily_maturity(
    decisions: &[lpbot_sim::agent::AgentDecision],
    scores: &[Value],
) -> (usize, bool) {
    let canonical: BTreeMap<_, _> = decisions
        .iter()
        .map(|decision| (decision.look, decision))
        .collect();
    let mut mature = BTreeMap::new();
    for score in scores {
        if score["agent"] != "solo" || score["horizon"] != "1d" || !daily_score_complete(score) {
            continue;
        }
        let Some(decision) = score["look"].as_u64().and_then(|look| canonical.get(&look)) else {
            continue;
        };
        if score["decision"]["action"].as_str() != Some(decision.action.name())
            || score["decided_as_of"]
                .as_u64()
                .is_none_or(|start| start < decision.at.unwrap_or(decision.look))
        {
            continue;
        }
        mature.insert(
            decision.look,
            matches!(decision.action, lpbot_sim::agent::AgentAction::Enter { .. }),
        );
    }
    (mature.len(), mature.values().any(|entry| *entry))
}

fn daily_horizon_counts(scores: &[Value]) -> Value {
    let mut counts = json!({});
    for horizon in ["next", "1d"] {
        let rows: Vec<_> = scores
            .iter()
            .filter(|score| score["kind"] == "eval" && score["horizon"] == horizon)
            .collect();
        let complete = rows
            .iter()
            .filter(|score| daily_score_complete(score))
            .count();
        counts[horizon] = json!({"complete":complete,"unfinished":rows.len()-complete});
    }
    counts
}

fn reserve_daily(config: &Config, state: &mut State, today: &str) -> Result<()> {
    // A failed or uncertain scoring/reporting job is inspected rather than
    // repeatedly started on every poll, including an immature scoring pass.
    state.evaluation_day = today.into();
    state.evaluation = Some(json!({"status":"reserved","day":today}));
    save(&config.state_dir.join("state.json"), state)
}

fn daily(config: &Config, state: &mut State) -> Result<()> {
    let today = day();
    if state.evaluation_day == today {
        return Ok(());
    }
    verify_resolved(config, state)?;
    resources(config, &crate::backfill_cmd::data_dir(None))?;
    let decisions = daily_solo_decisions(config)?;
    let (candidates, entry) = daily_candidates(&decisions, now());
    if candidates < 5 || !entry {
        bail!(
            "daily evaluation deferred: require five solo decisions aged one day including an entry before scoring"
        );
    }
    reserve_daily(config, state, &today)?;
    let dummy = &config.agents[0];
    let token = "";
    let experiment = config.experiment.to_string_lossy().into_owned();
    let output = cli(
        config,
        dummy,
        token,
        "evaluate",
        &[
            "agent".into(),
            "evaluate".into(),
            experiment.clone(),
            "--horizons".into(),
            "next,1d".into(),
            "--jobs".into(),
            "2".into(),
        ],
    )?;
    if !output.success {
        bail!("daily evaluation failed; inspect retained jobs before retrying");
    }
    let mut counts = json!({});
    let mut solo_scores = Vec::new();
    for agent in &config.agents {
        let scores = agent_log::read_records(&agent_log::scores_path(&agent.decisions))?;
        counts[&agent.name] = daily_horizon_counts(&scores);
        if agent.name == "solo" {
            solo_scores = scores;
        }
    }
    let (mature, entry) = daily_maturity(&decisions, &solo_scores);
    if mature < 5 || !entry {
        state.evaluation = Some(json!({
            "status":"scored_waiting_for_maturity", "day":today,
            "maturity_counts":counts, "full_solo_one_day_horizons":mature,
            "full_solo_entry":entry, "evaluate_stdout":output.text,
            "limitations":"Stored history does not yet prove five full solo one-day horizons including an entry. Terminal partial horizons are not complete; scoring will be reconsidered next UTC day.",
        }));
        return Ok(());
    }
    let results = config.results_dir.to_string_lossy().into_owned();
    let replay = cli(
        config,
        dummy,
        token,
        "replay",
        &[
            "run".into(),
            experiment,
            "--results-dir".into(),
            results.clone(),
        ],
    )?;
    if !replay.success {
        bail!("daily reporting replay failed");
    }
    let loaded = cli(
        config,
        dummy,
        token,
        "load",
        &["load-results".into(), "--results-dir".into(), results],
    )?;
    if !loaded.success {
        bail!("daily dashboard load failed; configure LPBOT_DB_URL for coordinator only");
    }
    state.evaluation = Some(
        json!({"status":"loaded","day":today,"maturity_counts":counts,"full_solo_one_day_horizons":mature,"full_solo_entry":entry,"evaluate_stdout":output.text,"replay_stdout":replay.text,"load_stdout":loaded.text,"limitations":"Terminal partial horizons are not complete; summaries may include them. A few decisions do not establish an edge."}),
    );
    Ok(())
}

#[cfg(test)]
#[path = "agent_runner_daily_tests.rs"]
mod daily_tests;

pub fn run(args: &Args) -> Result<()> {
    let config: Config = serde_yaml::from_str(&std::fs::read_to_string(&args.config)?)?;
    let runs = validate(&config)?;
    let state_path = config.state_dir.join("state.json");
    if args.status {
        println!("{}", agent_lock::read_optional(&state_path)?);
        return Ok(());
    }
    if args.release {
        let _coordinator = agent_lock::exclusive(&config.state_dir.join("coordinator.lock"), true)?;
        verify_jobs_drained(&config.state_dir)?;
        let mut leases = Vec::new();
        for agent in &config.agents {
            leases.push(agent_lock::exclusive(
                &agent_lock::sibling(&agent.decisions, ".runner.lock"),
                true,
            )?);
        }
        let mut commands = Vec::new();
        for agent in &config.agents {
            commands.push(agent_lock::exclusive(
                &agent_lock::sibling(&agent.decisions, ".command.lock"),
                true,
            )?);
        }
        // Validate every fence before changing any identity's ownership.
        let mut owners = Vec::new();
        for agent in &config.agents {
            let owner = agent_lock::sibling(&agent.decisions, ".runner-owner.json");
            if owner.exists() {
                let v: Value = serde_json::from_slice(&std::fs::read(&owner)?)?;
                if v["experiment_sha256"] != config.experiment_sha256 {
                    bail!("ownership fence belongs to a different experiment");
                }
                owners.push(owner);
            }
        }
        for owner in owners {
            std::fs::remove_file(owner)?;
        }
        println!(
            "Runner ownership released. Reconcile canonical state before enabling another writer."
        );
        return Ok(());
    }
    prerequisites(&config)?;
    resources(&config, &crate::backfill_cmd::data_dir(None))?;
    if args.check {
        println!("Configuration, immutable identities, Codex authentication and resource gates verified. No inference, synchronization or decision ran.");
        return Ok(());
    }
    std::fs::create_dir_all(&config.state_dir)?;
    let _coordinator = agent_lock::exclusive(&config.state_dir.join("coordinator.lock"), true)?;
    let mut state: State = if state_path.exists() {
        serde_json::from_slice(&std::fs::read(&state_path)?)?
    } else {
        State { schema:"lpbot.codex-runner/1".into(),experiment_sha256:config.experiment_sha256.clone(),activated_unix:now(),context:"Fresh isolated Codex CLI contexts. Runtime/context cutover recorded; codex-inherited experiment metadata retained.".into(),resolved_config_hashes:resolved_hashes(&config,&runs)?,evaluation_day:day(),evaluation:Some(json!({"status":"activation_day_skipped","reason":"Avoid repeating an earlier coordinator evaluation on the cutover day; first automatic evaluation is next UTC day."})),agents:BTreeMap::new() }
    };
    if state.experiment_sha256 != config.experiment_sha256 {
        bail!("coordinator state belongs to another immutable experiment");
    }
    verify_resolved(&config, &state)?;
    let mut leases = Vec::new();
    let mut tokens = BTreeMap::new();
    let mut initial = BTreeMap::new();
    for agent in &config.agents {
        let lease =
            agent_lock::exclusive(&agent_lock::sibling(&agent.decisions, ".runner.lock"), true)?;
        let _command = agent_lock::exclusive(
            &agent_lock::sibling(&agent.decisions, ".command.lock"),
            false,
        )?;
        let owner = agent_lock::sibling(&agent.decisions, ".runner-owner.json");
        if owner.exists() {
            let v: Value = serde_json::from_slice(&std::fs::read(&owner)?)?;
            if v["experiment_sha256"] != config.experiment_sha256 {
                bail!("existing runner fence belongs to another experiment");
            }
        }
        let token = digest(fresh_name(&agent.name).as_bytes());
        agent_lock::atomic(
            &owner,
            &serde_json::to_vec(
                &json!({"schema":"lpbot.runner-owner/1","token":token,"agent":agent.name,"experiment_sha256":config.experiment_sha256,"activated_unix":now()}),
            )?,
        )?;
        tokens.insert(agent.name.clone(), token);
        leases.push(lease);
        let run = runs
            .iter()
            .find(|r| agent_cmd::identity_of(r).is_ok_and(|i| i.name == agent.name))
            .unwrap();
        initial.insert(
            agent.name.clone(),
            run_cmd::run_window(&run.config)?.0.start(),
        );
    }
    // Default dashboard discovery must not evaluate this panel hourly.
    let stem = config
        .experiment
        .file_stem()
        .ok_or_else(|| anyhow!("experiment has no stem"))?;
    agent_lock::atomic(
        &Path::new("data/codex/runner-managed").join(stem),
        config.experiment_sha256.as_bytes(),
    )?;
    save(&state_path, &state)?;
    loop {
        verify_resolved(&config, &state)?;
        for agent in &config.agents {
            let mut current = state.agents.remove(&agent.name).unwrap_or_default();
            let result = cycle(
                &config,
                agent,
                &tokens[&agent.name],
                initial[&agent.name],
                &mut current,
                &state_path,
                &mut state,
            );
            if let Err(error) = result {
                current.error = Some(format!("{error:#}"));
                if current.phase.is_empty() {
                    current.phase = "deferred".into();
                }
                eprintln!("{}: {error:#}", agent.name);
            }
            let last = config
                .state_dir
                .join("last")
                .join(format!("{}.json", agent.name));
            if last.exists() {
                let job: Job = serde_json::from_slice(&std::fs::read(last)?)?;
                current.last_job = job.directory.to_string_lossy().into_owned();
            }
            state.agents.insert(agent.name.clone(), current);
            save(&state_path, &state)?;
        }
        if let Err(error) = daily(&config, &mut state) {
            state.evaluation = Some(
                json!({"status":"failed_or_deferred","error":format!("{error:#}"),"day":day()}),
            );
            eprintln!("daily evaluation: {error:#}");
        }
        save(&state_path, &state)?;
        if args.once {
            break;
        }
        std::thread::sleep(Duration::from_secs(config.poll_seconds));
    }
    drop(leases);
    Ok(())
}

#[cfg(test)]
#[path = "agent_runner_tests.rs"]
mod recovery_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_due_uses_observation_boundary_not_write_clock() {
        let dir = std::env::temp_dir().join(fresh_name("runner-recovery"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("solo.jsonl");
        std::fs::write(&path,"{\"look\":100,\"at\":200,\"decided\":999,\"action\":\"stay_out\",\"next\":\"2h\",\"note\":\"exact\"}\n").unwrap();
        assert_eq!(
            snapshot(&path, 0).unwrap(),
            Snapshot {
                count: 1,
                last_look: Some(100),
                next_due: 7400
            }
        );
        std::fs::write(
            &path,
            "{\"look\":100,\"action\":\"stay_out\",\"next\":\"2h\"}",
        )
        .unwrap();
        assert!(snapshot(&path, 0).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_or_replaced_decider_inputs_are_refused_before_commit() {
        let waiting = agent_cmd::Pending {
            look: 100,
            at: Some(200),
            number: Some(1),
            day: Some(0.0),
            seen: None,
            record: None,
        };
        assert!(agent_cmd::verify_pending_context(&waiting, 100, 200, 500, 300).is_ok());
        assert!(agent_cmd::verify_pending_context(&waiting, 100, 200, 501, 300).is_err());
        assert!(agent_cmd::verify_pending_context(&waiting, 100, 199, 250, 300).is_err());
        assert!(agent_cmd::verify_pending_context(&waiting, 101, 200, 250, 300).is_err());
        assert!(agent_cmd::verify_pending_context(&waiting, 100, 200, 199, 300).is_err());
    }
}
