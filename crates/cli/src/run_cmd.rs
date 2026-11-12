//! `lpbot run`: every run of an experiment file as a backtest on the stored
//! history, with the result tables written at the end.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;

use alloy_primitives::{Address, U256};
use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::config::{Mode, StrategyConfig};
use lpbot_core::experiment::ResolvedRun;
use lpbot_core::ranking::rank;
use lpbot_core::{Config, Tick};
use lpbot_data::day::Day;
use lpbot_data::lake::{read_events, stored_pools, swaps, TimedEvent};
use lpbot_data::snapshot::{SnapshotStore, StoredSnapshot};
use lpbot_data::store::EventStore;
use lpbot_risk::LossLimit;
use lpbot_sim::engine::{self, GasModel, Settings, Token};
use lpbot_sim::tables::{write_run, PoolRankRow, RunInfo};
use lpbot_strategy::FixedRule;
use serde_yaml::Value;

use crate::chain_cmd::{chain_addresses, checked_address, Tokens};
use crate::rank_cmd::{build_stats, floors, reading_from_events, token_of};

const DAY: u64 = 86_400;
const RANK_DAYS: u32 = 7;

/// Reads `90s`, `30m`, `1h` or `7d` as seconds.
pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => DAY,
        _ => return None,
    };
    number.parse::<u64>().ok()?.checked_mul(seconds)
}

/// Follows a path of keys into a settings map.
fn lookup<'a>(map: &'a BTreeMap<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let mut value = map.get(*first)?;
    for key in rest {
        value = value.get(*key)?;
    }
    Some(value)
}

fn number(map: &BTreeMap<String, Value>, path: &[&str]) -> Option<f64> {
    lookup(map, path).and_then(Value::as_f64)
}

fn duration(map: &BTreeMap<String, Value>, path: &[&str], default: u64) -> Result<u64> {
    match lookup(map, path) {
        None => Ok(default),
        Some(value) => value
            .as_str()
            .and_then(parse_duration)
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| {
                anyhow!(
                    "`{}` must be a duration such as 30m, 1h or 7d",
                    path.join(".")
                )
            }),
    }
}

/// The range width of a run, in ticks. A sweep sets one number; the base
/// config's search space counts as its smallest width.
pub fn width_ticks(strategy: &StrategyConfig) -> Result<Tick> {
    let value = lookup(&strategy.extra, &["entry", "width_ticks"])
        .ok_or_else(|| anyhow!("the strategy has no entry.width_ticks"))?;
    let width = value
        .as_i64()
        .or_else(|| value.get("min").and_then(Value::as_i64))
        .ok_or_else(|| anyhow!("entry.width_ticks must be a number of ticks, or have a `min`"))?;
    Tick::try_from(width)
        .ok()
        .filter(|w| *w > 0)
        .ok_or_else(|| anyhow!("entry.width_ticks must be greater than zero"))
}

/// The zone lookbacks of a strategy, in days.
pub fn lookbacks(strategy: &StrategyConfig) -> Result<Vec<u32>> {
    let Some(list) = lookup(&strategy.extra, &["zone", "lookbacks"]) else {
        return Ok(vec![7, 30, 90]);
    };
    let days: Option<Vec<u32>> = list.as_sequence().and_then(|items| {
        items
            .iter()
            .map(|item| {
                let seconds = parse_duration(item.as_str()?)?;
                (seconds >= DAY && seconds % DAY == 0).then_some((seconds / DAY) as u32)
            })
            .collect()
    });
    days.filter(|d| !d.is_empty())
        .ok_or_else(|| anyhow!("zone.lookbacks must be a list of whole days, such as [7d, 30d]"))
}

/// A pool's stored history for a run, loaded once per sweep.
struct History {
    snapshot: StoredSnapshot,
    events: Vec<TimedEvent>,
}

