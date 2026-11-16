//! `lpbot load-results`: the result tables into Postgres, where the viewer
//! reads them.

use std::path::Path;

use anyhow::{anyhow, bail, Result};
use lpbot_sim::loader::{load_results, LoadReport};

/// The database to load into: the argument, then `LPBOT_DB_URL`.
pub fn database(argument: Option<String>) -> Option<String> {
    argument
        .or_else(|| std::env::var("LPBOT_DB_URL").ok())
        .filter(|url| !url.is_empty())
}

/// Opens the database. The URL holds the password, so it is never printed.
pub fn connect(url: &str) -> Result<postgres::Client> {
    postgres::Client::connect(url, postgres::NoTls)
        .map_err(|e| anyhow!("cannot connect to the database: {e}"))
}

/// Loads every run under `results`. A run that is already loaded replaces
/// itself.
pub fn run(url: &str, results: &Path) -> Result<LoadReport> {
    let mut client = connect(url)?;
    let report = load_results(&mut client, results)?;
    if report.runs == 0 {
        bail!(
            "no runs found under {}; run `lpbot run <experiment.yml>` first",
            results.display()
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_database_argument_wins_and_an_empty_one_counts_as_none() {
        assert_eq!(
            database(Some("postgres://a".into())),
            Some("postgres://a".to_string())
        );
        assert_eq!(database(Some(String::new())), None);
    }

    #[test]
    fn a_database_that_is_not_there_is_reported_without_its_password() {
        // Port 9 has no listener.
        let url = "postgres://lpbot:topsecret@127.0.0.1:9/lpbot";
        let error = connect(url).err().expect("no database there").to_string();
        assert!(error.contains("cannot connect to the database"), "{error}");
        assert!(!error.contains("topsecret"), "{error}");
        let error = run(url, Path::new("runs")).unwrap_err().to_string();
        assert!(!error.contains("topsecret"), "{error}");
    }
}
