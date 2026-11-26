//! One-shot, tools-off Codex CLI transport for a clean LP decider.
//!
//! The caller supplies only the generated brief and permitted `lpbot agent`
//! output. Every invocation starts a new session outside the repository. Raw
//! subprocess output stays in a fresh job directory, including on failure.

use std::env;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MIN_CADENCE: u64 = 15 * 60;
static JOB_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A decider's response. Strings are retained exactly as the CLI returned them.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub action: String,
    pub next: String,
    pub note: String,
    pub reasoning: String,
    pub expect_low: f64,
    pub expect_high: f64,
    pub expect_confidence: f64,
    pub expect_text: String,
    pub lower_pct: Option<f64>,
    pub upper_pct: Option<f64>,
    pub stance: Option<String>,
    pub toward: Option<String>,
}

impl Response {
    fn check(&self) -> Result<()> {
        if !matches!(self.action.as_str(), "stay_out" | "hold" | "exit" | "enter") {
            bail!("Codex response has an invalid action");
        }
        match (self.action.as_str(), self.lower_pct, self.upper_pct) {
            ("enter", Some(low), Some(high))
                if low.is_finite() && high.is_finite() && low > -100.0 && low < high => {}
            ("enter", _, _) => bail!("Codex entry needs finite increasing bounds above -100%"),
            (_, None, None) => {}
            _ => bail!("Codex response supplies entry bounds for a non-entry action"),
        }
        if crate::run_cmd::parse_duration(&self.next).is_none_or(|n| n < MIN_CADENCE) {
            bail!("Codex next interval must be a duration of at least 15m");
        }
        crate::agent_log::Expectation {
            low_pct: Some(self.expect_low),
            high_pct: Some(self.expect_high),
            confidence: Some(self.expect_confidence),
            text: self.expect_text.clone(),
        }
        .check()
        .context("invalid Codex expectation")?;
        match (self.stance.as_deref(), self.toward.as_deref()) {
            (None | Some("own"), None) => {}
            (Some("copy" | "adapt" | "depart"), Some(peer)) if !peer.trim().is_empty() => {}
            _ => bail!("Codex stance must be own, or copy/adapt/depart with a peer"),
        }
        if self.note.trim().is_empty() || self.reasoning.trim().is_empty() {
            bail!("Codex response needs a note and reasoning");
        }
        Ok(())
    }
}

fn decision_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "action": {"type": "string", "enum": ["stay_out", "hold", "exit", "enter"]},
            "next": {"type": "string"},
            "note": {"type": "string"},
            "reasoning": {"type": "string"},
            "expect_low": {"type": "number"},
            "expect_high": {"type": "number"},
            "expect_confidence": {"type": "number", "minimum": 0, "maximum": 1},
            "expect_text": {"type": "string"},
            "lower_pct": {"type": ["number", "null"]},
            "upper_pct": {"type": ["number", "null"]},
            "stance": {"type": ["string", "null"], "enum": ["own", "copy", "adapt", "depart", null]},
            "toward": {"type": ["string", "null"]}
        },
        "required": ["action", "next", "note", "reasoning", "expect_low", "expect_high",
                     "expect_confidence", "expect_text", "lower_pct", "upper_pct", "stance", "toward"]
    })
}

/// Runs one clean decider and validates its structured decision.
pub fn run(
    codex: &Path,
    expected_sha256: &str,
    codex_home: &Path,
    work_dir: &Path,
    prompt: &str,
    timeout: Duration,
) -> Result<Response> {
    let value = run_json(
        codex,
        expected_sha256,
        codex_home,
        work_dir,
        prompt,
        timeout,
        &decision_schema(),
    )?;
    let response: Response =
        serde_json::from_value(value).context("invalid Codex decision JSON")?;
    response.check()?;
    Ok(response)
}

