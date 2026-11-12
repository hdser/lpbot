//! The `lpbot` command line.
//!
//! Working today: `plan` (resolve an experiment file into its runs), `iv`
//! (implied volatility from fee, volume and in-range liquidity), `pools`
//! (list the universe's pools from the node), `verify-pool` (check the
//! replica against a deployed pool), `rank` (rank the pools by implied
//! volatility), `zones` (a pool's zone map), `backfill` and `demo-data` (fill
//! the data folder) and `run` (backtest an experiment on the stored history).

mod backfill_cmd;
mod chain_cmd;
mod demo_cmd;
mod fees_cmd;
mod rank_cmd;
mod run_cmd;
mod zones_cmd;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use lpbot_core::metrics::{implied_vol_annual, l_sqrt_p_from_depth_1pct};
use lpbot_core::{resolve_experiment, Autonomy, ResolvedRun, RunOverrides};

#[derive(Parser)]
#[command(
    name = "lpbot",
    version,
    about = "Autonomous Uniswap v3 liquidity-provision bot"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Resolve an experiment file and list the runs it expands into.
    Plan(ExperimentArgs),
    /// Run an experiment file (backtest, paper or live, as the file says).
    Run(RunArgs),
    /// Implied volatility from fee tier, daily volume and in-range liquidity.
    Iv(IvArgs),
    /// List every Uniswap v3 pool whose two tokens are both in the universe.
    Pools(ChainArgs),
    /// Replay a pool's recent events and compare the replica with the chain.
    VerifyPool(VerifyArgs),
    /// Rank the universe's pools by implied volatility.
    Rank(RankArgs),
    /// Store pool history from the node in the local data folder.
    Backfill(BackfillArgs),
    /// Compare the replica's fees with what real positions collected.
    VerifyFees(VerifyFeesArgs),
    /// Load the result tables into Postgres, where the viewer reads them.
    LoadResults(LoadArgs),
    /// Write a simulated pool history, to try the pipeline without a node.
    DemoData(DemoArgs),
    /// Show a pool's zone map: how often price crosses each candidate range.
    Zones(ZonesArgs),
}

#[derive(Args)]
struct LoadArgs {
    /// Folder the result tables were written to.
    #[arg(long, default_value = "runs")]
    results_dir: PathBuf,
    /// Database to load into, as postgres://user:password@host:port/name.
    /// Defaults to LPBOT_DB_URL.
    #[arg(long)]
    db: Option<String>,
}

#[derive(Args)]
struct VerifyFeesArgs {
    /// Address of a pool with stored history.
    pool: String,
    /// Which chain the stored history belongs to.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Data folder. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Emptied and fully collected positions needed to pass.
    #[arg(long, default_value_t = 20)]
    positions: usize,
}

#[derive(Args)]
struct DemoArgs {
    /// Base config; the demo pool uses its USDC and WETH.
    #[arg(long, default_value = "config/mainnet.yml")]
    config: PathBuf,
    /// Which chain of the config to use.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Folder for the demo data. Keep it apart from real history.
    #[arg(long, default_value = "data/demo")]
    data_dir: PathBuf,
    /// Days of history to simulate.
    #[arg(long, default_value_t = 30)]
    days: u32,
    /// The same seed always gives the same history.
    #[arg(long, default_value_t = 1)]
    seed: u64,
}

#[derive(Args)]
struct BackfillArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// Pool to backfill. Repeat for several; none means the whole universe.
    #[arg(long = "pool")]
    pools: Vec<String>,
    /// First day to store, as YYYY-MM-DD. Overrides --days.
    #[arg(long)]
    from: Option<String>,
    /// Days of history to store, counted back from today.
    #[arg(long, default_value_t = 90)]
    days: u32,
    /// Data folder. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Blocks per log request. Shrinks by itself when the node refuses.
    #[arg(long, default_value_t = 2_000)]
    log_chunk: u64,
    /// After storing, replay the stored history and compare with the chain.
    #[arg(long)]
    verify: bool,
}

#[derive(Args)]
struct ZonesArgs {
    /// Address of the pool.
    pool: String,
    #[command(flatten)]
    chain: ChainArgs,
    /// Lookbacks in days.
    #[arg(long, value_delimiter = ',', default_value = "7,30,90")]
    days: Vec<u32>,
    /// Range widths to try, as multiples of the pool's tick spacing.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8,16")]
    widths: Vec<u32>,
    /// Ranges to show per lookback.
    #[arg(long, default_value_t = 15)]
    top: usize,
    /// Read the node instead of the data folder.
    #[arg(long)]
    node: bool,
    /// Data folder. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Blocks per log request. Shrinks by itself when the node refuses.
    #[arg(long, default_value_t = 2_000)]
    log_chunk: u64,
}

