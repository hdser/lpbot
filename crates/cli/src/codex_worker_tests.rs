use super::*;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let seq = JOB_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "lpbot-codex-worker-test-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("auth-home")).unwrap();
        fs::write(path.join("auth-home/auth.json"), b"{}\n").unwrap();
        Self(path)
    }

    fn home(&self) -> PathBuf {
        self.0.join("auth-home")
    }
    fn work(&self) -> PathBuf {
        self.0.join("jobs")
    }

    fn job(&self) -> PathBuf {
        let mut jobs = fs::read_dir(self.work())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(jobs.len(), 1);
        jobs.pop().unwrap()
    }

    #[cfg(unix)]
    fn fake(&self, response: &str, extra: &str) -> PathBuf {
        let path = self.0.join("fake-codex");
        let script = format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then
  printf 'codex-cli fake-1.0\n'
  exit 0
fi
previous=''
for argument do
  if [ "$previous" = "-o" ]; then output="$argument"; fi
  previous="$argument"
done
artifacts="${{output%/*}}"
printf '%s\n' "$@" > "$artifacts/argv.txt"
/usr/bin/env > "$artifacts/child.env"
printf '%s' "$$" > "$artifacts/child.pid"
pwd > "$artifacts/child.cwd"
cat > "$artifacts/prompt.received"
printf 'exact stderr\n' >&2
{extra}
cat > "$output" <<'LPBOT_FAKE_JSON'
{response}
LPBOT_FAKE_JSON
printf '%s\n' '{{"type":"thread.started","model":"fake-default"}}'
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"done"}}}}'
"#
        );
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn response() -> Value {
    json!({
        "action": "enter", "next": " 15m ", "note": "  exact note\n",
        "reasoning": "  first line\nsecond line  ", "expect_low": -2.0,
        "expect_high": 3.0, "expect_confidence": 0.75, "expect_text": "  expect\n  ",
        "lower_pct": -1.0, "upper_pct": 1.0, "stance": "adapt", "toward": "peer"
    })
}

#[test]
#[cfg(unix)]
fn preserves_exact_fields_prompt_and_process_artifacts_without_inherited_context() {
    let dir = TestDir::new();
    let original = response();
    let fake = dir.fake(&original.to_string(), "");
    let prompt = "generated brief\n\npermitted stdout\n  ";
    let result = run(
        &fake,
        &dir.home(),
        &dir.work(),
        prompt,
        Duration::from_secs(3),
    )
    .unwrap();
    assert_eq!(result.action, "enter");
    assert_eq!(result.next, " 15m ");
    assert_eq!(result.note, "  exact note\n");
    assert_eq!(result.reasoning, "  first line\nsecond line  ");
    assert_eq!(result.expect_text, "  expect\n  ");
    assert_eq!(result.expect_low, -2.0);
    assert_eq!(result.expect_high, 3.0);
    assert_eq!(result.expect_confidence, 0.75);
    assert_eq!(result.lower_pct, Some(-1.0));
    assert_eq!(result.upper_pct, Some(1.0));
    assert_eq!(result.stance.as_deref(), Some("adapt"));
    assert_eq!(result.toward.as_deref(), Some("peer"));
    let job = dir.job();
    assert_eq!(fs::read_to_string(job.join("prompt.txt")).unwrap(), prompt);
    assert_eq!(
        fs::read_to_string(job.join("prompt.received")).unwrap(),
        prompt
    );
    assert_eq!(
        fs::read_to_string(job.join("stderr.log")).unwrap(),
        "exact stderr\n"
    );
    assert_eq!(
        fs::read_to_string(job.join("response.json")).unwrap(),
        format!("{original}\n")
    );
    let status: Value =
        serde_json::from_slice(&fs::read(job.join("status.json")).unwrap()).unwrap();
    assert_eq!(status["success"], true);
    assert_eq!(status["code"], 0);
    let provenance: Value =
        serde_json::from_slice(&fs::read(job.join("provenance.json")).unwrap()).unwrap();
    assert_eq!(provenance["cli_version_output"], "codex-cli fake-1.0\n");
    assert_eq!(
        provenance["models_reported_by_cli"],
        json!(["fake-default"])
    );
    assert_eq!(provenance["resumed_session"], false);
    let args = fs::read_to_string(job.join("argv.txt")).unwrap();
    assert!(args.starts_with("exec\n--ignore-user-config\n--ignore-rules\n--ephemeral\n"));
    assert!(args.contains("--sandbox\nread-only\n--json\n"));
    assert!(args.contains("--disable\nshell_tool\n"));
    assert!(args.contains("agents.enabled=false"));
    assert!(args.contains("skills.bundled.enabled=false"));
    assert!(args.contains("project_doc_max_bytes=0"));
    assert!(!args
        .lines()
        .any(|a| matches!(a, "resume" | "fork" | "--model")));
    let vars = fs::read_to_string(job.join("child.env")).unwrap();
    let transport_environment = [
        "HOME",
        "TMPDIR",
        "CODEX_HOME",
        "PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
        "LANG",
        "LC_ALL",
    ];
    // The fake executable is /bin/sh, which creates these itself after the
    // transport's env_clear. They are not inherited parent credentials/context.
    let generated_shell_environment = ["PWD", "SHLVL", "_"];
    for var in vars.lines() {
        let name = var.split_once('=').unwrap().0;
        assert!(
            transport_environment.contains(&name) || generated_shell_environment.contains(&name),
            "unexpected inherited variable: {name}"
        );
    }
    let cwd = fs::read_to_string(job.join("child.cwd")).unwrap();
    let cwd = Path::new(cwd.trim_end());
    assert!(!cwd.starts_with(&dir.0));
    assert!(!cwd.starts_with(env!("CARGO_MANIFEST_DIR")));
    assert!(vars.contains(&format!("HOME={}/home\n", cwd.display())));
    assert!(!cwd.exists(), "temporary session directory must be removed");
    assert_eq!(provenance["temporary_cwd"], cwd.to_string_lossy().as_ref());
}