/// Runs a new tools-off session with a caller-supplied output schema.
///
/// Supports closed object schemas, arrays, scalar types, `enum`, `anyOf`,
/// `minimum` and `maximum`. All schema assertions are checked locally too.
/// The caller supplies an isolated, authenticated CODEX_HOME and an artifact
/// directory. The actual CLI cwd, HOME and TMPDIR live in a new owned temporary
/// directory, removed after the invocation. No home contents are copied.
pub fn run_json(
    codex: &Path,
    expected_sha256: &str,
    codex_home: &Path,
    work_dir: &Path,
    prompt: &str,
    timeout: Duration,
    schema: &Value,
) -> Result<Value> {
    if timeout.is_zero() {
        bail!("Codex timeout must be greater than zero");
    }
    check_schema(schema)?;
    if !codex.is_absolute() {
        bail!("Codex executable must be an absolute path; resolve it before starting a worker");
    }
    verify_host_digest(codex, expected_sha256)?;
    let codex_home = fs::canonicalize(codex_home).context("open isolated Codex home")?;
    let auth = codex_home.join("auth.json");
    if !fs::metadata(&auth).is_ok_and(|m| m.is_file() && m.len() > 0) {
        bail!("isolated Codex home has no authentication; sign in to its CODEX_HOME first");
    }
    let job = fresh_job(work_dir)?;
    fs::write(
        job.join("host.json"),
        serde_json::to_vec_pretty(&json!({
            "exec_path":codex,"sha256":expected_sha256,"required_policy":"startup allowed_tools=[]",
        }))?,
    )?;
    let session = SessionDir::new()?;
    let result = run_job(
        codex,
        &codex_home,
        &job,
        &session.0,
        prompt,
        timeout,
        schema,
    );
    if result.is_err() {
        // The returned error contains no subprocess text. Auth providers can
        // put credential material in diagnostics; those remain local artifacts.
        fs::write(
            job.join("failed"),
            b"Worker failed; inspect this job's local artifacts.\n",
        )?;
    }
    result.with_context(|| format!("Codex worker artifacts: {}", job.display()))
}

pub fn verify_host_digest(host: &Path, expected_sha256: &str) -> Result<()> {
    if !host.is_absolute() || expected_sha256.len() != 64 {
        bail!("configure the absolute tools-off host path and its verified SHA-256");
    }
    let actual = hex::encode(Sha256::digest(
        fs::read(host).context("read pinned tools-off host")?,
    ));
    if actual != expected_sha256 {
        bail!("tools-off host checksum mismatch; inference refused");
    }
    Ok(())
}

fn fresh_job(parent: &Path) -> Result<PathBuf> {
    fresh_directory(parent, "job")
}

struct SessionDir(PathBuf);

