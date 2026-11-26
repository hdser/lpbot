//! Process locks shared by CLI users, including existing Claude sessions.
//! Never hold the lake lock while waiting for a future look or a model.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

pub fn exclusive(path: &Path, nonblocking: bool) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    if nonblocking {
        file.try_lock()
            .with_context(|| format!("{} is busy; another job owns it", path.display()))?;
    } else {
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
    }
    Ok(file)
}

/// A command's mutation lock and a persistent runner ownership fence.
/// Runner subprocesses carry a token; ordinary next/decide calls refuse an
/// owned identity even between daemon polls or after a daemon crash.
pub fn command(decisions: &Path) -> Result<File> {
    let guard = exclusive(&sibling(decisions, ".command.lock"), false)?;
    let owner = sibling(decisions, ".runner-owner.json");
    let token = std::env::var("LPBOT_RUNNER_TOKEN").ok();
    match std::fs::read(&owner) {
        Ok(bytes) => {
            let value: Value =
                serde_json::from_slice(&bytes).context("damaged runner ownership fence")?;
            verify_owner(Some(&value), token.as_deref())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => verify_owner(None, token.as_deref())?,
        Err(e) => return Err(e).context("reading runner ownership fence"),
    }
    Ok(guard)
}

fn verify_owner(owner: Option<&Value>, token: Option<&str>) -> Result<()> {
    let token = token.filter(|t| !t.is_empty());
    match owner {
        Some(value) => {
            let expected = value["token"].as_str().filter(|t| !t.is_empty());
            if expected.is_none() || token != expected {
                bail!("this agent is owned by the server runner; inspect its status instead of starting another writer");
            }
        }
        None if token.is_some() => {
            bail!("runner ownership was released; this delayed runner job is stale");
        }
        None => {}
    }
    Ok(())
}

/// Both catch-up writers and history/scoring readers cooperate on this lock.
pub fn lake(root: &Path) -> Result<File> {
    exclusive(&root.join(".lpbot-operations.lock"), false)
}

pub fn read_optional(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Persist coordinator/pending state with rename and directory durability.
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = sibling(path, &format!(".{}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.with_context(|| format!("persisting {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn released_or_replaced_owner_refuses_a_delayed_runner_token() {
        let owner = serde_json::json!({"token":"current"});
        assert!(verify_owner(Some(&owner), Some("current")).is_ok());
        assert!(verify_owner(Some(&owner), Some("previous")).is_err());
        assert!(verify_owner(Some(&owner), None).is_err());
        assert!(verify_owner(None, Some("current")).is_err());
        assert!(verify_owner(None, None).is_ok());
        assert!(verify_owner(None, Some("")).is_ok());
        assert!(verify_owner(Some(&serde_json::json!({})), None).is_err());
    }

    #[test]
    fn independent_handles_cannot_own_the_same_job() {
        let path = std::env::temp_dir().join(format!(
            "lpbot-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let guard = exclusive(&path, true).unwrap();
        assert!(exclusive(&path, true).is_err());
        drop(guard);
        assert!(exclusive(&path, true).is_ok());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn atomic_replacement_keeps_a_complete_value() {
        let dir = std::env::temp_dir().join(format!(
            "lpbot-atomic-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("state.json");
        atomic(&path, b"first\n").unwrap();
        atomic(&path, b"second\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
