//! `lpbot run`: every run of an experiment file as a backtest on the stored
//! history, with the result tables written at the end.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::rc::Rc;

use alloy_primitives::{Address, U256};
use anyhow::{anyhow, bail, Context, Result};
use lpbot_core::config::{Mode, StrategyConfig};
use lpbot_core::experiment::ResolvedRun;
use lpbot_core::{Config, Tick};
use lpbot_data::day::Day;
use lpbot_data::lake::{read_events, stored_pools, swaps, TimedEvent};
use lpbot_data::snapshot::{SnapshotStore, StoredSnapshot};
use lpbot_data::store::EventStore;
use lpbot_risk::LossLimit;
use lpbot_sim::engine::{self, Dollar, GasModel, Policy, Settings, Token};
use lpbot_sim::tables::{write_run, PoolRankRow, RunInfo};
use lpbot_strategy::adaptive::AdaptiveParams;
use lpbot_strategy::FixedRule;
use serde_yaml::Value;

use crate::chain_cmd::{chain_addresses, checked_address, Tokens};
use crate::rank_cmd::{floors, rank_readings, reading_from_events, token_of};

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

/// A duration that may be left out or null: `None` then.
fn optional_duration(map: &BTreeMap<String, Value>, path: &[&str]) -> Result<Option<u64>> {
    match lookup(map, path) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => duration(map, path, 0).map(Some),
    }
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

/// How a run decides when price has left a position behind: a fixed rule,
/// or the adaptive decision with its settings. Settings left at
/// `calibrated` take the defaults until a calibration pins them.
pub fn policy(strategy: &StrategyConfig) -> Result<Policy> {
    if let Some(rule) = FixedRule::from_mode(strategy.patience.mode) {
        return Ok(Policy::Fixed(rule));
    }
    let extra = &strategy.patience.extra;
    let defaults = AdaptiveParams::default();
    // A percentage of the position's value, or `calibrated`.
    let switch_margin = match extra.get("switch_margin") {
        None => defaults.switch_margin,
        Some(value) if value.as_str() == Some("calibrated") => defaults.switch_margin,
        Some(value) => {
            value
                .as_f64()
                .filter(|pct| (0.0..=100.0).contains(pct))
                .ok_or_else(|| {
                    anyhow!("patience.switch_margin must be a percentage, or `calibrated`")
                })?
                / 100.0
        }
    };
    let reinvest_credit = match number(extra, &["reinvest_credit"]) {
        None => defaults.reinvest_credit,
        Some(credit) if (0.0..=1.0).contains(&credit) => credit,
        Some(_) => bail!("patience.reinvest_credit must be between 0 and 1"),
    };
    let lookback = duration(
        extra,
        &["lookback"],
        u64::from(defaults.lookback_days) * DAY,
    )?;
    Ok(Policy::Adaptive(AdaptiveParams {
        horizon: duration(extra, &["horizon"], defaults.horizon)?,
        switch_margin,
        reinvest_credit,
        lookback_days: (lookback / DAY).max(1) as u32,
    }))
}

fn policy_name(policy: &Policy) -> String {
    match policy {
        Policy::Fixed(rule) => format!("{rule:?}").to_lowercase(),
        Policy::Adaptive(_) => "adaptive".to_string(),
    }
}

/// A pool's stored history for a run, loaded once per sweep.
struct History {
    snapshot: StoredSnapshot,
    events: Vec<TimedEvent>,
}

/// The pool a strategy names with `pool`, if it names one.
pub(crate) fn named_pool(strategy: &StrategyConfig) -> Result<Option<Address>> {
    match strategy.extra.get("pool") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => checked_address("strategy pool", text).map(Some),
        Some(_) => bail!(
            "the strategy's `pool` must be a checksummed pool address; leave it out to trade \
             the pool that ranks highest"
        ),
    }
}

/// What the choice of a run's pool depends on. Runs with the same key
/// trade the same pool and share its ranking.
pub(crate) type PoolKey = (String, Day, Option<Address>);

/// The pool key of a run: its value token, the first day of its window and
/// the pool its strategy names, if any.
pub(crate) fn pool_key(strategy: &StrategyConfig, from: Day) -> Result<PoolKey> {
    Ok((strategy.value_token.clone(), from, named_pool(strategy)?))
}

