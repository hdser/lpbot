//! The `lpbot` command line.
//!
//! Working today: `plan` (resolve an experiment file into its runs), `iv`
//! (implied volatility from fee, volume and in-range liquidity), `pools`
//! (list the universe's pools from the node), `verify-pool` (check the
//! replica against a deployed pool), `rank` (rank the pools by implied
//! volatility), `zones` (a pool's zone map), `backfill` and `demo-data` (fill
//! the data folder), `run` (backtest an experiment on the stored history)
//! and `gates` (every check against real chain data, in order, then the
//! backtest, the load and the calibration). `watch-pools` keeps the live
//! tables current from the node, block by block.

mod agent_cmd;
mod backfill_cmd;
mod chain_cmd;
mod demo_cmd;
mod fees_cmd;
mod gas_history_cmd;
mod gates_cmd;
mod load_cmd;
mod lp_positions_cmd;
mod pool_stats_cmd;
mod rank_cmd;
mod run_cmd;
mod watch_cmd;
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
    /// Fit the adaptive patience decision walk-forward and score it
    /// against the fixed rules.
    Calibrate(CalibrateArgs),
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
    /// Once the node is synced: every check against real chain data, then
    /// the backtest, the load and the calibration. Stops at the first
    /// step that fails.
    Gates(GatesArgs),
    /// Write a simulated pool history, to try the pipeline without a node.
    DemoData(DemoArgs),
    /// Show a pool's zone map: how often price crosses each candidate range.
    Zones(ZonesArgs),
    /// Sample the stored pools: price, liquidity, value locked, volume and
    /// fees over time, and the liquidity around the price, as the two pool
    /// tables for the pool dashboard.
    PoolStats(PoolStatsArgs),
    /// A liquidity provider's positions on the stored pools, from the
    /// node's position manager records, one row per stretch with liquidity
    /// in place: what a backtest can follow (`strategy.follow`).
    LpPositions(LpPositionsCli),
    /// Store each block's base fee and median priority fee from the node's
    /// fee history, so backtests charge gas at the price of the block an
    /// action lands in.
    GasHistory(GasHistoryArgs),
    /// Decide a run look by look, as a person or a session would: `next`
    /// shows the pool at the first look without a decision, `decide`
    /// writes one. The experiment's strategy names the decisions file
    /// (`agent.decisions`).
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Follow the stored pools live from the node, block by block, and keep
    /// the live tables in Postgres current: price, liquidity, swaps, minute
    /// rows and every position. Runs until stopped.
    WatchPools(WatchPoolsArgs),
}

#[derive(Args)]
struct WatchPoolsArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// Pool to follow, by address; repeat for several. Default: every
    /// stored pool.
    #[arg(long = "pool")]
    pools: Vec<String>,
    /// Data folder with the stored history. Defaults to LPBOT_DATA_DIR,
    /// then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Database, as postgres://user:password@host:port/name. Defaults to
    /// LPBOT_DB_URL.
    #[arg(long)]
    db: Option<String>,
    /// Follow positions from this day, as YYYY-MM-DD: the replay starts at
    /// the newest stored snapshot on or before it. Default: the earliest
    /// snapshot, which sees the most positions and takes the longest.
    #[arg(long)]
    from: Option<String>,
    /// The checkpoint file. Default: <data dir>/live/checkpoint-<chain>.json.
    #[arg(long)]
    state: Option<PathBuf>,
    /// Leave the checkpoint aside and replay the stored history again.
    #[arg(long)]
    rebuild: bool,
    /// How often to ask the node for a new block.
    #[arg(long, default_value = "2s")]
    poll: String,
    /// The shortest time between two writes to the database.
    #[arg(long, default_value = "10s")]
    every: String,
    /// How often positions in range are written again, for their fees.
    #[arg(long, default_value = "1m")]
    refresh: String,
    /// How often the finalized state is saved as the checkpoint.
    #[arg(long, default_value = "10m")]
    checkpoint_every: String,
    /// How long swaps and minute rows stay in the tables.
    #[arg(long, default_value = "7d")]
    keep: String,
    /// How long closed positions stay in the table.
    #[arg(long, default_value = "90d")]
    keep_closed: String,
    /// Connections that read positions from the node at once.
    #[arg(long, default_value_t = 4)]
    threads: usize,
    /// Stop once at the head, after one write: for a try or a catch-up.
    #[arg(long)]
    once: bool,
}

