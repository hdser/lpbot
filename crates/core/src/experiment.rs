//! Experiment files.
//!
//! An experiment is one YAML file that points at a shared base config and
//! states only what it changes. An optional `sweep` block expands into one
//! run per combination. Every run carries its fully resolved config and a
//! hash of it, so the same file, data and code version give the same result.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use sha2::{Digest, Sha256};

use crate::config::{Autonomy, Config, Mode};

/// The experiment file as written by the user.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExperimentFile {
    /// Name of the experiment; becomes the prefix of every run id.
    pub experiment: String,
    /// Path of the shared base config.
    pub base: PathBuf,
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub window: Option<Value>,
    /// Run inputs; merged over the base config's `run` block.
    #[serde(default)]
    pub run: Option<Mapping>,
    /// Id of the strategy in the base config this experiment runs.
    pub strategy: String,
    /// Dotted setting path (relative to the strategy) to the values to try.
    #[serde(default)]
    pub sweep: BTreeMap<String, Vec<Value>>,
}

/// Run inputs given on the command line. They win over the files.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RunOverrides {
    pub capital_usd: Option<f64>,
    pub loss_limit_pct: Option<f64>,
    pub autonomy: Option<Autonomy>,
}

/// One concrete run of an experiment.
#[derive(Debug, Clone)]
pub struct ResolvedRun {
    pub run_id: String,
    pub experiment: String,
    /// Position of this run inside the experiment's sweep, from zero.
    pub index: usize,
    /// The swept settings of this run, as `path -> value`. Empty without a sweep.
    pub sweep_point: BTreeMap<String, String>,
    pub config: Config,
    /// The resolved config exactly as hashed.
    pub config_yaml: String,
    /// SHA-256 of `config_yaml`, hex encoded.
    pub config_hash: String,
}

/// Loads an experiment file and its base config and expands it into runs.
pub fn resolve_experiment(path: &Path, overrides: &RunOverrides) -> Result<Vec<ResolvedRun>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read experiment file {}", path.display()))?;
    let exp: ExperimentFile = serde_yaml::from_str(&text)
        .with_context(|| format!("invalid experiment file {}", path.display()))?;

    let base_path = find_base(path, &exp.base)?;
    let base_text = std::fs::read_to_string(&base_path)
        .with_context(|| format!("cannot read base config {}", base_path.display()))?;
    let base: Value = serde_yaml::from_str(&base_text)
        .with_context(|| format!("invalid base config {}", base_path.display()))?;

    resolve(&exp, base, overrides)
}

