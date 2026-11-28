//! The global pause flag and the operator's exit (`docs/DESIGN.md`, "The
//! three exit paths", 2).
//!
//! The flag is a small JSON file the trader reads at every step. `paused`
//! stops new entries; `exit` also asks every position to be taken out at
//! once. It is written by `lpbot pause` and `lpbot exit-all` and cleared by
//! `lpbot resume`, from a terminal on the machine or, later, from the chat
//! channel. A flag that cannot be read counts as paused: when in doubt, no
//! new entries.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The flag's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flag {
    pub paused: bool,
    /// Take every position out now, not only stop entering.
    pub exit: bool,
    pub reason: String,
    pub by: String,
    pub at: u64,
}

/// What the trader does about the flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Running,
    /// No new entries; open positions stay under their own rules.
    Paused(String),
    /// Every position out, and no new entries.
    Exiting(String),
}

/// The flag's file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseFlag {
    path: PathBuf,
}

impl PauseFlag {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What the flag says now.
    pub fn state(&self) -> State {
        match std::fs::read(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::Running,
            Err(e) => State::Paused(format!(
                "the pause flag {} cannot be read ({e}), so entries are held",
                self.path.display()
            )),
            Ok(bytes) => match serde_json::from_slice::<Flag>(&bytes) {
                Err(e) => State::Paused(format!(
                    "the pause flag {} is not valid ({e}), so entries are held; `lpbot resume` \
                     clears it",
                    self.path.display()
                )),
                Ok(flag) if flag.exit => State::Exiting(describe(&flag)),
                Ok(flag) if flag.paused => State::Paused(describe(&flag)),
                Ok(_) => State::Running,
            },
        }
    }

    /// Sets the flag: `exit` takes every position out as well.
    pub fn set(&self, exit: bool, reason: &str, by: &str, at: u64) -> std::io::Result<Flag> {
        let flag = Flag {
            paused: true,
            exit,
            reason: reason.trim().to_string(),
            by: by.trim().to_string(),
            at,
        };
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self.path.with_extension("tmp");
        std::fs::write(
            &temporary,
            serde_json::to_vec_pretty(&flag).expect("a flag always serializes"),
        )?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(flag)
    }

    /// Clears the flag. Clearing one that is not set is not an error.
    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

fn describe(flag: &Flag) -> String {
    format!(
        "{} by {} at {}: {}",
        if flag.exit { "exit-all" } else { "paused" },
        flag.by,
        flag.at,
        flag.reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lpbot-pause-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_flag_pauses_exits_and_clears() {
        let dir = temp("flag");
        let flag = PauseFlag::new(dir.join("state/pause.json"));
        assert_eq!(flag.state(), State::Running, "no file: running");
        flag.set(false, "gas spike", "owner", 10).unwrap();
        assert!(
            matches!(flag.state(), State::Paused(r) if r.contains("paused by owner at 10: gas spike"))
        );
        flag.set(true, "suspected key leak", "owner", 11).unwrap();
        assert!(matches!(flag.state(), State::Exiting(r) if r.contains("exit-all")));
        flag.clear().unwrap();
        assert_eq!(flag.state(), State::Running);
        flag.clear().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_flag_that_cannot_be_read_holds_entries() {
        let dir = temp("broken");
        let path = dir.join("pause.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(
            matches!(PauseFlag::new(&path).state(), State::Paused(r) if r.contains("not valid"))
        );
        // A folder where the file should be cannot be read either.
        let folder = dir.join("folder.json");
        std::fs::create_dir_all(&folder).unwrap();
        assert!(matches!(PauseFlag::new(&folder).state(), State::Paused(_)));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