#[derive(Args)]
struct RankArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// Length of the volume window in days.
    #[arg(long, default_value_t = 7)]
    days: u64,
    /// Read the node instead of the data folder.
    #[arg(long)]
    node: bool,
    /// Data folder. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Blocks per log request. Shrinks by itself when the node refuses.
    #[arg(long, default_value_t = 2_000)]
    log_chunk: u64,
}

#[derive(Args)]
struct ChainArgs {
    /// Base config with the chain's endpoints and addresses.
    #[arg(long, default_value = "config/mainnet.yml")]
    config: PathBuf,
    /// Which chain of the config to use.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Node endpoint. Defaults to LPBOT_RPC, then the config's local endpoint.
    #[arg(long)]
    rpc: Option<String>,
}

#[derive(Args)]
struct VerifyArgs {
    /// Address of the pool.
    pool: String,
    #[command(flatten)]
    chain: ChainArgs,
    /// First block of the range. Defaults to `--blocks` before the end.
    #[arg(long)]
    from_block: Option<u64>,
    /// Last block of the range. Defaults to just behind the head.
    #[arg(long)]
    to_block: Option<u64>,
    /// Length of the range when no start block is given.
    #[arg(long, default_value_t = 2_000)]
    blocks: u64,
    /// Blocks per log request. Shrinks by itself when the node refuses.
    #[arg(long, default_value_t = 2_000)]
    log_chunk: u64,
}

#[derive(Args)]
struct ExperimentArgs {
    /// Path of the experiment YAML file.
    experiment: PathBuf,
    /// Capital the run may deploy, in USD. Overrides the files.
    #[arg(long)]
    capital: Option<f64>,
    /// Most the run may lose, in percent of the value token. Overrides the files.
    #[arg(long = "loss-limit")]
    loss_limit: Option<f64>,
    /// How much the bot does without asking. Overrides the files.
    #[arg(long, value_enum)]
    autonomy: Option<AutonomyArg>,
    /// Print the fully resolved config of each run.
    #[arg(long)]
    show_config: bool,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    experiment: ExperimentArgs,
    /// Which chain of the config the stored history belongs to.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Data folder with the stored history. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Folder the result tables are written to.
    #[arg(long, default_value = "runs")]
    results_dir: PathBuf,
}

#[derive(Clone, Copy, ValueEnum)]
enum AutonomyArg {
    Auto,
    Approve,
}

#[derive(Args)]
struct IvArgs {
    /// Fee tier in percent, for example 0.05.
    #[arg(long = "fee-pct")]
    fee_pct: f64,
    /// Traded volume per day, in the quote token.
    #[arg(long)]
    volume: f64,
    /// Value sitting within 1% of the price, in the quote token.
    #[arg(long = "depth-1pct")]
    depth_1pct: f64,
}