impl SessionDir {
    fn new() -> Result<Self> {
        let path = fresh_directory(&env::temp_dir(), "lpbot-codex-session")?;
        let session = Self(path);
        // Refuse a configured temporary root inside a checkout or a directory
        // with project instructions. These files are never read by the worker.
        if session.0.ancestors().any(|p| {
            [".git", ".codex", "AGENTS.md", "AGENTS.override.md"]
                .iter()
                .any(|name| p.join(name).exists())
        }) {
            bail!("Codex temporary directory has project context in an ancestor; set TMPDIR outside the checkout");
        }
        fs::create_dir(session.0.join("home"))?;
        fs::create_dir(session.0.join("tmp"))?;
        Ok(session)
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fresh_directory(parent: &Path, prefix: &str) -> Result<PathBuf> {
    fs::create_dir_all(parent).context("create Codex worker directory")?;
    let parent = fs::canonicalize(parent).context("open Codex worker directory")?;
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for _ in 0..100 {
        let seq = JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!("{prefix}-{}-{epoch}-{seq}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).context("create fresh Codex job directory"),
        }
    }
    bail!("cannot allocate a fresh Codex job directory")
}

fn run_job(
    codex: &Path,
    codex_home: &Path,
    job: &Path,
    session: &Path,
    prompt: &str,
    timeout: Duration,
    schema: &Value,
) -> Result<Value> {
    let started = Instant::now();
    fs::write(job.join("prompt.txt"), prompt.as_bytes())?;
    fs::write(job.join("schema.json"), serde_json::to_vec_pretty(schema)?)?;
    // Keep even a missing/partial final response as an artifact.
    File::create(job.join("response.json"))?;
    let mut capabilities = command(codex, codex_home, session);
    capabilities.arg("--lpbot-tools-off").stdin(Stdio::null());
    let status = capture(
        &mut capabilities,
        job,
        "capabilities",
        timeout.min(Duration::from_secs(10)),
    )?;
    if !status.success()
        || fs::read_to_string(job.join("capabilities.stdout"))? != "lpbot-codex-tools-off/1\n"
    {
        bail!("the pinned executable is not the reviewed tools-off host; inference refused");
    }
    let mut version = command(codex, codex_home, session);
    version.arg("--version").stdin(Stdio::null());
    let version_status = capture(
        &mut version,
        job,
        "version",
        timeout.min(Duration::from_secs(10)),
    )?;
    if !version_status.success() {
        bail!("Codex CLI version check failed; inspect version artifacts");
    }
    let remaining = timeout
        .checked_sub(started.elapsed())
        .context("Codex worker timed out")?;
    let mut exec = command(codex, codex_home, session);
    exec.args([
        "--ignore-user-config",
        "--ignore-rules",
        "--ephemeral",
        "--strict-config",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--json",
        "--color",
        "never",
    ]);
    for feature in [
        "shell_tool",
        "apps",
        "plugins",
        "multi_agent",
        "memories",
        "hooks",
        "goals",
        "view_image",
        "browser_use",
        "browser_use_external",
        "computer_use",
        "image_generation",
        "remote_plugin",
        "skill_search",
        "shell_snapshot",
        "request_permissions_tool",
        "code_mode_host",
        "code_mode",
        "sleep_tool",
        "workspace_dependencies",
        "skill_mcp_dependency_install",
        "in_app_browser",
    ] {
        exec.args(["-c", &format!("features.{feature}=false")]);
    }
    exec.args(["-c", "features.skip_host_skill_discovery=true"]);
    for setting in [
        "web_search=\"disabled\"",
        "agents.enabled=false",
        "skills.bundled.enabled=false",
        "project_doc_max_bytes=0",
        "project_doc_fallback_filenames=[]",
        "approval_policy=\"never\"",
        "cli_auth_credentials_store=\"file\"",
        "suppress_unstable_features_warning=true",
    ] {
        exec.args(["-c", setting]);
    }
    exec.arg("--output-schema").arg(job.join("schema.json"));
    exec.arg("-o").arg(job.join("response.json")).arg("-");
    exec.stdin(Stdio::from(File::open(job.join("prompt.txt"))?));
    let status = capture(&mut exec, job, "exec", remaining);
    write_provenance(job, session)?;
    let status = status?;
    if !status.success() {
        bail!("Codex CLI failed with exit status {status}; inspect local stderr.log");
    }
    audit_events(job)?;
    let value: Value = serde_json::from_reader(File::open(job.join("response.json"))?)
        .context("Codex final response is missing or malformed JSON")?;
    validate(&value, schema).context("Codex final response does not match its output schema")?;
    Ok(value)
}

fn command(codex: &Path, codex_home: &Path, session: &Path) -> Command {
    let mut command = Command::new(codex);
    command
        .env_clear()
        .current_dir(session)
        .env("CODEX_HOME", codex_home);
    command
        .env("HOME", session.join("home"))
        .env("TMPDIR", session.join("tmp"));
    // No API keys, SSH agent, cloud credentials, session ids, or parent Codex
    // context survive. Certificate paths are needed by some local TLS setups.
    for key in [
        "PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "LANG",
        "LC_ALL",
    ] {
        if let Some(value) = env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

fn capture(
    command: &mut Command,
    job: &Path,
    stage: &str,
    timeout: Duration,
) -> Result<ExitStatus> {
    let (stdout, stderr) = if stage == "exec" {
        (job.join("stdout.jsonl"), job.join("stderr.log"))
    } else {
        (
            job.join(format!("{stage}.stdout")),
            job.join(format!("{stage}.stderr")),
        )
    };
    command.stdout(Stdio::from(File::create(stdout)?));
    command.stderr(Stdio::from(File::create(stderr)?));
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            write_status(job, stage, None, false, Some("spawn failed"))?;
            bail!("cannot start Codex CLI; check the configured executable");
        }
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A child must not leave any helper processes alive after the
                // job is complete, even when the CLI itself succeeded.
                kill_group(&mut child);
                write_status(job, stage, Some(status), false, None)?;
                return Ok(status);
            }
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(
                    Duration::from_millis(10).min(timeout - started.elapsed().min(timeout)),
                );
            }
            Ok(None) => {
                kill_group(&mut child);
                let status = child.wait().context("reap timed-out Codex CLI")?;
                write_status(job, stage, Some(status), true, None)?;
                bail!("Codex CLI timed out and was terminated");
            }
            Err(_) => {
                kill_group(&mut child);
                let status = child.wait().ok();
                write_status(job, stage, status, false, Some("wait failed"))?;
                bail!("cannot wait for Codex CLI; worker was terminated");
            }
        }
    }
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        // SAFETY: this child's process group was created by process_group(0).
        // A negative pid addresses only that group, never the runner's group.
        if let Ok(pid) = i32::try_from(child.id()) {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        }
    }
    let _ = child.kill();
}