/// Expands an already parsed experiment over an already parsed base config.
pub fn resolve(
    exp: &ExperimentFile,
    mut base: Value,
    overrides: &RunOverrides,
) -> Result<Vec<ResolvedRun>> {
    if exp.experiment.trim().is_empty() {
        bail!("experiment name is empty");
    }
    if !base.is_mapping() {
        bail!("base config must be a YAML mapping");
    }

    if let Some(mode) = exp.mode {
        set_path(&mut base, "mode", serde_yaml::to_value(mode)?)?;
    }
    if let Some(window) = &exp.window {
        set_path(&mut base, "window", window.clone())?;
    }
    if let Some(run) = &exp.run {
        for (k, v) in run {
            let key = k
                .as_str()
                .ok_or_else(|| anyhow!("run keys must be strings"))?;
            set_path(&mut base, &format!("run.{key}"), v.clone())?;
        }
    }
    if let Some(c) = overrides.capital_usd {
        set_path(&mut base, "run.capital_usd", serde_yaml::to_value(c)?)?;
    }
    if let Some(l) = overrides.loss_limit_pct {
        set_path(&mut base, "run.loss_limit_pct", serde_yaml::to_value(l)?)?;
    }
    if let Some(a) = overrides.autonomy {
        set_path(&mut base, "run.autonomy", serde_yaml::to_value(a)?)?;
    }

    // Approval cannot be simulated on history, so backtests always run as auto.
    if base.get("mode").and_then(Value::as_str) == Some("backtest") {
        set_path(&mut base, "run.autonomy", Value::String("auto".into()))?;
    }

    select_strategy(&mut base, &exp.strategy)?;

    let points = sweep_points(&exp.sweep)?;
    let mut runs = Vec::with_capacity(points.len());
    for (index, point) in points.into_iter().enumerate() {
        let mut value = base.clone();
        {
            let strategy = value
                .get_mut("strategies")
                .and_then(Value::as_sequence_mut)
                .and_then(|s| s.first_mut())
                .ok_or_else(|| anyhow!("strategy disappeared during resolution"))?;
            for (path, v) in &point {
                set_path(strategy, path, v.clone())
                    .with_context(|| format!("cannot apply sweep setting `{path}`"))?;
            }
        }

        let config_yaml = serde_yaml::to_string(&value)?;
        let config_hash = hex::encode(Sha256::digest(config_yaml.as_bytes()));
        let config: Config = serde_yaml::from_value(value).with_context(|| {
            format!("run {index} of `{}` is not a valid config", exp.experiment)
        })?;
        if let Err(errors) = config.validate() {
            let list: Vec<String> = errors.iter().map(|e| format!("  - {e}")).collect();
            bail!(
                "run {index} of `{}` cannot start:\n{}",
                exp.experiment,
                list.join("\n")
            );
        }

        let sweep_point = point
            .iter()
            .map(|(k, v)| Ok((k.clone(), display_value(v)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;

        runs.push(ResolvedRun {
            run_id: format!("{}-{:03}-{}", exp.experiment, index, &config_hash[..8]),
            experiment: exp.experiment.clone(),
            index,
            sweep_point,
            config,
            config_yaml,
            config_hash,
        });
    }
    Ok(runs)
}

fn find_base(experiment_path: &Path, base: &Path) -> Result<PathBuf> {
    if base.is_absolute() {
        return Ok(base.to_path_buf());
    }
    let exp_dir = experiment_path.parent().unwrap_or(Path::new("."));
    let mut candidates = vec![base.to_path_buf(), exp_dir.join(base)];
    if let Some(parent) = exp_dir.parent() {
        candidates.push(parent.join(base));
    }
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "base config `{}` not found (looked relative to the working directory, the experiment file, and its parent)",
                base.display()
            )
        })
}

/// Keeps only the strategy with the given id.
fn select_strategy(base: &mut Value, id: &str) -> Result<()> {
    let strategies = base
        .get_mut("strategies")
        .and_then(Value::as_sequence_mut)
        .ok_or_else(|| anyhow!("base config has no `strategies` list"))?;
    let known: Vec<String> = strategies
        .iter()
        .filter_map(|s| s.get("id").and_then(Value::as_str).map(String::from))
        .collect();
    strategies.retain(|s| s.get("id").and_then(Value::as_str) == Some(id));
    match strategies.len() {
        1 => Ok(()),
        0 => bail!(
            "strategy `{id}` is not in the base config (known: {})",
            known.join(", ")
        ),
        _ => bail!("strategy id `{id}` appears more than once in the base config"),
    }
}

/// Cartesian product of the sweep, in key order.
fn sweep_points(sweep: &BTreeMap<String, Vec<Value>>) -> Result<Vec<BTreeMap<String, Value>>> {
    let mut points = vec![BTreeMap::new()];
    for (key, values) in sweep {
        if values.is_empty() {
            bail!("sweep setting `{key}` has no values");
        }
        let mut next = Vec::with_capacity(points.len() * values.len());
        for point in &points {
            for v in values {
                let mut p = point.clone();
                p.insert(key.clone(), v.clone());
                next.push(p);
            }
        }
        points = next;
    }
    Ok(points)
}

/// Sets `a.b.c` inside a YAML mapping, creating missing levels.
fn set_path(root: &mut Value, dotted: &str, new: Value) -> Result<()> {
    let parts: Vec<&str> = dotted.split('.').collect();
    if parts.iter().any(|p| p.is_empty()) {
        bail!("`{dotted}` is not a valid setting path");
    }
    let mut cur = root;
    for part in &parts[..parts.len() - 1] {
        cur = child_mapping(cur, dotted)?
            .entry(Value::String((*part).to_string()))
            .or_insert(Value::Null);
    }
    child_mapping(cur, dotted)?.insert(Value::String(parts[parts.len() - 1].to_string()), new);
    Ok(())
}

fn child_mapping<'a>(value: &'a mut Value, dotted: &str) -> Result<&'a mut Mapping> {
    if value.is_null() {
        *value = Value::Mapping(Mapping::new());
    }
    value
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("`{dotted}` passes through a setting that is not a mapping"))
}