/// Picks the pool a run trades and ranks the candidates as of `from`.
///
/// A `pool` address on the strategy decides it. Otherwise it is the stored
/// pool holding the value token that ranks highest by implied volatility
/// over the week before the window.
fn choose_pool(
    root: &Path,
    chain: &str,
    tokens: &Tokens,
    config: &Config,
    strategy: &StrategyConfig,
    from: Day,
) -> Result<(Address, Vec<PoolRankRow>)> {
    let value_address = tokens
        .get(&strategy.value_token)
        .map(|(address, _)| *address)
        .ok_or_else(|| anyhow!("value token {} has no address", strategy.value_token))?;
    let snapshots = SnapshotStore::new(root);
    let store = EventStore::new(root);

    let mut candidates = Vec::new();
    let mut readings = Vec::new();
    for pool in stored_pools(root, chain)? {
        let Some(snapshot) = snapshots.latest_on_or_before(chain, pool, from)? else {
            continue;
        };
        if snapshot.token0 != value_address && snapshot.token1 != value_address {
            continue;
        }
        // The week before the window, as far as it is stored.
        let stored = store.days(chain, pool)?;
        let before: Vec<Day> = stored
            .iter()
            .map(|(day, _)| *day)
            .filter(|day| *day < from && *day >= from.minus(RANK_DAYS) && *day >= snapshot.day)
            .collect();
        let events = match (before.first(), before.last()) {
            (Some(first), Some(last)) => read_events(root, chain, pool, *first, *last)?,
            _ => Vec::new(),
        };
        let token0 = token_of(tokens, snapshot.token0)?;
        let token1 = token_of(tokens, snapshot.token1)?;
        let id = format!(
            "{}/{} {}",
            token0.0,
            token1.0,
            snapshot.state.fee_pips as f64 / 10_000.0
        );
        readings.push(reading_from_events(
            id,
            (token0, token1),
            &snapshot.state,
            &events,
            from.start(),
            u64::from(RANK_DAYS),
        ));
        candidates.push(pool);
    }
    if candidates.is_empty() {
        bail!(
            "no pool holding {} is stored under {} with a snapshot on or before {from}; \
             run `lpbot backfill --from <an earlier date>` first",
            strategy.value_token,
            root.display()
        );
    }

    let (_, stats) = build_stats(
        &readings,
        &config.universe.usd_stables,
        f64::from(RANK_DAYS),
    );
    let ranked = rank(stats, &floors(config)?);
    let rows: Vec<PoolRankRow> = ranked
        .iter()
        .map(|r| PoolRankRow {
            time: from.start(),
            pool: r.stats.id.clone(),
            fee: r.stats.fee,
            volume_24h_usd: r.stats.volume_24h,
            volume_per_day_usd: r.stats.volume_per_day_long,
            depth_1pct_usd: r.stats.depth_1pct(),
            implied_vol_24h: r.iv_24h,
            implied_vol: r.iv_long,
            excluded: r.excluded.clone(),
        })
        .collect();

    if let Some(text) = strategy.extra.get("pool").and_then(Value::as_str) {
        let address = checked_address("strategy pool", text)?;
        if !candidates.contains(&address) {
            bail!("pool {address:#x} is not stored, or does not hold the value token");
        }
        return Ok((address, rows));
    }
    let top = &ranked[0].stats.id;
    let position = readings
        .iter()
        .position(|r| &r.id == top)
        .expect("ranked pools come from the readings");
    Ok((candidates[position], rows))
}

fn load_history(
    root: &Path,
    chain: &str,
    pool: Address,
    from: Day,
    to: Day,
    longest_lookback: u32,
) -> Result<History> {
    let snapshots = SnapshotStore::new(root);
    let days = snapshots.days(chain, pool)?;
    // The newest snapshot that still leaves the whole lookback as history,
    // or failing that the oldest one before the window.
    let wanted = from.minus(longest_lookback);
    let day = days
        .iter()
        .rfind(|day| **day <= wanted)
        .or_else(|| days.iter().find(|day| **day <= from))
        .ok_or_else(|| {
            anyhow!(
                "pool {pool:#x} has no snapshot on or before {from}; \
                 run `lpbot backfill --pool {pool:#x} --from {wanted}` first"
            )
        })?;
    let snapshot = snapshots.read(chain, pool, *day)?;
    let events = read_events(root, chain, pool, snapshot.day, to)?;
    Ok(History { snapshot, events })
}