fn main() -> ExitCode {
    match real_main() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<()> {
    match Cli::parse().command {
        Command::Plan(args) => {
            let runs = resolve(&args)?;
            print_runs(&runs, args.show_config);
            Ok(())
        }
        Command::Run(args) => {
            let runs = resolve(&args.experiment)?;
            print_runs(&runs, args.experiment.show_config);
            println!();
            run_cmd::run(
                &runs,
                &backfill_cmd::data_dir(args.data_dir.as_deref()),
                &args.results_dir,
                &args.chain,
            )
        }
        Command::Pools(args) => {
            let chain = chain_cmd::connect(&args.config, &args.chain, args.rpc.as_deref())?;
            chain_cmd::pools(&chain)
        }
        Command::VerifyPool(args) => {
            let chain = chain_cmd::connect(
                &args.chain.config,
                &args.chain.chain,
                args.chain.rpc.as_deref(),
            )?;
            let passed = chain_cmd::verify(
                &chain,
                &args.pool,
                args.from_block,
                args.to_block,
                args.blocks,
                args.log_chunk,
            )?;
            if passed {
                Ok(())
            } else {
                bail!("the replica does not match the chain")
            }
        }
        Command::Rank(args) => {
            let config = chain_cmd::load_base_config(&args.chain.config)?;
            if args.node {
                let chain = chain_cmd::connect(
                    &args.chain.config,
                    &args.chain.chain,
                    args.chain.rpc.as_deref(),
                )?;
                rank_cmd::run(&chain, &config, args.days, args.log_chunk)
            } else {
                let (_, tokens) = chain_cmd::chain_addresses(&config, &args.chain.chain)?;
                rank_cmd::run_local(
                    &backfill_cmd::data_dir(args.data_dir.as_deref()),
                    &args.chain.chain,
                    &tokens,
                    &config,
                    args.days,
                )
            }
        }
        Command::LoadResults(args) => {
            let url = args
                .db
                .or_else(|| std::env::var("LPBOT_DB_URL").ok())
                .filter(|url| !url.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no database given: pass --db or set LPBOT_DB_URL \
                         (`make load` takes it from deploy/.env)"
                    )
                })?;
            // The URL holds the password, so it is never printed.
            let mut client = postgres::Client::connect(&url, postgres::NoTls)
                .map_err(|e| anyhow::anyhow!("cannot connect to the database: {e}"))?;
            let report = lpbot_sim::loader::load_results(&mut client, &args.results_dir)?;
            if report.runs == 0 {
                bail!(
                    "no runs found under {}; run `lpbot run <experiment.yml>` first",
                    args.results_dir.display()
                );
            }
            println!(
                "loaded {} run(s), {} rows, from {}",
                report.runs,
                report.rows,
                args.results_dir.display()
            );
            Ok(())
        }
        Command::VerifyFees(args) => {
            let passed = fees_cmd::run(
                &backfill_cmd::data_dir(args.data_dir.as_deref()),
                &args.chain,
                chain_cmd::checked_address("pool", &args.pool)?,
                args.positions,
            )?;
            if passed {
                Ok(())
            } else {
                bail!("the fee check did not pass")
            }
        }
        Command::DemoData(args) => {
            let config = chain_cmd::load_base_config(&args.config)?;
            let (_, tokens) = chain_cmd::chain_addresses(&config, &args.chain)?;
            demo_cmd::run(&args.data_dir, &args.chain, &tokens, args.days, args.seed)
        }
        Command::Backfill(args) => {
            let chain = chain_cmd::connect(
                &args.chain.config,
                &args.chain.chain,
                args.chain.rpc.as_deref(),
            )?;
            let pools = args
                .pools
                .iter()
                .map(|p| chain_cmd::checked_address("pool", p))
                .collect::<Result<Vec<_>>>()?;
            let from = args
                .from
                .as_deref()
                .map(|text| text.parse().map_err(anyhow::Error::msg))
                .transpose()?;
            backfill_cmd::run(
                &chain,
                &backfill_cmd::BackfillArgs {
                    pools: &pools,
                    from,
                    days: args.days,
                    root: &backfill_cmd::data_dir(args.data_dir.as_deref()),
                    log_chunk: args.log_chunk,
                    verify: args.verify,
                },
            )
        }
        Command::Zones(args) => {
            let config = chain_cmd::load_base_config(&args.chain.config)?;
            let zone_args = zones_cmd::ZonesArgs {
                pool: chain_cmd::checked_address("pool", &args.pool)?,
                lookbacks: &args.days,
                width_steps: &args.widths,
                top: args.top,
                log_chunk: args.log_chunk,
            };
            let input = if args.node {
                let chain = chain_cmd::connect(
                    &args.chain.config,
                    &args.chain.chain,
                    args.chain.rpc.as_deref(),
                )?;
                zones_cmd::load_from_node(&chain, &config, &zone_args)?
            } else {
                let (_, tokens) = chain_cmd::chain_addresses(&config, &args.chain.chain)?;
                zones_cmd::load_local(
                    &backfill_cmd::data_dir(args.data_dir.as_deref()),
                    &args.chain.chain,
                    &tokens,
                    &config,
                    &zone_args,
                )?
            };
            zones_cmd::print(&input, &zone_args)
        }
        Command::Iv(args) => {
            let l_sqrt_p = l_sqrt_p_from_depth_1pct(args.depth_1pct);
            match implied_vol_annual(args.fee_pct / 100.0, args.volume, l_sqrt_p) {
                Some(iv) => {
                    println!("implied volatility: {:.1}% a year", iv * 100.0);
                    Ok(())
                }
                None => bail!("fee, volume and depth must all be positive numbers"),
            }
        }
    }
}

fn resolve(args: &ExperimentArgs) -> Result<Vec<ResolvedRun>> {
    let overrides = RunOverrides {
        capital_usd: args.capital,
        loss_limit_pct: args.loss_limit,
        autonomy: args.autonomy.map(|a| match a {
            AutonomyArg::Auto => Autonomy::Auto,
            AutonomyArg::Approve => Autonomy::Approve,
        }),
    };
    resolve_experiment(&args.experiment, &overrides)
}

fn print_runs(runs: &[ResolvedRun], show_config: bool) {
    let Some(first) = runs.first() else {
        println!("no runs");
        return;
    };
    let run = &first.config.run;
    println!("experiment: {}", first.experiment);
    println!("mode:       {:?}", first.config.mode);
    if let Some(w) = &first.config.window {
        println!("window:     {} to {}", w.from, w.to);
    }
    println!(
        "run inputs: capital {} USD, loss limit {}%, autonomy {:?}",
        run.capital_usd.unwrap_or_default(),
        run.loss_limit_pct.unwrap_or_default(),
        run.autonomy
    );
    println!("runs:       {}", runs.len());
    println!();
    for r in runs {
        let point: Vec<String> = r
            .sweep_point
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        println!("{}  {}", r.run_id, point.join("  "));
        if show_config {
            println!("---\n{}", r.config_yaml);
        }
    }
}