fn write_status(
    job: &Path,
    stage: &str,
    status: Option<ExitStatus>,
    timed_out: bool,
    error: Option<&str>,
) -> Result<()> {
    #[cfg(unix)]
    let signal = {
        use std::os::unix::process::ExitStatusExt;
        status.and_then(|s| s.signal())
    };
    #[cfg(not(unix))]
    let signal: Option<i32> = None;
    let value = json!({"stage": stage, "code": status.and_then(|s| s.code()),
        "signal": signal, "success": status.is_some_and(|s| s.success()),
        "timed_out": timed_out, "error": error});
    fs::write(
        job.join(format!("{stage}.status.json")),
        serde_json::to_vec_pretty(&value)?,
    )?;
    if stage == "exec" {
        fs::write(job.join("status.json"), serde_json::to_vec_pretty(&value)?)?;
    }
    Ok(())
}

fn write_provenance(job: &Path, session: &Path) -> Result<()> {
    let version = fs::read_to_string(job.join("version.stdout"))?;
    let mut models = Vec::new();
    for line in BufReader::new(File::open(job.join("stdout.jsonl"))?).lines() {
        if let Ok(value) = serde_json::from_str::<Value>(&line?) {
            collect_models(&value, &mut models);
        }
    }
    fs::write(
        job.join("provenance.json"),
        serde_json::to_vec_pretty(&json!({
            "cli_version_output": version, "models_reported_by_cli": models,
            "tools_off_host_marker": fs::read_to_string(job.join("capabilities.stdout"))?,
            "tool_policy_allowed_tools": [],
            "model_override": null, "fresh_session": true, "stateless_context": true,
            "context_source": "supplied prompt only", "resumed_session": false,
            "temporary_cwd": session, "temporary_home": session.join("home"),
            "temporary_directory_cleanup": "removed when invocation returns"
        }))?,
    )?;
    Ok(())
}

fn audit_events(job: &Path) -> Result<()> {
    let mut completed = false;
    for line in BufReader::new(File::open(job.join("stdout.jsonl"))?).lines() {
        let value: Value =
            serde_json::from_str(&line?).context("Codex stdout contains a malformed JSON event")?;
        match value.get("type").and_then(Value::as_str) {
            Some("turn.completed") => completed = true,
            Some("thread.started" | "turn.started") => {}
            Some("error" | "turn.failed") => {
                bail!("Codex reported a failed turn; decision rejected")
            }
            Some("item.started" | "item.updated" | "item.completed") => {
                let kind = value
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str);
                // Native exec uses ErrorItem for configuration warnings,
                // deprecations and rerouting; actual failures are top-level errors.
                if !matches!(kind, Some("agent_message" | "reasoning" | "error")) {
                    bail!(
                        "Codex worker emitted a forbidden tool or unknown item; decision rejected"
                    );
                }
            }
            _ => bail!("Codex emitted an unknown event; decision rejected"),
        }
    }
    if !completed {
        bail!("Codex did not acknowledge a completed turn; decision rejected");
    }
    Ok(())
}