/// Turns a resolved run into engine settings for a loaded pool.
fn settings(
    run: &ResolvedRun,
    strategy: &StrategyConfig,
    rule: FixedRule,
    history: &History,
    tokens: &Tokens,
    from: Day,
    to: Day,
) -> Result<(Settings, String)> {
    let config = &run.config;
    let (symbol0, decimals0) = token_of(tokens, history.snapshot.token0)?;
    let (symbol1, decimals1) = token_of(tokens, history.snapshot.token1)?;
    let value = if strategy.value_token == symbol0 {
        Token::Zero
    } else {
        Token::One
    };
    let (value_symbol, value_decimals, other_symbol) = match value {
        Token::Zero => (&symbol0, decimals0, &symbol1),
        Token::One => (&symbol1, decimals1, &symbol0),
    };

    // The pool price as trading starts: the last swap before the window.
    let trade_from = from.start();
    let sqrt_price = swaps(&history.events)
        .take_while(|(time, _)| *time < trade_from)
        .last()
        .map_or(history.snapshot.state.sqrt_price_x96, |(_, swap)| {
            swap.sqrt_price_x96
        });
    let sqrt = sqrt_price.to_string().parse::<f64>().unwrap_or(f64::NAN) / 2f64.powi(96);
    let token1_per_token0 = sqrt * sqrt * 10f64.powi(i32::from(decimals0) - i32::from(decimals1));
    // Whole other tokens one whole value token is worth.
    let other_per_value = match value {
        Token::Zero => token1_per_token0,
        Token::One => 1.0 / token1_per_token0,
    };

    let stables = &config.universe.usd_stables;
    let usd_per_value = if stables.contains(value_symbol) {
        1.0
    } else if stables.contains(other_symbol) {
        other_per_value
    } else {
        bail!(
            "capital is given in dollars, but the pool {symbol0}/{symbol1} has no dollar stable \
             to price {value_symbol} with; pools without one are not supported yet"
        );
    };
    let capital_usd = config
        .run
        .capital_usd
        .ok_or_else(|| anyhow!("no capital"))?;
    let unit = 10f64.powi(i32::from(value_decimals));
    let capital = U256::from((capital_usd / usd_per_value * unit) as u128);

    let costs = |key: &str| number(&config.extra, &["costs", key]);
    let units = |key: &str, default: u64| {
        number(&config.extra, &["costs", "gas_units", key]).map_or(default, |v| v as u64)
    };
    let value_per_eth = if value_symbol == "WETH" {
        1.0
    } else if other_symbol == "WETH" {
        other_per_value.recip()
    } else {
        costs("eth_price_usd").ok_or_else(|| {
            anyhow!("the pool holds no WETH; set costs.eth_price_usd so gas can be priced")
        })? / usd_per_value
    };
    let gas = GasModel {
        open_units: units("open", 450_000),
        close_units: units("close", 300_000),
        swap_units: units("swap", 180_000),
        gas_price_gwei: costs("gas_price_gwei").unwrap_or(1.0),
        value_raw_per_eth: value_per_eth * unit,
    };

    let spacing = history.snapshot.state.tick_spacing;
    // A width between two tick steps is widened to the next one.
    let width = (width_ticks(strategy)? + spacing - 1).div_euclid(spacing) * spacing;
    let loss_limit = config
        .run
        .loss_limit_pct
        .and_then(LossLimit::new)
        .ok_or_else(|| anyhow!("run.loss_limit_pct must be between 0 and 100"))?;

    let settings = Settings {
        value,
        decimals0,
        decimals1,
        width,
        max_gap: number(&strategy.extra, &["entry", "max_gap_ticks"]).map_or(width, |v| v as Tick),
        capital,
        capital_share: number(&strategy.extra, &["capital_share"]).unwrap_or(1.0),
        loss_limit,
        rule,
        decide_every: duration(&strategy.patience.extra, &["decide_every"], 3_600)?,
        check_every: duration(&strategy.extra, &["entry", "check_every"], 3_600)?,
        latency_blocks: number(&config.extra, &["sim", "latency_blocks"]).map_or(1, |v| v as u64),
        lookbacks_days: lookbacks(strategy)?,
        move_margin: number(&strategy.extra, &["entry", "move_margin"]).unwrap_or(1.5),
        gas,
        series_every: duration(&config.extra, &["sim", "series_every"], 3_600)?,
        trade_from,
        trade_to: to.end(),
    };
    let pool_name = format!(
        "{symbol0}/{symbol1} {}",
        history.snapshot.state.fee_pips as f64 / 10_000.0
    );
    Ok((settings, pool_name))
}

fn window(config: &Config) -> Result<(Day, Day)> {
    let window = config
        .window
        .as_ref()
        .ok_or_else(|| anyhow!("a backtest needs a window"))?;
    let from: Day = window.from.parse().map_err(anyhow::Error::msg)?;
    let to: Day = window.to.parse().map_err(anyhow::Error::msg)?;
    Ok((from, to))
}