#[derive(Args)]
struct GatesArgs {
    #[command(flatten)]
    experiment: ExperimentArgs,
    /// Which chain of the config to use.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Node endpoint. Defaults to LPBOT_RPC, then the config's local endpoint.
    #[arg(long)]
    rpc: Option<String>,
    /// Data folder for the pool history. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Folder the result tables are written to.
    #[arg(long, default_value = "runs")]
    results_dir: PathBuf,
    /// Database to load the results into, as postgres://user:password@host:port/name.
    /// Defaults to LPBOT_DB_URL; without one the load is left out.
    #[arg(long)]
    db: Option<String>,
    /// Step to start at: node, pools, replica, history, fees, backtest,
    /// load or calibrate. The steps before it are left out.
    #[arg(long)]
    start_at: Option<String>,
    /// Steps to leave out, separated by commas.
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
    /// First day of history to store, as YYYY-MM-DD. Defaults to the longest
    /// zone lookback before the experiment's window.
    #[arg(long)]
    history_from: Option<String>,
    /// Blocks the replica check replays for every pool.
    #[arg(long, default_value_t = 2_000)]
    blocks: u64,
    /// Blocks per log request. Shrinks by itself when the node refuses.
    #[arg(long, default_value_t = 2_000)]
    log_chunk: u64,
    /// Emptied and fully collected positions the fee check needs to pass.
    #[arg(long, default_value_t = 20)]
    positions: usize,
    /// Days to fit on. Defaults to calibration.fit_window in the config.
    #[arg(long)]
    fit: Option<u32>,
    /// Days to score on. Defaults to calibration.score_window in the config.
    #[arg(long)]
    score: Option<u32>,
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
    /// The node that says which positions started empty and what the pool
    /// still owes them; an archive node for a history older than its state.
    #[command(flatten)]
    chain: ChainArgs,
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
struct PoolStatsArgs {
    /// Pool to sample, by address; repeat for several. Default: every
    /// stored pool.
    #[arg(long = "pool")]
    pools: Vec<String>,
    #[command(flatten)]
    chain: ChainArgs,
    /// First day to sample, as YYYY-MM-DD. Default: the first stored day.
    #[arg(long)]
    from: Option<String>,
    /// Time between rows of the series, such as 1h.
    #[arg(long, default_value = "1h")]
    every: String,
    /// Time between liquidity distributions, such as 1d.
    #[arg(long, default_value = "1d")]
    liquidity_every: String,
    /// How far the distribution reaches on each side of the price, in
    /// percent. Stablecoin pairs use at most 0.5.
    #[arg(long, default_value_t = 20.0)]
    span_pct: f64,
    /// Width of a distribution bucket, in percent of the price, widened to
    /// the pool's tick spacing. Stablecoin pairs use at most 0.01.
    #[arg(long, default_value_t = 0.5)]
    bucket_pct: f64,
    /// Data folder with the stored history. Defaults to LPBOT_DATA_DIR,
    /// then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Folder the pool tables are written to, beside the run tables.
    #[arg(long, default_value = "runs")]
    results_dir: PathBuf,
}

#[derive(Args)]
struct LpPositionsCli {
    /// A position's NFT id, such as one seen on revert.finance; its owner's
    /// positions are read. Repeat for several.
    #[arg(long = "nft")]
    nfts: Vec<String>,
    /// An owner's address. Repeat for several.
    #[arg(long = "account")]
    accounts: Vec<String>,
    #[command(flatten)]
    chain: ChainArgs,
    /// First day to read, as YYYY-MM-DD.
    #[arg(long)]
    from: String,
    /// Data folder with the stored pools. Defaults to LPBOT_DATA_DIR, then
    /// data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// File the positions are written to, as CSV.
    #[arg(long, default_value = "data/follow/positions.csv")]
    out: PathBuf,
    /// Blocks per log request; the node refuses more than 100,000.
    #[arg(long, default_value_t = 100_000)]
    log_chunk: u64,
}

#[derive(Subcommand)]
enum AgentCommand {
    /// Replay the run to its first look without a decision and show the
    /// pool there.
    Next(AgentArgs),
    /// Write the decision for the look `next` showed, then show the next.
    Decide(DecideArgs),
}

#[derive(Args)]
struct AgentArgs {
    #[command(flatten)]
    experiment: ExperimentArgs,
    /// Which of the experiment's runs, by index, when it has several.
    #[arg(long)]
    run: Option<usize>,
    /// Which chain of the config to use.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Data folder for the pool history. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Args)]
struct DecideArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// stay_out, hold, exit or enter.
    #[arg(long)]
    action: String,
    /// For an entry: the range's lower price, in percent from now.
    #[arg(long, allow_hyphen_values = true)]
    lower: Option<f64>,
    /// For an entry: the range's upper price, in percent from now.
    #[arg(long, allow_hyphen_values = true)]
    upper: Option<f64>,
    /// When to look again, such as 30m, 4h or 2d.
    #[arg(long)]
    next: String,
    /// Why, in a sentence.
    #[arg(long, default_value = "")]
    note: String,
}

#[derive(Args)]
struct GasHistoryArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// First day to store, as YYYY-MM-DD.
    #[arg(long)]
    from: String,
    /// Data folder. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Requests sent at the same time.
    #[arg(long, default_value_t = 2)]
    threads: usize,
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

#[derive(Args)]
struct CalibrateArgs {
    #[command(flatten)]
    run: RunArgs,
    /// Days to fit on. Defaults to calibration.fit_window in the config.
    #[arg(long)]
    fit: Option<u32>,
    /// Days to score on. Defaults to calibration.score_window in the config.
    #[arg(long)]
    score: Option<u32>,
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
        Command::Calibrate(args) => {
            let runs = resolve(&args.run.experiment)?;
            run_cmd::calibrate(
                &runs,
                &backfill_cmd::data_dir(args.run.data_dir.as_deref()),
                &args.run.results_dir,
                &args.run.chain,
                args.fit,
                args.score,
            )
            .map(|_| ())
        }
        Command::Gates(args) => {
            let runs = resolve(&args.experiment)?;
            print_runs(&runs, args.experiment.show_config);
            println!();
            let history_from = args
                .history_from
                .as_deref()
                .map(|text| text.parse().map_err(anyhow::Error::msg))
                .transpose()?;
            let gates = gates_cmd::Gates::new(
                runs,
                gates_cmd::Options {
                    chain_name: args.chain,
                    rpc: args.rpc,
                    root: backfill_cmd::data_dir(args.data_dir.as_deref()),
                    results: args.results_dir,
                    db: load_cmd::database(args.db),
                    history_from,
                    blocks: args.blocks,
                    log_chunk: args.log_chunk,
                    positions: args.positions,
                    fit_days: args.fit,
                    score_days: args.score,
                },
            )?;
            let plan = gates_cmd::Plan {
                start_at: args.start_at,
                skip: args.skip,
            };
            gates_cmd::run(gates, &plan)
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
            let url = load_cmd::database(args.db).ok_or_else(|| {
                anyhow::anyhow!(
                    "no database given: pass --db or set LPBOT_DB_URL \
                     (`make load` takes it from deploy/.env)"
                )
            })?;
            let report = load_cmd::run(&url, &args.results_dir)?;
            println!(
                "loaded {} run(s) and {} pool(s), {} rows, from {}",
                report.runs,
                report.pools,
                report.rows,
                args.results_dir.display()
            );
            Ok(())
        }
        Command::VerifyFees(args) => {
            let chain = chain_cmd::connect(
                &args.chain.config,
                &args.chain.chain,
                args.chain.rpc.as_deref(),
            )?;
            let summary = fees_cmd::run_with(
                &backfill_cmd::data_dir(args.data_dir.as_deref()),
                &args.chain.chain,
                chain_cmd::checked_address("pool", &args.pool)?,
                args.positions,
                Some(&chain.client),
            )?;
            if summary.passed(args.positions) {
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
            .map(|_| ())
        }
        Command::LpPositions(args) => {
            let chain = chain_cmd::connect(
                &args.chain.config,
                &args.chain.chain,
                args.chain.rpc.as_deref(),
            )?;
            let config = chain_cmd::load_base_config(&args.chain.config)?;
            let nfts = args
                .nfts
                .iter()
                .map(|text| {
                    text.parse::<alloy_primitives::U256>()
                        .map_err(|_| anyhow::anyhow!("--nft {text}: give the NFT id as a number"))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let accounts = args
                .accounts
                .iter()
                .map(|a| chain_cmd::checked_address("account", a))
                .collect::<anyhow::Result<Vec<_>>>()?;
            if nfts.is_empty() && accounts.is_empty() {
                anyhow::bail!("say whose positions to read: --nft <id> or --account <address>");
            }
            let from = args
                .from
                .parse::<lpbot_data::day::Day>()
                .map_err(|e| anyhow::anyhow!("--from {}: {e}", args.from))?;
            let root = backfill_cmd::data_dir(args.data_dir.as_deref());
            let written = lp_positions_cmd::run(
                &config,
                &chain.client,
                &lp_positions_cmd::LpPositionsArgs {
                    nfts: &nfts,
                    accounts: &accounts,
                    from,
                    root: &root,
                    out: &args.out,
                    chain: &args.chain.chain,
                    log_chunk: args.log_chunk,
                },
            )?;
            println!("{written} stretch(es) written to {}", args.out.display());
            Ok(())
        }
        Command::GasHistory(args) => {
            let chain = chain_cmd::connect(
                &args.chain.config,
                &args.chain.chain,
                args.chain.rpc.as_deref(),
            )?;
            let from = args
                .from
                .parse::<lpbot_data::day::Day>()
                .map_err(|e| anyhow::anyhow!("--from {}: {e}", args.from))?;
            let client = &chain.client;
            let source = lpbot_data::backfill::NodeSource {
                client,
                head_margin: 0,
                log_chunk: 2_000,
            };
            // Up to the newest block no reorg can change.
            let to = match client.finalized_block()? {
                Some((block, _)) => block,
                None => client.block_number()?.saturating_sub(64),
            };
            let first =
                lpbot_data::backfill::first_block_at_or_after(&source, from.start(), 1, to)?;
            let root = backfill_cmd::data_dir(args.data_dir.as_deref());
            println!(
                "  gas prices of blocks {first} to {to} into {}",
                root.display()
            );
            let done =
                gas_history_cmd::run(client, &root, &args.chain.chain, first, to, args.threads)?;
            let stored =
                lpbot_data::gas::GasStore::new(&root).read(&args.chain.chain, first, to)?;
            let average = stored
                .as_ref()
                .and_then(|fees| fees.summary())
                .map_or(String::new(), |(_, wei)| {
                    format!(", {:.3} gwei a block on average", wei / 1e9)
                });
            println!(
                "{} chunk(s) read from the node, {} already stored{average}",
                done.chunks, done.kept
            );
            Ok(())
        }
        Command::Agent(command) => {
            let args = match &command {
                AgentCommand::Next(args) => args,
                AgentCommand::Decide(args) => &args.agent,
            };
            let runs = resolve(&args.experiment)?;
            let run = match (args.run, runs.len()) {
                (Some(i), _) => runs.get(i).ok_or_else(|| {
                    anyhow::anyhow!("--run {i}: the experiment has {} run(s)", runs.len())
                })?,
                (None, 1) => &runs[0],
                (None, n) => bail!("the experiment has {n} runs; say which with --run <index>"),
            };
            let root = backfill_cmd::data_dir(args.data_dir.as_deref());
            match &command {
                AgentCommand::Next(_) => agent_cmd::next(run, &root, &args.chain).map(|_| ()),
                AgentCommand::Decide(d) => agent_cmd::decide(
                    run,
                    &root,
                    &args.chain,
                    &agent_cmd::Decision {
                        action: &d.action,
                        lower_pct: d.lower,
                        upper_pct: d.upper,
                        next: &d.next,
                        note: &d.note,
                    },
                ),
            }
        }
        Command::PoolStats(args) => {
            let config = chain_cmd::load_base_config(&args.chain.config)?;
            let pools = args
                .pools
                .iter()
                .map(|p| chain_cmd::checked_address("pool", p))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let from = args
                .from
                .as_deref()
                .map(|text| {
                    text.parse::<lpbot_data::day::Day>()
                        .map_err(|e| anyhow::anyhow!("--from {text}: {e}"))
                })
                .transpose()?;
            let every = |text: &str, flag: &str| {
                run_cmd::parse_duration(text)
                    .filter(|s| *s > 0)
                    .ok_or_else(|| {
                        anyhow::anyhow!("--{flag} {text}: give a duration such as 1h or 1d")
                    })
            };
            let root = backfill_cmd::data_dir(args.data_dir.as_deref());
            let written = pool_stats_cmd::run(
                &config,
                &pool_stats_cmd::PoolStatsArgs {
                    pools: &pools,
                    from,
                    every: every(&args.every, "every")?,
                    liquidity_every: every(&args.liquidity_every, "liquidity-every")?,
                    span_pct: args.span_pct,
                    bucket_pct: args.bucket_pct,
                    root: &root,
                    results: &args.results_dir,
                    chain: &args.chain.chain,
                },
            )?;
            println!(
                "{written} pool(s) written to {}; `lpbot load-results` puts them in the viewer",
                args.results_dir.display()
            );
            Ok(())
        }
        Command::WatchPools(args) => {
            let config = chain_cmd::load_base_config(&args.chain.config)?;
            let chain =
                chain_cmd::connect_to(&config, &args.chain.chain, args.chain.rpc.as_deref())?;
            let pools = args
                .pools
                .iter()
                .map(|p| chain_cmd::checked_address("pool", p))
                .collect::<Result<Vec<_>>>()?;
            let from = args
                .from
                .as_deref()
                .map(|text| {
                    text.parse::<lpbot_data::day::Day>()
                        .map_err(|e| anyhow::anyhow!("--from {text}: {e}"))
                })
                .transpose()?;
            let seconds = |text: &str, flag: &str| {
                run_cmd::parse_duration(text)
                    .filter(|s| *s > 0)
                    .ok_or_else(|| {
                        anyhow::anyhow!("--{flag} {text}: give a duration such as 10s, 5m or 7d")
                    })
            };
            let duration =
                |text: &str, flag: &str| seconds(text, flag).map(std::time::Duration::from_secs);
            let db = load_cmd::database(args.db).ok_or_else(|| {
                anyhow::anyhow!(
                    "no database given: pass --db or set LPBOT_DB_URL \
                     (`make watch` takes it from deploy/.env)"
                )
            })?;
            let lake = backfill_cmd::data_dir(args.data_dir.as_deref());
            let state_file = args.state.unwrap_or_else(|| {
                lake.join("live")
                    .join(format!("checkpoint-{}.json", args.chain.chain))
            });
            watch_cmd::run(
                &config,
                &chain,
                &watch_cmd::WatchArgs {
                    lake: &lake,
                    db_url: &db,
                    pools: &pools,
                    from,
                    state_file,
                    rebuild: args.rebuild,
                    poll: duration(&args.poll, "poll")?,
                    every: duration(&args.every, "every")?,
                    refresh: duration(&args.refresh, "refresh")?,
                    checkpoint_every: duration(&args.checkpoint_every, "checkpoint-every")?,
                    keep: seconds(&args.keep, "keep")?,
                    keep_closed: seconds(&args.keep_closed, "keep-closed")?,
                    threads: args.threads.max(1),
                    once: args.once,
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