fn collect_models(value: &Value, models: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if matches!(key.as_str(), "model" | "model_id" | "model_slug") {
                    if let Some(model) = value.as_str() {
                        if !models.iter().any(|m| m == model) {
                            models.push(model.to_owned());
                        }
                    }
                } else {
                    collect_models(value, models);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_models(item, models);
            }
        }
        _ => {}
    }
}

fn check_schema(schema: &Value) -> Result<()> {
    let object = schema
        .as_object()
        .context("Codex output schema must be an object")?;
    for (key, value) in object {
        match key.as_str() {
            "title" | "description" if value.is_string() => {}
            "enum" if value.as_array().is_some_and(|a| !a.is_empty()) => {}
            "minimum" | "maximum" if value.as_f64().is_some_and(f64::is_finite) => {}
            "type" => {
                let types = value
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| vec![value.clone()]);
                if types.is_empty()
                    || types.iter().any(|t| {
                        !matches!(
                            t.as_str(),
                            Some(
                                "object"
                                    | "array"
                                    | "string"
                                    | "number"
                                    | "integer"
                                    | "boolean"
                                    | "null"
                            )
                        )
                    })
                {
                    bail!("unsupported Codex output schema type");
                }
            }
            "properties" => {
                for schema in value
                    .as_object()
                    .context("schema properties must be an object")?
                    .values()
                {
                    check_schema(schema)?;
                }
            }
            "required" => {
                if value
                    .as_array()
                    .is_none_or(|a| a.iter().any(|v| !v.is_string()))
                {
                    bail!("schema required must be an array of strings");
                }
            }
            "additionalProperties" if value == false => {}
            "items" => check_schema(value)?,
            "anyOf" => {
                let variants = value.as_array().context("schema anyOf must be an array")?;
                if variants.is_empty() {
                    bail!("schema anyOf must not be empty");
                }
                for variant in variants {
                    check_schema(variant)?;
                }
            }
            _ => bail!("unsupported Codex output schema assertion: {key}"),
        }
    }
    if object.get("type").is_some_and(|t| t == "object")
        && object.get("additionalProperties") != Some(&Value::Bool(false))
    {
        bail!("Codex object output schema must set additionalProperties=false");
    }
    Ok(())
}

fn validate(value: &Value, schema: &Value) -> Result<()> {
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        if !variants.iter().any(|s| validate(value, s).is_ok()) {
            bail!("value matches no anyOf alternative");
        }
    }
    if let Some(expected) = schema.get("type") {
        let matches = |t: &Value| match t.as_str() {
            Some("object") => value.is_object(),
            Some("array") => value.is_array(),
            Some("string") => value.is_string(),
            Some("number") => value.is_number(),
            Some("integer") => value.is_i64() || value.is_u64(),
            Some("boolean") => value.is_boolean(),
            Some("null") => value.is_null(),
            _ => false,
        };
        if !expected
            .as_array()
            .map_or_else(|| matches(expected), |types| types.iter().any(matches))
        {
            bail!("value has the wrong JSON type");
        }
    }
    if schema
        .get("enum")
        .and_then(Value::as_array)
        .is_some_and(|allowed| !allowed.contains(value))
    {
        bail!("value is outside its enum");
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required {
                if !object.contains_key(key.as_str().context("invalid schema required key")?) {
                    bail!("required response field is missing");
                }
            }
        }
        for (key, value) in object {
            if let Some(property) = properties.and_then(|p| p.get(key)) {
                validate(value, property)?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                bail!("response has an unknown field");
            }
        }
    }
    if let (Some(items), Some(schema)) = (value.as_array(), schema.get("items")) {
        for item in items {
            validate(item, schema)?;
        }
    }
    if let Some(number) = value.as_f64() {
        if !number.is_finite()
            || schema
                .get("minimum")
                .and_then(Value::as_f64)
                .is_some_and(|n| number < n)
            || schema
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|n| number > n)
        {
            bail!("response number is outside its permitted range");
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "codex_worker_tests.rs"]
mod tests;