fn display_value(v: &Value) -> Result<String> {
    Ok(serde_yaml::to_string(v)?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PatienceMode;

    const BASE: &str = r#"
mode: paper
run: { autonomy: approve }
universe: { tokens: [WETH, WBTC, USDC, USDT] }
strategies:
  - id: eth-zone-roundtrip
    kind: zone_round_trip
    value_token: WETH
    entry: { shapes: [one_sided], width_ticks: { min: 10, max: 80 } }
    patience: { mode: adaptive, decide_every: 1h }
  - id: other
    kind: wide_static
    value_token: USDC
"#;

    const EXPERIMENT: &str = r#"
experiment: width-and-patience-eth
base: config/mainnet.yml
mode: backtest
window: { from: 2026-05-01, to: 2026-10-31 }
run: { capital_usd: 10000, loss_limit_pct: 5 }
strategy: eth-zone-roundtrip
sweep:
  entry.width_ticks: [10, 20, 40, 80]
  patience.mode: [adaptive, wait, flip, close]
"#;

    fn runs(experiment: &str, overrides: &RunOverrides) -> Result<Vec<ResolvedRun>> {
        let exp: ExperimentFile = serde_yaml::from_str(experiment)?;
        resolve(&exp, serde_yaml::from_str(BASE)?, overrides)
    }

    #[test]
    fn a_sweep_expands_into_one_run_per_combination() {
        let runs = runs(EXPERIMENT, &RunOverrides::default()).unwrap();
        assert_eq!(runs.len(), 16);

        let first = &runs[0];
        assert_eq!(first.sweep_point["entry.width_ticks"], "10");
        assert_eq!(first.sweep_point["patience.mode"], "adaptive");
        assert_eq!(first.config.strategies.len(), 1);
        assert_eq!(first.config.strategies[0].id, "eth-zone-roundtrip");

        let last = &runs[15];
        assert_eq!(last.sweep_point["entry.width_ticks"], "80");
        assert_eq!(last.config.strategies[0].patience.mode, PatienceMode::Close);
        // Settings that were not swept survive from the base config.
        assert!(last.config.strategies[0]
            .patience
            .extra
            .contains_key("decide_every"));
    }

    #[test]
    fn run_ids_are_unique_and_reproducible() {
        let a = runs(EXPERIMENT, &RunOverrides::default()).unwrap();
        let b = runs(EXPERIMENT, &RunOverrides::default()).unwrap();
        let ids: std::collections::BTreeSet<_> = a.iter().map(|r| r.run_id.clone()).collect();
        assert_eq!(ids.len(), a.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x.run_id, y.run_id);
            assert_eq!(x.config_hash, y.config_hash);
        }
    }

    #[test]
    fn backtests_always_run_as_auto() {
        let runs = runs(EXPERIMENT, &RunOverrides::default()).unwrap();
        assert!(runs.iter().all(|r| r.config.run.autonomy == Autonomy::Auto));
    }

    #[test]
    fn a_run_without_a_loss_limit_refuses_to_start() {
        let without = EXPERIMENT.replace(
            "run: { capital_usd: 10000, loss_limit_pct: 5 }",
            "run: { capital_usd: 10000 }",
        );
        let err = runs(&without, &RunOverrides::default()).unwrap_err();
        assert!(format!("{err:#}").contains("loss_limit_pct"), "{err:#}");

        // The command line can supply it instead.
        let overrides = RunOverrides {
            loss_limit_pct: Some(3.0),
            ..Default::default()
        };
        let ok = runs(&without, &overrides).unwrap();
        assert_eq!(ok[0].config.run.loss_limit_pct, Some(3.0));
    }

    #[test]
    fn an_experiment_without_a_sweep_is_a_single_run() {
        let single = EXPERIMENT.split("sweep:").next().unwrap();
        let runs = runs(single, &RunOverrides::default()).unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].sweep_point.is_empty());
    }

    #[test]
    fn an_unknown_strategy_is_an_error_that_names_the_known_ones() {
        let wrong = EXPERIMENT.replace("strategy: eth-zone-roundtrip", "strategy: nope");
        let err = runs(&wrong, &RunOverrides::default()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nope") && msg.contains("eth-zone-roundtrip"),
            "{msg}"
        );
    }

    #[test]
    fn unknown_experiment_keys_are_rejected() {
        let typo = EXPERIMENT.replace("sweep:", "sweeep:");
        assert!(serde_yaml::from_str::<ExperimentFile>(&typo).is_err());
    }
}