#[test]
#[cfg(unix)]
fn times_out_kills_and_reaps_the_cli_and_keeps_partial_outputs() {
    let dir = TestDir::new();
    let fake = dir.fake(
        &response().to_string(),
        "printf 'partial event'\nsleep 30 &\nprintf ready > \"$artifacts/ready\"\nwait",
    );
    let home = dir.home();
    let work = dir.work();
    let worker_work = work.clone();
    let started = Instant::now();
    let worker =
        thread::spawn(move || run(&fake, &home, &worker_work, "brief", Duration::from_secs(3)));
    let mut ready = false;
    while started.elapsed() < Duration::from_secs(5) && !worker.is_finished() {
        ready = fs::read_dir(&work).is_ok_and(|jobs| {
            jobs.filter_map(std::result::Result::ok)
                .any(|job| job.path().join("ready").exists())
        });
        if ready {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let error = worker.join().unwrap().unwrap_err();
    assert!(
        ready,
        "fake exec must reach its blocking workload before timeout: {error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(6));
    assert!(format!("{error:#}").contains("timed out"));
    let job = dir.job();
    assert_eq!(
        fs::read_to_string(job.join("stdout.jsonl")).unwrap(),
        "partial event"
    );
    let status: Value =
        serde_json::from_slice(&fs::read(job.join("status.json")).unwrap()).unwrap();
    assert_eq!(status["timed_out"], true);
    assert_eq!(status["success"], false);
    let pid: i32 = fs::read_to_string(job.join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks existence; the worker must have reaped this pid.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
#[cfg(unix)]
fn rejects_malformed_json_without_returning_raw_process_diagnostics() {
    let dir = TestDir::new();
    let fake = dir.fake("this is not JSON", "printf 'sensitive-diagnostic' >&2");
    let error = run(
        &fake,
        &dir.home(),
        &dir.work(),
        "brief",
        Duration::from_secs(3),
    )
    .unwrap_err();
    let rendered = format!("{error:#}");
    assert!(rendered.contains("malformed JSON"));
    assert!(!rendered.contains("sensitive-diagnostic"));
    assert!(fs::read_to_string(dir.job().join("stderr.log"))
        .unwrap()
        .contains("sensitive-diagnostic"));
}

#[test]
#[cfg(unix)]
fn rejects_a_cadence_shorter_than_fifteen_minutes() {
    let dir = TestDir::new();
    let mut value = response();
    value["next"] = json!("14m59s");
    let fake = dir.fake(&value.to_string(), "");
    let error = run(
        &fake,
        &dir.home(),
        &dir.work(),
        "brief",
        Duration::from_secs(3),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("at least 15m"));
}

#[test]
#[cfg(unix)]
fn rejects_unknown_or_missing_response_fields() {
    for missing in [false, true] {
        let dir = TestDir::new();
        let mut value = response();
        if missing {
            value.as_object_mut().unwrap().remove("toward");
        } else {
            value["unrequested"] = json!(true);
        }
        let fake = dir.fake(&value.to_string(), "");
        assert!(run(
            &fake,
            &dir.home(),
            &dir.work(),
            "brief",
            Duration::from_secs(3)
        )
        .is_err());
    }
}

#[test]
#[cfg(unix)]
fn rejects_tool_events_even_if_a_valid_final_response_was_written() {
    let dir = TestDir::new();
    let fake = dir.fake(
        &response().to_string(),
        r#"printf '%s\n' '{"type":"item.started","item":{"type":"command_execution"}}'"#,
    );
    let error = run(
        &fake,
        &dir.home(),
        &dir.work(),
        "brief",
        Duration::from_secs(3),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("forbidden tool"));
}

#[test]
#[cfg(unix)]
fn retains_nonzero_exit_and_never_returns_process_stderr() {
    let dir = TestDir::new();
    let fake = dir.fake(
        &response().to_string(),
        "printf 'secret diagnostic' >&2\nexit 9",
    );
    let error = run(
        &fake,
        &dir.home(),
        &dir.work(),
        "brief",
        Duration::from_secs(3),
    )
    .unwrap_err();
    assert!(!format!("{error:#}").contains("secret diagnostic"));
    let status: Value =
        serde_json::from_slice(&fs::read(dir.job().join("status.json")).unwrap()).unwrap();
    assert_eq!(status["code"], 9);
}

#[test]
#[cfg(unix)]
fn accepts_a_separate_closed_review_schema() {
    let dir = TestDir::new();
    let value = json!({"assessment": " uncertain ", "justification": "  evidence\n", "limitations": "supplied look only"});
    let schema = json!({"type": "object", "additionalProperties": false, "properties": {
        "assessment": {"type": "string"}, "justification": {"type": "string"},
        "limitations": {"type": "string"}
    }, "required": ["assessment", "justification", "limitations"]});
    let fake = dir.fake(&value.to_string(), "");
    let result = run_json(
        &fake,
        &dir.home(),
        &dir.work(),
        "permitted review",
        Duration::from_secs(3),
        &schema,
    )
    .unwrap();
    assert_eq!(result, value);
}

#[test]
#[cfg(unix)]
fn refuses_missing_authentication_without_starting_codex() {
    let dir = TestDir::new();
    fs::remove_file(dir.home().join("auth.json")).unwrap();
    let fake = dir.fake(&response().to_string(), "");
    let error = run(
        &fake,
        &dir.home(),
        &dir.work(),
        "brief",
        Duration::from_secs(3),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("sign in"));
    assert!(!dir.work().exists());
}

#[test]
fn checks_action_ranges_expectations_and_stance_without_normalizing_strings() {
    let check = |v: Value| serde_json::from_value::<Response>(v).unwrap().check();
    assert!(check(response()).is_ok());
    for (key, value) in [
        ("action", json!(" enter")),
        ("lower_pct", json!(-100.0)),
        ("upper_pct", json!(-1.0)),
        ("expect_low", json!(3.0)),
        ("expect_confidence", json!(1.01)),
        ("next", json!("later")),
        ("stance", json!("follow")),
        ("note", json!(" \n")),
    ] {
        let mut v = response();
        v[key] = value;
        assert!(check(v).is_err(), "accepted invalid {key}");
    }
    let mut v = response();
    v["action"] = json!("hold");
    assert!(check(v.clone()).is_err());
    v["lower_pct"] = Value::Null;
    v["upper_pct"] = Value::Null;
    assert!(check(v).is_ok());
}

#[test]
fn schema_assertions_are_applied_locally_and_unknown_assertions_refused() {
    let schema = json!({"type": "array", "items": {"anyOf": [
        {"type": "integer", "minimum": 0, "maximum": 2},
        {"type": "string", "enum": ["ok"]}
    ]}});
    check_schema(&schema).unwrap();
    assert!(validate(&json!([1, "ok"]), &schema).is_ok());
    assert!(validate(&json!([3]), &schema).is_err());
    assert!(validate(&json!(["wrong"]), &schema).is_err());
    assert!(check_schema(&json!({"type": "object"})).is_err());
    assert!(check_schema(&json!({"type": "string", "pattern": ".*"})).is_err());
}