/// Picks the pool a run trades and ranks the candidates as of `from`.
///
/// A `pool` address on the strategy decides it, even when the pool fails
/// the floors (with a warning). Otherwise it is the stored pool holding
/// the value token that ranks highest by implied volatility over the week
/// before the window; when none clears the floors, the run is refused.
/// Each pool's daily volume is averaged over the days of that week that
/// are stored.
pub(crate) fn choose_pool(
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
    let named = named_pool(strategy)?;
    let snapshots = SnapshotStore::new(root);
    let store = EventStore::new(root);
    let week_start = from.minus(RANK_DAYS);

    let mut candidates = Vec::new();
    let mut readings = Vec::new();
    for pool in stored_pools(root, chain)? {
        let Some(snapshot) = snapshots.latest_on_or_before(chain, pool, from)? else {
            continue;
        };
        if snapshot.token0 != value_address && snapshot.token1 != value_address {
            continue;
        }
        // The week before the window, as far as it is stored. Read day by
        // day, so a day missing in between only shortens what is averaged.
        let before: Vec<Day> = store
            .days(chain, pool)?
            .into_iter()
            .map(|(day, _)| day)
            .filter(|day| *day < from && *day >= week_start)
            .collect();
        let mut events = Vec::new();
        for day in &before {
            events.extend(read_events(root, chain, pool, *day, *day)?);
        }
        let token0 = token_of(tokens, snapshot.token0)?;
        let token1 = token_of(tokens, snapshot.token1)?;
        let id = format!(
            "{}/{} {}",
            token0.0,
            token1.0,
            snapshot.state.fee_pips as f64 / 10_000.0
        );
        if before.len() < RANK_DAYS as usize {
            println!(
                "warning: {id} ({pool:#x}) has {} of the {RANK_DAYS} days before {from} stored; \
                 its daily volume is the average over {}. `lpbot backfill --from {week_start}` \
                 stores the rest",
                before.len(),
                match before.len() {
                    0 => "no day, so it is not ranked".to_string(),
                    1 => "that one day".to_string(),
                    n => format!("those {n} days"),
                }
            );
        }
        readings.push(reading_from_events(
            id,
            (token0, token1),
            &snapshot,
            &events,
            (from.start(), u64::from(RANK_DAYS)),
            before.len() as f64,
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

    let floors_config = floors(config)?;
    let (_, ranked) = rank_readings(&readings, &config.universe.usd_stables, &floors_config);
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

    if let Some(address) = named {
        let Some(position) = candidates.iter().position(|pool| *pool == address) else {
            bail!("pool {address:#x} is not stored, or does not hold the value token");
        };
        let id = &readings[position].id;
        if let Some(reason) = ranked
            .iter()
            .find(|r| &r.stats.id == id)
            .and_then(|r| r.excluded.as_ref())
        {
            println!(
                "warning: {id} ({address:#x}) fails universe.floors as of {from}: {reason}. \
                 It is traded because the strategy names it"
            );
        }
        return Ok((address, rows));
    }
    let top = &ranked[0];
    if top.excluded.is_some() {
        let reasons: Vec<String> = ranked
            .iter()
            .map(|r| format!("  {}: {}", r.stats.id, r.excluded.as_deref().unwrap_or("")))
            .collect();
        bail!(
            "no stored pool holding {} clears universe.floors (volume_usd_per_day {}, \
             in_range_liquidity_usd {}) over the {RANK_DAYS} days before {from}:\n{}\n\
             Backfill from an earlier date so the week before the window is stored \
             (`lpbot backfill --from {week_start}`), lower universe.floors in the base config, \
             or name a pool with `pool:` in the strategy",
            strategy.value_token,
            floors_config.volume_per_day,
            floors_config.depth_1pct,
            reasons.join("\n")
        );
    }
    let position = readings
        .iter()
        .position(|r| r.id == top.stats.id)
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
    policy: Policy,
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
        dollar: if stables.contains(value_symbol) {
            Dollar::Value
        } else {
            Dollar::Other
        },
        decimals0,
        decimals1,
        width,
        max_gap: number(&strategy.extra, &["entry", "max_gap_ticks"]).map_or(width, |v| v as Tick),
        capital,
        capital_share: number(&strategy.extra, &["capital_share"]).unwrap_or(1.0),
        max_share_of_in_range_liquidity: number(
            &config.universe.extra,
            &["max_share_of_in_range_liquidity"],
        ),
        loss_limit,
        policy,
        decide_every: duration(&strategy.patience.extra, &["decide_every"], 3_600)?,
        check_every: duration(&strategy.extra, &["entry", "check_every"], 3_600)?,
        latency_blocks: number(&config.extra, &["sim", "latency_blocks"]).map_or(1, |v| v as u64),
        lookbacks_days: lookbacks(strategy)?,
        move_margin: number(&strategy.extra, &["entry", "move_margin"]).unwrap_or(1.5),
        min_passes_per_day: number(&strategy.extra, &["entry", "min_passes_per_day"])
            .unwrap_or(0.0),
        max_vol_to_width: number(&strategy.extra, &["entry", "max_vol_to_width"]),
        max_daily_vol_ticks: number(&strategy.extra, &["entry", "max_daily_vol_pct"])
            .map(|pct| (1.0 + pct / 100.0).ln() / 1.0001f64.ln()),
        min_gas_multiple: number(&strategy.extra, &["entry", "min_gas_multiple"]),
        max_seconds_beyond: optional_duration(&strategy.patience.extra, &["max_wait"])?,
        exit_vol_to_width: number(&strategy.extra, &["exit", "max_vol_to_width"]),
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

pub(crate) fn window(config: &Config) -> Result<(Day, Day)> {
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
    // A history depends on the pool, the window and how far back the run
    // looks; a pool on what `pool_key` holds. Runs share what they agree on.
    let mut histories: HashMap<(Address, Day, Day, u32), Rc<History>> = HashMap::new();
    let mut pools: HashMap<PoolKey, (Address, Vec<PoolRankRow>)> = HashMap::new();
    let mut done = 0;

    println!(
        "{:<44} {:<16} {:>8} {:>9} {:>8} {:>10} {:>9} {:>9} {:>6} {:>7} {:>7}",
        "run",
        "pool",
        "rule",
        "result %",
        "USD %",
        "hold USD %",
        "fees",
        "gas",
        "camps",
        "passes",
        "max dd"
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
        let policy = policy(strategy)?;
        let (from, to) = window(config)?;
        let (_, tokens) = chain_addresses(config, chain)?;

        let key = pool_key(strategy, from)?;
        if !pools.contains_key(&key) {
            let chosen = choose_pool(root, chain, &tokens, config, strategy, from)?;
            pools.insert(key.clone(), chosen);
        }
        let (pool, rank_rows) = &pools[&key];

        let longest = lookbacks(strategy)?.into_iter().max().unwrap_or(0);
        let history_key = (*pool, from, to, longest);
        let history = match histories.get(&history_key) {
            Some(history) => Rc::clone(history),
            None => {
                let loaded = Rc::new(load_history(root, chain, *pool, from, to, longest)?);
                histories.insert(history_key, Rc::clone(&loaded));
                loaded
            }
        };

        let (settings, pool_name) = settings(run, strategy, policy, &history, &tokens, from, to)?;
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
            patience: policy_name(&policy),
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
            "{:<44} {:<16} {:>8} {:>9.3} {:>8.3} {:>10.3} {:>9.5} {:>9.5} {:>6} {:>7} {:>6.2}%{}",
            run.run_id,
            pool_name,
            info.patience,
            s.result_pct,
            s.result_pct_usd,
            s.hold_value_pct_usd,
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
        "{done} run(s) written to {} ({} tables each). Results, fees and gas are in the value token; \
         USD % is the result in dollars, the capital valued at the pool price at the start and \
         at the end; hold USD % is what simply holding the value token did in dollars",
        results.display(),
        lpbot_sim::RESULT_TABLES.len()
    );
    if done == 0 {
        bail!("the experiment has no runs");
    }
    Ok(())
}

/// The settings the calibrator tries for the adaptive decision.
pub fn calibration_grid(lookback_days: u32) -> Vec<AdaptiveParams> {
    let mut grid = Vec::new();
    for horizon in [6 * 3_600, 24 * 3_600, 72 * 3_600] {
        for switch_margin in [0.001, 0.003, 0.01] {
            for reinvest_credit in [0.0, 0.25] {
                grid.push(AdaptiveParams {
                    horizon,
                    switch_margin,
                    reinvest_credit,
                    lookback_days,
                });
            }
        }
    }
    grid
}

/// One step of a walk-forward calibration.
#[derive(Debug, Clone, PartialEq)]
pub struct Fold {
    pub fit_from: Day,
    pub score_from: Day,
    pub score_to: Day,
    /// The settings that did best on the fit window.
    pub chosen: AdaptiveParams,
    /// Result on the fit window with those settings, in percent.
    pub fit_result: f64,
    /// Results on the score window, in percent: the adaptive decision
    /// with the chosen settings, then always wait, flip and close.
    pub adaptive: f64,
    pub fixed: [f64; 3],
}

const FIXED: [(FixedRule, &str); 3] = [
    (FixedRule::Wait, "wait"),
    (FixedRule::Flip, "flip"),
    (FixedRule::Close, "close"),
];

/// The windows of a walk-forward calibration: fit on `fit_days`, score on
/// the `score_days` that follow, then move both on by `score_days`.
pub fn fold_windows(from: Day, to: Day, fit_days: u32, score_days: u32) -> Vec<(Day, Day, Day)> {
    let mut windows = Vec::new();
    if fit_days == 0 || score_days == 0 {
        return windows;
    }
    let mut fit_from = from;
    loop {
        let score_from = Day(fit_from.0 + fit_days);
        let score_to = Day(score_from.0 + score_days - 1);
        if score_to > to {
            return windows;
        }
        windows.push((fit_from, score_from, score_to));
        fit_from = Day(fit_from.0 + score_days);
    }
}

/// What a calibration concluded.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// Sum of the score-window results, in percent.
    pub adaptive_total: f64,
    pub fixed_totals: [f64; 3],
    /// `adaptive`, or the fixed rule that ships instead.
    pub ships: &'static str,
    /// The settings fitted on the newest window.
    pub latest: Option<AdaptiveParams>,
}

/// The adaptive decision ships only if it beat every fixed rule out of
/// sample, summed over the folds. Otherwise the best fixed rule ships.
pub fn verdict(folds: &[Fold]) -> Verdict {
    let adaptive_total: f64 = folds.iter().map(|f| f.adaptive).sum();
    let mut fixed_totals = [0.0; 3];
    for fold in folds {
        for (total, result) in fixed_totals.iter_mut().zip(fold.fixed) {
            *total += result;
        }
    }
    let (best, best_total) = FIXED.iter().zip(fixed_totals).fold(
        ("wait", f64::NEG_INFINITY),
        |best, ((_, name), total)| {
            if total > best.1 {
                (name, total)
            } else {
                best
            }
        },
    );
    Verdict {
        adaptive_total,
        fixed_totals,
        ships: if !folds.is_empty() && adaptive_total > best_total {
            "adaptive"
        } else {
            best
        },
        latest: folds.last().map(|f| f.chosen),
    }
}

/// Walk-forward calibration of the adaptive decision for every adaptive
/// run of an experiment: fit its settings on one window, score them on
/// the next against the three fixed rules, and say which one ships.
/// Returns what ships for each calibrated run: `adaptive` or a fixed rule.
pub fn calibrate(
    runs: &[ResolvedRun],
    root: &Path,
    results: &Path,
    chain: &str,
    fit_days: Option<u32>,
    score_days: Option<u32>,
) -> Result<Vec<(String, &'static str)>> {
    let mut calibrated = Vec::new();
    for run in runs {
        let config = &run.config;
        let strategy = config
            .strategies
            .first()
            .ok_or_else(|| anyhow!("the run has no strategy"))?;
        let Policy::Adaptive(base) = policy(strategy)? else {
            continue;
        };
        let window_days = |key: &str, default: u64| -> Result<u32> {
            Ok(
                (duration(&config.extra, &["calibration", key], default * DAY)? / DAY).max(1)
                    as u32,
            )
        };
        let fit_days = match fit_days {
            Some(days) => days,
            None => window_days("fit_window", 90)?,
        };
        let score_days = match score_days {
            Some(days) => days,
            None => window_days("score_window", 14)?,
        };

        let (from, to) = window(config)?;
        let windows = fold_windows(from, to, fit_days, score_days);
        if windows.is_empty() {
            bail!(
                "the window {from} to {to} is shorter than one fit window of {fit_days} days plus \
                 one score window of {score_days} days; widen it or pass --fit and --score"
            );
        }
        let (_, tokens) = chain_addresses(config, chain)?;
        let (pool, _) = choose_pool(root, chain, &tokens, config, strategy, from)?;
        let longest = lookbacks(strategy)?.into_iter().max().unwrap_or(0);
        let history = load_history(root, chain, pool, from, to, longest)?;
        let (base_settings, pool_name) = settings(
            run,
            strategy,
            Policy::Adaptive(base),
            &history,
            &tokens,
            from,
            to,
        )?;
        let result = |policy: Policy, first: Day, last: Day| -> Result<f64> {
            let settings = Settings {
                policy,
                trade_from: first.start(),
                trade_to: last.end(),
                ..base_settings.clone()
            };
            Ok(
                engine::run(history.snapshot.state.clone(), &history.events, &settings)?
                    .summary
                    .result_pct,
            )
        };

        println!(
            "{}: {pool_name}, width {} ticks, fit {fit_days} days, score {score_days} days",
            run.run_id, base_settings.width
        );
        println!(
            "  {:<24} {:>8} {:>8} {:>7} {:>9} {:>9} {:>8} {:>8} {:>8}",
            "score window",
            "horizon",
            "margin",
            "credit",
            "fit %",
            "adaptive",
            "wait",
            "flip",
            "close"
        );
        let mut folds = Vec::new();
        for (fit_from, score_from, score_to) in windows {
            let fit_to = Day(score_from.0 - 1);
            let mut best: Option<(AdaptiveParams, f64)> = None;
            for params in calibration_grid(base.lookback_days) {
                let fitted = result(Policy::Adaptive(params), fit_from, fit_to)?;
                if best.is_none_or(|(_, top)| fitted > top) {
                    best = Some((params, fitted));
                }
            }
            let (chosen, fit_result) = best.expect("the grid is not empty");
            let mut fixed = [0.0; 3];
            for (slot, (rule, _)) in fixed.iter_mut().zip(FIXED) {
                *slot = result(Policy::Fixed(rule), score_from, score_to)?;
            }
            let fold = Fold {
                fit_from,
                score_from,
                score_to,
                chosen,
                fit_result,
                adaptive: result(Policy::Adaptive(chosen), score_from, score_to)?,
                fixed,
            };
            println!(
                "  {:<24} {:>7}h {:>7.1}% {:>7.2} {:>9.3} {:>9.3} {:>8.3} {:>8.3} {:>8.3}",
                format!("{score_from} to {score_to}"),
                chosen.horizon / 3_600,
                chosen.switch_margin * 100.0,
                chosen.reinvest_credit,
                fold.fit_result,
                fold.adaptive,
                fold.fixed[0],
                fold.fixed[1],
                fold.fixed[2]
            );
            folds.push(fold);
        }

        let verdict = verdict(&folds);
        println!(
            "  {:<24} {:>8} {:>8} {:>7} {:>9} {:>9.3} {:>8.3} {:>8.3} {:>8.3}",
            "out of sample, summed",
            "",
            "",
            "",
            "",
            verdict.adaptive_total,
            verdict.fixed_totals[0],
            verdict.fixed_totals[1],
            verdict.fixed_totals[2]
        );
        let path = results
            .join("calibration")
            .join(format!("{}.yml", run.run_id));
        let mut report = format!(
            "# Walk-forward calibration of the adaptive patience decision.\nrun: {}\npool: {pool_name}\nwidth_ticks: {}\nfit_days: {fit_days}\nscore_days: {score_days}\nships: {}\nout_of_sample_pct: {{ adaptive: {:.4}, wait: {:.4}, flip: {:.4}, close: {:.4} }}\n",
            run.run_id,
            base_settings.width,
            verdict.ships,
            verdict.adaptive_total,
            verdict.fixed_totals[0],
            verdict.fixed_totals[1],
            verdict.fixed_totals[2]
        );
        if verdict.ships == "adaptive" {
            let p = verdict.latest.expect("a verdict for adaptive has folds");
            println!(
                "  the adaptive decision beat all three fixed rules out of sample: it ships, with \
                 the settings fitted on the newest window"
            );
            report.push_str(&format!(
                "patience: {{ mode: adaptive, horizon: {}h, switch_margin: {}, reinvest_credit: {}, lookback: {}d }}\n",
                p.horizon / 3_600,
                p.switch_margin * 100.0,
                p.reinvest_credit,
                p.lookback_days
            ));
        } else {
            println!(
                "  the adaptive decision did not beat always-{} out of sample: the fixed rule ships",
                verdict.ships
            );
            report.push_str(&format!("patience: {{ mode: {} }}\n", verdict.ships));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::write(&path, report)
            .with_context(|| format!("cannot write {}", path.display()))?;
        println!("  written to {}", path.display());
        println!();
        calibrated.push((run.run_id.clone(), verdict.ships));
    }
    if calibrated.is_empty() {
        bail!("the experiment has no run with `patience.mode: adaptive` to calibrate");
    }
    Ok(calibrated)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chain_cmd::Tokens;
    use crate::demo_cmd::DEMO_POOL;
    use alloy_primitives::address;
    use lpbot_core::config::FloorsConfig;
    use lpbot_core::experiment::{resolve_experiment, RunOverrides};
    use lpbot_sim::tables::table_path;
    use std::path::PathBuf;

    fn strategy(yaml: &str) -> StrategyConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    const BASE: &str = "id: s\nkind: zone_round_trip\nvalue_token: WETH\n";

    /// Where a second simulated pool is stored, next to the demo pool.
    pub(crate) const OTHER_POOL: Address = address!("5157000000000000000000000000000000000002");

    /// A second simulated USDC/WETH pool, from another seed, stored under
    /// `OTHER_POOL` beside the demo pool in `lake`.
    pub(crate) fn store_other_pool(lake: &Path, tokens: &Tokens, days: u32) {
        let elsewhere = lake.with_extension("other");
        let _ = std::fs::remove_dir_all(&elsewhere);
        crate::demo_cmd::run(&elsewhere, "ethereum", tokens, days, 2).unwrap();
        for kind in ["events", "snapshots"] {
            let folder = |root: &Path, pool: Address| {
                root.join(kind)
                    .join("chain=ethereum")
                    .join(format!("pool={pool:#x}"))
            };
            std::fs::rename(folder(&elsewhere, DEMO_POOL), folder(lake, OTHER_POOL)).unwrap();
        }
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// The demo history over `days` days in a fresh folder, and the demo
    /// experiment's runs on a window of three days so the test stays quick.
    fn demo(label: &str, days: u32) -> (PathBuf, Vec<ResolvedRun>, Tokens) {
        let root = std::env::temp_dir().join(format!("lpbot-run-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut runs = resolve_experiment(
            &repo.join("experiments/demo-patience.yml"),
            &RunOverrides::default(),
        )
        .unwrap();
        for run in &mut runs {
            run.config.window.as_mut().unwrap().to = "2026-01-10".into();
        }
        let (_, tokens) = chain_addresses(&runs[0].config, "ethereum").unwrap();
        crate::demo_cmd::run(&root.join("lake"), "ethereum", &tokens, days, 1).unwrap();
        (root, runs, tokens)
    }

    /// The six tables of a run, as written.
    fn tables(results: &Path, run_id: &str) -> Vec<Vec<u8>> {
        lpbot_sim::RESULT_TABLES
            .iter()
            .map(|table| std::fs::read(table_path(results, table, run_id)).unwrap())
            .collect()
    }

    fn day(text: &str) -> Day {
        text.parse().unwrap()
    }

    #[test]
    fn a_named_pool_is_part_of_what_picks_the_pool() {
        let checksummed = DEMO_POOL.to_checksum(None);
        let named = strategy(&format!("{BASE}pool: \"{checksummed}\"\n"));
        assert_eq!(named_pool(&named).unwrap(), Some(DEMO_POOL));
        assert_eq!(named_pool(&strategy(BASE)).unwrap(), None);
        assert_eq!(
            named_pool(&strategy(&format!("{BASE}pool: null\n"))).unwrap(),
            None
        );
        for bad in ["pool: 5", "pool: [a, b]", "pool: \"0x51\""] {
            assert!(
                named_pool(&strategy(&format!("{BASE}{bad}\n"))).is_err(),
                "{bad}"
            );
        }
        let from = day("2026-01-08");
        let other = strategy(&format!(
            "{BASE}pool: \"{}\"\n",
            OTHER_POOL.to_checksum(None)
        ));
        assert_ne!(
            pool_key(&named, from).unwrap(),
            pool_key(&other, from).unwrap()
        );
        assert_ne!(
            pool_key(&named, from).unwrap(),
            pool_key(&strategy(BASE), from).unwrap()
        );
    }

    #[test]
    fn a_sweep_over_pools_trades_each_run_on_its_own_pool() {
        let (root, runs, tokens) = demo("pools", 11);
        let lake = root.join("lake");
        store_other_pool(&lake, &tokens, 11);

        let named = |run: &ResolvedRun, pool: Address, suffix: &str| {
            let mut run = run.clone();
            run.run_id = format!("{}-{suffix}", run.run_id);
            run.config.strategies[0]
                .extra
                .insert("pool".into(), Value::String(pool.to_checksum(None)));
            run
        };
        let first = named(&runs[1], DEMO_POOL, "demo");
        let second = named(&runs[1], OTHER_POOL, "other");
        run(
            &[first.clone(), second.clone()],
            &lake,
            &root.join("both"),
            "ethereum",
        )
        .unwrap();
        run(
            std::slice::from_ref(&second),
            &lake,
            &root.join("alone"),
            "ethereum",
        )
        .unwrap();

        // The second run traded its own pool, as it does on its own.
        assert_eq!(
            tables(&root.join("both"), &second.run_id),
            tables(&root.join("alone"), &second.run_id)
        );
        // And the two pools do give different results.
        let summary = |run_id: &str| {
            let table =
                lpbot_sim::tables::read_table(&table_path(&root.join("both"), "runs", run_id))
                    .unwrap();
            table.column("end_capital").cloned()
        };
        assert_ne!(summary(&first.run_id), summary(&second.run_id));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn runs_that_look_back_differently_load_their_own_history() {
        use lpbot_data::lake::replay_from_snapshot;

        let (root, runs, _) = demo("lookback", 11);
        let lake = root.join("lake");
        // A second snapshot, on 5 January: the state after the first four days.
        let snapshots = SnapshotStore::new(&lake);
        let first = snapshots
            .read("ethereum", DEMO_POOL, day("2026-01-01"))
            .unwrap();
        let (pool, _) =
            replay_from_snapshot(&lake, "ethereum", first.clone(), day("2026-01-04")).unwrap();
        snapshots
            .write(
                "ethereum",
                &StoredSnapshot {
                    block: first.block + 4 * 7_200,
                    day: day("2026-01-05"),
                    state: pool.snapshot(),
                    ..first
                },
            )
            .unwrap();

        let looking_back = |days: &str, suffix: &str| {
            let mut run = runs[1].clone();
            run.run_id = format!("{}-{suffix}", run.run_id);
            let zone = run.config.strategies[0].extra.get_mut("zone").unwrap();
            zone["lookbacks"] = serde_yaml::from_str(days).unwrap();
            run
        };
        // One day back starts from the snapshot of 5 January, seven days
        // back from the one of 1 January.
        let short = looking_back("[1d]", "short");
        let long = looking_back("[7d]", "long");
        run(
            &[short, long.clone()],
            &lake,
            &root.join("both"),
            "ethereum",
        )
        .unwrap();
        run(
            std::slice::from_ref(&long),
            &lake,
            &root.join("alone"),
            "ethereum",
        )
        .unwrap();
        assert_eq!(
            tables(&root.join("both"), &long.run_id),
            tables(&root.join("alone"), &long.run_id)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_run_refuses_a_pool_below_the_floors_unless_the_strategy_names_it() {
        let (root, runs, _) = demo("floors", 11);
        let lake = root.join("lake");
        let mut thin = runs[1].clone();
        thin.config.universe.floors = Some(FloorsConfig {
            volume_usd_per_day: 1e15,
            in_range_liquidity_usd: 1.0,
        });
        let error = format!(
            "{:#}",
            run(
                std::slice::from_ref(&thin),
                &lake,
                &root.join("runs"),
                "ethereum"
            )
            .unwrap_err()
        );
        for wanted in [
            "USDC/WETH 0.05",
            "daily volume",
            "universe.floors",
            "lpbot backfill --from 2026-01-01",
        ] {
            assert!(error.contains(wanted), "{wanted}: {error}");
        }

        // Named in the strategy, the pool is traded all the same.
        thin.config.strategies[0]
            .extra
            .insert("pool".into(), Value::String(DEMO_POOL.to_checksum(None)));
        run(&[thin], &lake, &root.join("runs"), "ethereum").unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_daily_volume_is_averaged_over_the_days_actually_stored() {
        let (root, runs, tokens) = demo("days", 11);
        let lake = root.join("lake");
        let config = &runs[1].config;
        let strategy = &config.strategies[0];

        // On 2 January only 1 January is stored before the window: its
        // average day is that one day, not a seventh of it.
        let (_, rows) = choose_pool(
            &lake,
            "ethereum",
            &tokens,
            config,
            strategy,
            day("2026-01-02"),
        )
        .unwrap();
        let ratio = rows[0].volume_per_day_usd / rows[0].volume_24h_usd;
        assert!((ratio - 1.0).abs() < 0.01, "{ratio}");
        assert!(rows[0].excluded.is_none(), "{:?}", rows[0].excluded);

        // A full week averages over seven days.
        let (_, week) = choose_pool(
            &lake,
            "ethereum",
            &tokens,
            config,
            strategy,
            day("2026-01-08"),
        )
        .unwrap();
        let ratio = week[0].volume_per_day_usd / week[0].volume_24h_usd;
        assert!((0.5..2.0).contains(&ratio), "{ratio}");

        // With nothing stored before the window the pool is not ranked,
        // and the run is refused with what to do.
        let error = format!(
            "{:#}",
            choose_pool(
                &lake,
                "ethereum",
                &tokens,
                config,
                strategy,
                day("2026-01-01")
            )
            .unwrap_err()
        );
        assert!(
            error.contains("no history stored")
                && error.contains("lpbot backfill --from 2025-12-25"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_demo_experiment_runs_end_to_end_and_writes_all_six_tables() {
        use lpbot_core::experiment::{resolve_experiment, RunOverrides};
        use lpbot_sim::tables::{read_table, table_path, Column};

        let root = std::env::temp_dir().join(format!("lpbot-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (lake, results) = (root.join("demo"), root.join("runs"));
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

        let runs = resolve_experiment(
            &repo.join("experiments/demo-patience.yml"),
            &RunOverrides::default(),
        )
        .unwrap();
        assert_eq!(runs.len(), 12);
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
    fn walk_forward_windows_never_score_on_what_they_fitted() {
        let day = |text: &str| text.parse::<Day>().unwrap();
        let windows = fold_windows(day("2026-01-08"), day("2026-01-30"), 8, 5);
        let text: Vec<String> = windows
            .iter()
            .map(|(a, b, c)| format!("{a} {b} {c}"))
            .collect();
        assert_eq!(
            text,
            vec![
                "2026-01-08 2026-01-16 2026-01-20",
                "2026-01-13 2026-01-21 2026-01-25",
                "2026-01-18 2026-01-26 2026-01-30",
            ]
        );
        for (fit_from, score_from, score_to) in &windows {
            assert!(fit_from < score_from && score_from <= score_to);
        }
        // Too short for a single fold, and windows of no length.
        assert!(fold_windows(day("2026-01-08"), day("2026-01-19"), 8, 5).is_empty());
        assert!(fold_windows(day("2026-01-08"), day("2026-01-30"), 0, 5).is_empty());
        assert!(fold_windows(day("2026-01-08"), day("2026-01-30"), 8, 0).is_empty());
    }

    #[test]
    fn the_adaptive_decision_ships_only_if_it_beats_every_fixed_rule() {
        let fold = |adaptive: f64, fixed: [f64; 3]| Fold {
            fit_from: Day(0),
            score_from: Day(8),
            score_to: Day(12),
            chosen: AdaptiveParams::default(),
            fit_result: 0.0,
            adaptive,
            fixed,
        };
        // Ahead in total, even though it lost one fold.
        let won = verdict(&[fold(1.0, [0.5, -1.0, -2.0]), fold(0.2, [0.4, 0.0, 0.1])]);
        assert_eq!(won.ships, "adaptive");
        assert!((won.adaptive_total - 1.2).abs() < 1e-12);
        assert_eq!(won.latest, Some(AdaptiveParams::default()));
        // Behind always-close in total: that rule ships.
        let lost = verdict(&[fold(1.0, [0.5, -1.0, 0.9]), fold(0.2, [0.4, 0.0, 0.5])]);
        assert_eq!(lost.ships, "close");
        // A tie is not a win, and no folds prove nothing.
        assert_eq!(verdict(&[fold(1.0, [1.0, 0.0, 0.0])]).ships, "wait");
        assert_eq!(verdict(&[]).ships, "wait");
        assert_eq!(calibration_grid(30).len(), 18);
    }

    #[test]
    fn the_patience_policy_is_a_fixed_rule_or_the_adaptive_settings() {
        let fixed = strategy(&format!("{BASE}patience: {{ mode: flip }}\n"));
        assert_eq!(policy(&fixed).unwrap(), Policy::Fixed(FixedRule::Flip));

        // The base config's `calibrated` margin means the default for now.
        let base = strategy(&format!(
            "{BASE}patience: {{ mode: adaptive, decide_every: 1h, switch_margin: calibrated }}\n"
        ));
        assert_eq!(
            policy(&base).unwrap(),
            Policy::Adaptive(AdaptiveParams::default())
        );
        let set = strategy(&format!(
            "{BASE}patience: {{ mode: adaptive, switch_margin: 0.5, horizon: 6h, reinvest_credit: 0, lookback: 14d }}\n"
        ));
        assert_eq!(
            policy(&set).unwrap(),
            Policy::Adaptive(AdaptiveParams {
                horizon: 6 * 3_600,
                switch_margin: 0.005,
                reinvest_credit: 0.0,
                lookback_days: 14,
            })
        );
        for bad in [
            "switch_margin: tight",
            "switch_margin: -1",
            "reinvest_credit: 2",
            "horizon: soon",
        ] {
            let s = strategy(&format!("{BASE}patience: {{ mode: adaptive, {bad} }}\n"));
            assert!(policy(&s).is_err(), "{bad}");
        }
        assert_eq!(policy_name(&policy(&base).unwrap()), "adaptive");
        assert_eq!(policy_name(&policy(&fixed).unwrap()), "flip");
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