/// Runs every run of an experiment and writes its tables under `results`.
pub fn run(runs: &[ResolvedRun], root: &Path, results: &Path, chain: &str) -> Result<()> {
    let mut histories: HashMap<(Address, Day, Day), Rc<History>> = HashMap::new();
    let mut pools: HashMap<(String, Day), (Address, Vec<PoolRankRow>)> = HashMap::new();
    let (mut done, mut skipped) = (0, 0);

    println!(
        "{:<44} {:<16} {:>6} {:>9} {:>9} {:>9} {:>6} {:>7} {:>7}",
        "run", "pool", "rule", "result %", "fees", "gas", "camps", "passes", "max dd"
    );
    for run in runs {
        let config = &run.config;
        if config.mode != Mode::Backtest {
            bail!("only `mode: backtest` runs so far; paper and live runs are not built yet");
        }
        let strategy = config
            .strategies
            .first()
            .ok_or_else(|| anyhow!("the run has no strategy"))?;
        if strategy.kind != "zone_round_trip" {
            bail!("strategy kind `{}` is not built yet", strategy.kind);
        }
        let Some(rule) = FixedRule::from_mode(strategy.patience.mode) else {
            println!(
                "{:<44} skipped: the adaptive patience decision is not built yet",
                run.run_id
            );
            skipped += 1;
            continue;
        };
        let (from, to) = window(config)?;
        let (_, tokens) = chain_addresses(config, chain)?;

        let key = (strategy.value_token.clone(), from);
        if !pools.contains_key(&key) {
            let chosen = choose_pool(root, chain, &tokens, config, strategy, from)?;
            pools.insert(key.clone(), chosen);
        }
        let (pool, rank_rows) = &pools[&key];

        let longest = lookbacks(strategy)?.into_iter().max().unwrap_or(0);
        let history = match histories.get(&(*pool, from, to)) {
            Some(history) => Rc::clone(history),
            None => {
                let loaded = Rc::new(load_history(root, chain, *pool, from, to, longest)?);
                histories.insert((*pool, from, to), Rc::clone(&loaded));
                loaded
            }
        };

        let (settings, pool_name) = settings(run, strategy, rule, &history, &tokens, from, to)?;
        let out = engine::run(history.snapshot.state.clone(), &history.events, &settings)
            .with_context(|| format!("run {} failed", run.run_id))?;

        let info = RunInfo {
            run_id: run.run_id.clone(),
            experiment: run.experiment.clone(),
            index: run.index,
            config_hash: run.config_hash.clone(),
            code_version: env!("CARGO_PKG_VERSION").to_string(),
            mode: "backtest".to_string(),
            window_from: from.to_string(),
            window_to: to.to_string(),
            capital_usd: config.run.capital_usd.unwrap_or_default(),
            loss_limit_pct: config.run.loss_limit_pct.unwrap_or_default(),
            autonomy: format!("{:?}", config.run.autonomy).to_lowercase(),
            strategy: strategy.id.clone(),
            patience: format!("{rule:?}").to_lowercase(),
            pool: pool_name.clone(),
            value_token: strategy.value_token.clone(),
            fee_pips: history.snapshot.state.fee_pips,
            width_ticks: settings.width,
            sweep: run
                .sweep_point
                .iter()
                .map(|(path, value)| format!("{path}={value}"))
                .collect::<Vec<_>>()
                .join("; "),
            config_yaml: run.config_yaml.clone(),
        };
        write_run(results, &info, &out, rank_rows)?;

        let s = &out.summary;
        println!(
            "{:<44} {:<16} {:>6} {:>9.3} {:>9.5} {:>9.5} {:>6} {:>7} {:>6.2}%{}",
            run.run_id,
            pool_name,
            info.patience,
            s.result_pct,
            s.fees,
            s.gas,
            s.campaigns,
            s.passes,
            s.max_drawdown_pct,
            if s.halted_by_loss_limit {
                "  stopped by the loss limit"
            } else {
                ""
            }
        );
        done += 1;
    }

    println!();
    println!(
        "{done} run(s) written to {} ({} tables each); results, fees and gas are in the value token",
        results.display(),
        lpbot_sim::RESULT_TABLES.len()
    );
    if skipped > 0 {
        println!("{skipped} run(s) skipped: the adaptive patience decision is phase 2");
    }
    if done == 0 && skipped == 0 {
        bail!("the experiment has no runs");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strategy(yaml: &str) -> StrategyConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    const BASE: &str = "id: s\nkind: zone_round_trip\nvalue_token: WETH\n";

    #[test]
    fn the_demo_experiment_runs_end_to_end_and_writes_all_six_tables() {
        use lpbot_core::experiment::{resolve_experiment, RunOverrides};
        use lpbot_sim::tables::{read_table, table_path, Column};

        let root = std::env::temp_dir().join(format!("lpbot-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (lake, results) = (root.join("demo"), root.join("runs"));
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        let runs = resolve_experiment(
            &repo.join("experiments/demo-fixed-rules.yml"),
            &RunOverrides::default(),
        )
        .unwrap();
        assert_eq!(runs.len(), 9);
        let (_, tokens) = chain_addresses(&runs[0].config, "ethereum").unwrap();
        crate::demo_cmd::run(&lake, "ethereum", &tokens, 30, 1).unwrap();

        run(&runs, &lake, &results, "ethereum").unwrap();

        for resolved in &runs {
            for table in lpbot_sim::RESULT_TABLES {
                let path = table_path(&results, table, &resolved.run_id);
                assert!(path.is_file(), "{}", path.display());
            }
            let summary = read_table(&table_path(&results, "runs", &resolved.run_id)).unwrap();
            assert_eq!(summary.rows(), 1);
            assert_eq!(
                summary.column("config_hash"),
                Some(&Column::Text(vec![Some(resolved.config_hash.clone())]))
            );
            // 10,000 dollars at about 2,000 dollars per ETH.
            let Some(Column::Float(start)) = summary.column("start_capital") else {
                panic!("no start capital");
            };
            assert!((start[0].unwrap() - 5.0).abs() < 0.2, "{:?}", start);
            let series = read_table(&table_path(&results, "series", &resolved.run_id)).unwrap();
            assert!(series.rows() > 100);
            let rank = read_table(&table_path(&results, "pool_rank", &resolved.run_id)).unwrap();
            assert_eq!(rank.rows(), 1);
            let zones = read_table(&table_path(&results, "zone_map", &resolved.run_id)).unwrap();
            assert!(zones.rows() > 0);
        }

        // The same file, data and code give the same result.
        let first = table_path(&results, "campaigns", &runs[0].run_id);
        let before = std::fs::read(&first).unwrap();
        run(&runs[..1], &lake, &results, "ethereum").unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), before);

        // A window the stored history does not cover is refused with the remedy.
        let mut late = runs[0].clone();
        late.config.window.as_mut().unwrap().to = "2026-03-01".into();
        let error = run(&[late], &lake, &results, "ethereum").unwrap_err();
        assert!(format!("{error:#}").contains("lpbot backfill"), "{error:#}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn durations_are_read_with_their_unit() {
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("30m"), Some(1_800));
        assert_eq!(parse_duration("1h"), Some(3_600));
        assert_eq!(parse_duration("7d"), Some(604_800));
        for bad in ["", "h", "1", "1w", "-1h", "1.5h", "one hour"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_width_is_a_number_or_the_low_end_of_the_search_space() {
        let swept = strategy(&format!("{BASE}entry: {{ width_ticks: 40 }}\n"));
        assert_eq!(width_ticks(&swept).unwrap(), 40);
        let space = strategy(&format!(
            "{BASE}entry: {{ width_ticks: {{ min: 10, max: 80 }} }}\n"
        ));
        assert_eq!(width_ticks(&space).unwrap(), 10);
        for bad in [
            "entry: { width_ticks: 0 }",
            "entry: { width_ticks: wide }",
            "entry: {}",
            "",
        ] {
            assert!(
                width_ticks(&strategy(&format!("{BASE}{bad}\n"))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn lookbacks_are_whole_days_with_a_default() {
        assert_eq!(lookbacks(&strategy(BASE)).unwrap(), vec![7, 30, 90]);
        let two = strategy(&format!("{BASE}zone: {{ lookbacks: [1d, 14d] }}\n"));
        assert_eq!(lookbacks(&two).unwrap(), vec![1, 14]);
        for bad in ["[12h]", "[]", "[7]", "weekly"] {
            let s = strategy(&format!("{BASE}zone: {{ lookbacks: {bad} }}\n"));
            assert!(lookbacks(&s).is_err(), "{bad}");
        }
    }

    #[test]
    fn settings_paths_are_followed_into_nested_maps() {
        let s = strategy(&format!(
            "{BASE}capital_share: 0.25\nentry: {{ check_every: 30m, move_margin: 2 }}\n"
        ));
        assert_eq!(number(&s.extra, &["capital_share"]), Some(0.25));
        assert_eq!(number(&s.extra, &["entry", "move_margin"]), Some(2.0));
        assert_eq!(number(&s.extra, &["entry", "absent"]), None);
        assert_eq!(
            duration(&s.extra, &["entry", "check_every"], 5).unwrap(),
            1_800
        );
        assert_eq!(duration(&s.extra, &["entry", "absent"], 5).unwrap(), 5);
        let bad = strategy(&format!("{BASE}entry: {{ check_every: soon }}\n"));
        let error = duration(&bad.extra, &["entry", "check_every"], 5).unwrap_err();
        assert!(error.to_string().contains("entry.check_every"), "{error}");
    }
}
