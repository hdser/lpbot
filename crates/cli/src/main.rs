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

mod agent_board;
mod agent_brief;
mod agent_cmd;
mod agent_eval;
mod agent_lock;
mod agent_log;
mod agent_peers;
mod agent_rows;
mod agent_runner;
mod backfill_cmd;
mod chain_cmd;
mod codex_worker;
mod demo_cmd;
mod fees_cmd;
mod gas_history_cmd;
mod gates_cmd;
mod load_cmd;
mod load_looks_cmd;
mod lp_positions_cmd;
mod paper_cmd;
mod pool_stats_cmd;
mod rank_cmd;
mod run_cmd;
mod select_cmd;
mod select_ops;
mod supervisor_list;
mod supervisor_procs;
mod supervisor_status;
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
    /// Run the isolated Codex forward-panel coordinator on this host.
    AgentRunner(agent_runner::Args),
    #[command(hide = true)]
    RunnerJob { spec: PathBuf },
    #[command(hide = true)]
    RunnerModelJob { spec: PathBuf },
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
    /// Load each agent's decided looks, with their reasoning, from its log
    /// into Postgres at once, without replays, for the agents dashboard.
    /// Safe to run every minute: runs whose looks are loaded are left alone.
    /// Results and scores still need `lpbot run` and `lpbot load-results`.
    LoadLooks(LoadLooksArgs),
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
    /// Watch the agents: what every forward agent is doing now and whether
    /// anything will wake it for its next look.
    #[command(subcommand)]
    Supervisor(SupervisorCommand),
    /// Choose what trades: score a rule's settings walk-forward, and hold
    /// forward runs, rules and agents alike, against the gates to real
    /// positions.
    #[command(subcommand)]
    Select(SelectCommand),
    /// Follow a forward experiment at the chain's head with virtual
    /// positions: the same engine as a backtest, fed each block as it comes,
    /// checked against a replay of the same blocks. Runs until stopped.
    Paper(PaperArgs),
    /// Hold new entries: set the pause flag. Open positions stay under their
    /// own rules.
    Pause(PauseArgs),
    /// Take every position out and hold new entries: the operator's exit.
    ExitAll(PauseArgs),
    /// Clear the pause flag.
    Resume(ResumeArgs),
    /// The keeper's signer: opens the keystore with a password typed here,
    /// then signs, over a local socket, only calls to the roles module.
    Signer(SignerArgs),
}

#[derive(Subcommand)]
enum SelectCommand {
    /// Over the runs of a results folder: each period trades the setting
    /// that made most over the periods before it, and is scored on what
    /// that setting then made, against cash, the average setting and the
    /// best one chosen with hindsight.
    WalkForward(WalkForwardArgs),
    /// Hold every forward run of a ladder file against its gates and share
    /// each track's capital among those that pass. Writes the plan; sends
    /// nothing.
    Ladder(LadderArgs),
    /// Every experiment file, rule or agent panel: forward or practice, its
    /// agents, and with --plan where each of its runs stands.
    List(ListArgs),
    /// Record the owner's yes to a plan `ladder` wrote, with its SHA-256:
    /// the trader takes only an approved plan whose bytes are unchanged.
    /// The owner's step.
    Approve(ApproveArgs),
    /// Whether the trader would take a plan: approved, unchanged since, and
    /// recent enough.
    Check(CheckArgs),
}

#[derive(Args)]
struct CheckArgs {
    /// The plan, such as data/selection/dollars.json.
    plan: PathBuf,
    /// The oldest plan the trader takes, as a duration.
    #[arg(long, default_value = "36h")]
    max_age: String,
}

#[derive(Args)]
struct ListArgs {
    /// Folder of experiment files.
    #[arg(long, default_value = "experiments")]
    experiments: PathBuf,
    /// A plan `lpbot select ladder` wrote, such as data/selection/dollars.json.
    #[arg(long)]
    plan: Option<PathBuf>,
}

#[derive(Args)]
struct ApproveArgs {
    /// The plan, such as data/selection/dollars.json.
    plan: PathBuf,
    /// Who approves, for the record.
    #[arg(long)]
    by: String,
}

#[derive(Args)]
struct WalkForwardArgs {
    /// Folder the runs were written to.
    #[arg(long, default_value = "runs")]
    results_dir: PathBuf,
    /// Only experiments whose name starts with this.
    #[arg(long)]
    experiment: Option<String>,
    /// Length of a period, such as 30d or 7d.
    #[arg(long, default_value = "30d")]
    fold: String,
    /// What a pick is fitted on: numbers of periods before it, or `all`
    /// for every period so far, separated by commas.
    #[arg(long, default_value = "3,6,all")]
    fit: String,
    /// Periods `all` needs before its first pick.
    #[arg(long, default_value_t = 3)]
    min_fit: usize,
    /// Choose among the values of a swept setting, such as `regime`, each
    /// judged on the average of the runs that share it, instead of among
    /// single runs.
    #[arg(long)]
    by: Option<String>,
    /// Compare over every pool at once instead of pool by pool. Use it
    /// with --experiment so only one value token is in.
    #[arg(long)]
    across_pools: bool,
}

#[derive(Args)]
struct LadderArgs {
    /// The ladder file, such as selection/ladder.example.yml.
    file: PathBuf,
    /// Folder the plan is written to, as <ladder>.json.
    #[arg(long, default_value = "data/selection")]
    out: PathBuf,
}

#[derive(Subcommand)]
enum SupervisorCommand {
    /// Every forward agent on this machine: its state, its next look,
    /// what waits for it and what schedules it. `--load` writes the
    /// agent_status table for the agents dashboard.
    Status(supervisor_status::Args),
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
struct LoadLooksArgs {
    #[command(flatten)]
    experiment: ExperimentArgs,
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
    /// Carry on from where the stored history ends: the earliest of the
    /// pools' newest stored days. For a job that runs on a schedule.
    #[arg(long, conflicts_with = "from")]
    resume: bool,
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
    /// pool there, with the peers' records up to that look. A forward run
    /// (window `to: open`) is first brought up to the node's head.
    Next(NextArgs),
    /// Write the decision for the look `next` showed, then show the next.
    Decide(Box<DecideArgs>),
    /// Show again every record of the run's peers up to the look waiting
    /// for a decision.
    Peers(AgentArgs),
    /// Score every decision against what happened and the actions not
    /// taken, for one agent's run or every agent of the experiment.
    Evaluate(EvaluateArgs),
    /// Print the brief for a session acting as one agent of the experiment.
    Brief(AgentArgs),
    /// A forward or live panel: ask other agents a question at the waiting
    /// look, on the panel's board.
    Ask(AskArgs),
    /// The open questions to the agent; with --wait, wait for one to arrive.
    Inbox(InboxArgs),
    /// Answer a question on the board.
    Answer(AnswerArgs),
    /// The answers to the agent's questions at the waiting look; with
    /// --wait, wait until all have answered or the deadline has passed.
    Answers(AnswersArgs),
    /// The panel's board: every agent's questions and answers so far.
    Board(AgentArgs),
}

#[derive(Args)]
struct AskArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Who to ask: names of the panel's other agents, comma-separated, or all.
    #[arg(long)]
    to: String,
    /// The question, as long as it needs to be.
    #[arg(long)]
    question: String,
    /// How long they have to answer, counted from when the look was shown.
    #[arg(long, default_value = "10m")]
    wait: String,
}

#[derive(Args)]
struct InboxArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Wait up to this long (such as 6h) for a question to arrive, and
    /// return as soon as one does.
    #[arg(long)]
    wait: Option<String>,
}

#[derive(Args)]
struct AnswerArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// The question's id, as `agent inbox` shows it.
    #[arg(long)]
    ask: String,
    /// The answer, as long as it needs to be.
    #[arg(long)]
    answer: String,
    /// What you would do in the asker's place: stay_out, hold, exit or enter.
    #[arg(long)]
    would: Option<String>,
    /// For --would enter: the range's lower price, in percent from the asker's price.
    #[arg(long, allow_hyphen_values = true)]
    lower: Option<f64>,
    /// For --would enter: the range's upper price, in percent from the asker's price.
    #[arg(long, allow_hyphen_values = true)]
    upper: Option<f64>,
    /// How sure you are, from 0 to 1.
    #[arg(long)]
    confidence: Option<f64>,
}

#[derive(Args)]
struct AnswersArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// Wait until every addressee has answered or the deadline has passed.
    #[arg(long)]
    wait: bool,
}

#[derive(Args)]
struct AgentArgs {
    #[command(flatten)]
    experiment: ExperimentArgs,
    /// Which of the experiment's runs, by index, when it has several.
    #[arg(long)]
    run: Option<usize>,
    /// Which of the experiment's runs, by the name of the agent deciding it
    /// (`agent.name`, or its decisions file's name).
    #[arg(long)]
    agent: Option<String>,
    /// Which chain of the config to use.
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Data folder for the pool history. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// A forward run: the node endpoint the store is brought up to the
    /// head from. Defaults to LPBOT_RPC, then the config's local endpoint.
    #[arg(long)]
    rpc: Option<String>,
    /// A forward run: replay what is stored, without bringing it up to the
    /// head first.
    #[arg(long)]
    no_sync: bool,
    /// A forward run: how old the newest block may be, by the wall clock,
    /// for a look to be shown. An older one means the node is behind.
    #[arg(long, default_value = "5m")]
    max_lag: String,
}

impl AgentArgs {
    fn forward(&self, wait: u64) -> Result<agent_cmd::Forward> {
        let max_lag = run_cmd::parse_duration(&self.max_lag).ok_or_else(|| {
            anyhow::anyhow!("--max-lag {}: give a duration such as 5m", self.max_lag)
        })?;
        Ok(agent_cmd::Forward {
            sync: !self.no_sync,
            rpc: self.rpc.clone(),
            max_lag,
            wait,
        })
    }
}

#[derive(Args)]
struct NextArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// A forward run: when no look is due yet, wait for it, up to this
    /// long (such as 4h or 1d), then show it.
    #[arg(long)]
    wait: Option<String>,
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
    /// Why, in a sentence. Kept with the decision in the decisions file.
    #[arg(long, default_value = "")]
    note: String,
    /// The reasoning behind the decision, as long as it needs to be. It goes
    /// to the reasoning log next to the decisions file (`<name>.log.md`,
    /// local, not committed), with what the look showed, and to the shared
    /// log other agents are handed (`<name>.log.jsonl`); the run never reads it.
    #[arg(long, default_value = "")]
    reasoning: String,
    /// The lowest price, in percent from now, you expect before the next look.
    #[arg(long = "expect-low", allow_hyphen_values = true)]
    expect_low: Option<f64>,
    /// The highest price, in percent from now, you expect before the next look.
    #[arg(long = "expect-high", allow_hyphen_values = true)]
    expect_high: Option<f64>,
    /// How sure you are that price stays within that band, from 0 to 1.
    #[arg(long = "expect-confidence")]
    expect_confidence: Option<f64>,
    /// What you expect to happen, in words.
    #[arg(long, default_value = "")]
    expect: String,
    /// How the decision relates to the peers' records the look showed: own,
    /// copy, adapt or depart.
    #[arg(long)]
    stance: Option<String>,
    /// The peer a stance of copy, adapt or depart is toward.
    #[arg(long)]
    toward: Option<String>,
    /// Refuse if the pending look changed while the external decider worked.
    #[arg(long, requires = "expected_as_of")]
    expected_look: Option<u64>,
    /// The exact pending information boundary paired with --expected-look.
    #[arg(long, requires = "expected_look")]
    expected_as_of: Option<u64>,
}

#[derive(Args)]
struct EvaluateArgs {
    #[command(flatten)]
    agent: AgentArgs,
    /// How long each decision is left alone before it is scored: `next` (until
    /// the agent's own next look) or durations, comma-separated.
    #[arg(long, default_value = "next,1d")]
    horizons: String,
    /// Replays run at the same time; all cores by default.
    #[arg(long)]
    jobs: Option<usize>,
}

#[derive(Args)]
struct GasHistoryArgs {
    #[command(flatten)]
    chain: ChainArgs,
    /// First day to store, as YYYY-MM-DD. Without it, carry on from the
    /// newest chunk of blocks that is stored.
    #[arg(long)]
    from: Option<String>,
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
struct PaperArgs {
    #[command(flatten)]
    experiment: ExperimentArgs,
    #[arg(long, default_value = "ethereum")]
    chain: String,
    /// Data folder with the stored history. Defaults to LPBOT_DATA_DIR, then data/lake.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Node endpoint. Defaults to LPBOT_RPC, then the config's local endpoint.
    #[arg(long)]
    rpc: Option<String>,
    /// Folder the result tables are written to. Defaults to runs/paper/<experiment>.
    #[arg(long)]
    results_dir: Option<PathBuf>,
    /// How long to wait between steps.
    #[arg(long, default_value = "15s")]
    every: String,
    /// Steps between replays of the whole history against the paper run.
    #[arg(long, default_value_t = 240)]
    check_every: u32,
    /// Stop after this many steps instead of running until stopped.
    #[arg(long)]
    steps: Option<u32>,
    /// The pause flag.
    #[arg(long, default_value = "data/pause.json")]
    pause_flag: PathBuf,
}

#[derive(Args)]
struct PauseArgs {
    /// Why, for the record.
    #[arg(long)]
    reason: String,
    /// Who, for the record.
    #[arg(long, default_value = "owner")]
    by: String,
    /// The pause flag.
    #[arg(long, default_value = "data/pause.json")]
    pause_flag: PathBuf,
}

#[derive(Args)]
struct ResumeArgs {
    /// The pause flag.
    #[arg(long, default_value = "data/pause.json")]
    pause_flag: PathBuf,
}

#[derive(Args)]
struct SignerArgs {
    /// The keeper's encrypted keystore (Web3 Secret Storage v3).
    #[arg(long)]
    keystore: PathBuf,
    /// The socket the trader asks on; created with mode 0600.
    #[arg(long, default_value = "data/signer/keeper.sock")]
    socket: PathBuf,
    /// The chain id the signer signs for.
    #[arg(long, default_value_t = 1)]
    chain_id: u64,
    /// The roles module: the only address the signer signs calls to.
    #[arg(long)]
    roles_module: String,
    /// The most gas one transaction may use.
    #[arg(long, default_value_t = 2_000_000)]
    max_gas: u64,
    /// The highest fee per gas, in gwei.
    #[arg(long, default_value_t = 200.0)]
    max_fee_gwei: f64,
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

/// `lpbot pause` and `lpbot exit-all`.
fn pause(args: &PauseArgs, exit: bool) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    if args.reason.trim().is_empty() {
        anyhow::bail!("--reason says why, for the record");
    }
    let flag = lpbot_risk::pause::PauseFlag::new(&args.pause_flag)
        .set(exit, &args.reason, &args.by, now)
        .map_err(|e| anyhow::anyhow!("writing {}: {e}", args.pause_flag.display()))?;
    println!(
        "{} set in {}: {}. The trader {} at its next step; `lpbot resume` clears it.",
        if flag.exit { "Exit-all" } else { "Pause" },
        args.pause_flag.display(),
        flag.reason,
        if flag.exit {
            "takes every position out and holds new entries"
        } else {
            "holds new entries"
        }
    );
    Ok(())
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
        Command::AgentRunner(args) => agent_runner::run(&args),
        Command::RunnerJob { spec } => agent_runner::job(&spec),
        Command::RunnerModelJob { spec } => agent_runner::model_job(&spec),
        Command::Run(args) => {
            let _operations = agent_lock::lake(&backfill_cmd::data_dir(args.data_dir.as_deref()))?;
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
        Command::Paper(args) => {
            let runs = resolve(&args.experiment)?;
            print_runs(&runs, args.experiment.show_config);
            let experiment = runs
                .first()
                .map(|r| r.experiment.clone())
                .ok_or_else(|| anyhow::anyhow!("the experiment has no runs"))?;
            let every = run_cmd::parse_duration(&args.every)
                .filter(|s| *s > 0)
                .ok_or_else(|| {
                    anyhow::anyhow!("--every {}: give a duration such as 15s", args.every)
                })?;
            let results = args
                .results_dir
                .clone()
                .unwrap_or_else(|| PathBuf::from("runs/paper").join(&experiment));
            let journal = paper_cmd::journal_dir(std::path::Path::new("data"), &experiment);
            let pause = lpbot_risk::pause::PauseFlag::new(&args.pause_flag);
            let rpc = args
                .rpc
                .clone()
                .or_else(|| std::env::var("LPBOT_RPC").ok().filter(|v| !v.is_empty()));
            paper_cmd::follow(&paper_cmd::PaperArgs {
                runs: &runs,
                root: &backfill_cmd::data_dir(args.data_dir.as_deref()),
                results: &results,
                journal: &journal,
                chain: &args.chain,
                rpc: rpc.as_deref(),
                every: std::time::Duration::from_secs(every),
                check_every: args.check_every,
                steps: args.steps,
                pause: &pause,
            })
        }
        Command::Pause(args) => pause(&args, false),
        Command::ExitAll(args) => pause(&args, true),
        Command::Resume(args) => {
            lpbot_risk::pause::PauseFlag::new(&args.pause_flag)
                .clear()
                .map_err(|e| anyhow::anyhow!("clearing {}: {e}", args.pause_flag.display()))?;
            println!("The pause flag {} is clear.", args.pause_flag.display());
            Ok(())
        }
        Command::Signer(args) => {
            let roles = chain_cmd::checked_address("roles module", &args.roles_module)?;
            let text = std::fs::read_to_string(&args.keystore)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", args.keystore.display()))?;
            let password = lpbot_signer::password::read_password("Keystore password: ")?;
            let key = lpbot_signer::keystore::decrypt(&text, &password)?;
            drop(password);
            let policy = lpbot_signer::policy::Policy {
                chain_id: args.chain_id,
                roles_module: roles,
                max_gas_limit: args.max_gas,
                max_fee_per_gas: (args.max_fee_gwei * 1e9) as u128,
            };
            if let Some(parent) = args.socket.parent() {
                std::fs::create_dir_all(parent)?;
            }
            println!(
                "Signing for {} on chain {}, calls to {roles:#x} only, on {}.",
                key.address(),
                args.chain_id,
                args.socket.display()
            );
            lpbot_signer::server::serve(&args.socket, &key, &policy, &mut std::io::stdout())?;
            Ok(())
        }
        Command::Calibrate(args) => {
            let _operations =
                agent_lock::lake(&backfill_cmd::data_dir(args.run.data_dir.as_deref()))?;
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
            let _operations = agent_lock::lake(&backfill_cmd::data_dir(args.data_dir.as_deref()))?;
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
        Command::LoadLooks(args) => {
            let url = load_cmd::database(args.db).ok_or_else(|| {
                anyhow::anyhow!(
                    "no database given: pass --db or set LPBOT_DB_URL \
                     (`deploy/agents/refresh.sh --looks` takes it from deploy/.env)"
                )
            })?;
            let runs = resolve(&args.experiment)?;
            let mut failed = 0;
            for (agent, outcome) in load_looks_cmd::run(&url, &runs)? {
                match outcome {
                    Ok(outcome) => println!("{}", outcome.describe()),
                    Err(error) => {
                        failed += 1;
                        eprintln!("agent {agent}: the looks were not loaded: {error:#}");
                    }
                }
            }
            if failed > 0 {
                bail!("the looks of {failed} agent run(s) were not loaded");
            }
            Ok(())
        }
        Command::VerifyFees(args) => {
            let _operations = agent_lock::lake(&backfill_cmd::data_dir(args.data_dir.as_deref()))?;
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
            let _operations = agent_lock::lake(&backfill_cmd::data_dir(args.data_dir.as_deref()))?;
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
                    resume: args.resume,
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
            let _operations = agent_lock::lake(&backfill_cmd::data_dir(args.data_dir.as_deref()))?;
            let chain = chain_cmd::connect(
                &args.chain.config,
                &args.chain.chain,
                args.chain.rpc.as_deref(),
            )?;
            let from = args
                .from
                .as_deref()
                .map(|text| {
                    text.parse::<lpbot_data::day::Day>()
                        .map_err(|e| anyhow::anyhow!("--from {text}: {e}"))
                })
                .transpose()?;
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
            let root = backfill_cmd::data_dir(args.data_dir.as_deref());
            let first = match from {
                Some(day) => {
                    lpbot_data::backfill::first_block_at_or_after(&source, day.start(), 1, to)?
                }
                None => lpbot_data::gas::GasStore::new(&root)
                    .newest_chunk(&args.chain.chain)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "no gas prices are stored under {} yet, so there is nothing to \
                             carry on from: give --from <YYYY-MM-DD> for the first run",
                            root.display()
                        )
                    })?,
            };
            println!(
                "  gas prices of blocks {first} to {to} into {}",
                root.display()
            );
            let done = gas_history_cmd::run(
                client,
                &root,
                &args.chain.chain,
                first,
                to,
                to,
                args.threads,
            )?;
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
                AgentCommand::Next(args) => &args.agent,
                AgentCommand::Peers(args)
                | AgentCommand::Brief(args)
                | AgentCommand::Board(args) => args,
                AgentCommand::Decide(args) => &args.agent,
                AgentCommand::Evaluate(args) => &args.agent,
                AgentCommand::Ask(args) => &args.agent,
                AgentCommand::Inbox(args) => &args.agent,
                AgentCommand::Answer(args) => &args.agent,
                AgentCommand::Answers(args) => &args.agent,
            };
            let runs = resolve(&args.experiment)?;
            let root = backfill_cmd::data_dir(args.data_dir.as_deref());
            // Every agent of the experiment: the panel that shares a board.
            let panel: Vec<agent_log::Identity> = runs
                .iter()
                .filter_map(|r| r.config.strategies.first())
                .map(agent_log::identity)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            let duration = |flag: &str, text: &str| {
                run_cmd::parse_duration(text)
                    .filter(|s| *s > 0)
                    .ok_or_else(|| {
                        anyhow::anyhow!("--{flag} {text}: give a duration such as 10m, 6h or 1h30m")
                    })
            };
            let poll = std::time::Duration::from_secs(5);
            if let AgentCommand::Board(_) = &command {
                print!("{}", agent_board::summary(&panel)?);
                return Ok(());
            }
            if let AgentCommand::Evaluate(e) = &command {
                let _operations = agent_lock::lake(&root)?;
                let horizons = agent_eval::parse_horizons(&e.horizons)?;
                let jobs = e.jobs.unwrap_or_else(|| {
                    std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
                });
                let chosen: Vec<&ResolvedRun> = if args.run.is_some() || args.agent.is_some() {
                    vec![pick_run(&runs, args.run, args.agent.as_deref())?]
                } else {
                    runs.iter().collect()
                };
                let mut reports = Vec::new();
                for run in chosen {
                    let Some(who) = agent_log::identity(&run.config.strategies[0])? else {
                        continue;
                    };
                    let report =
                        agent_eval::evaluate(run, &who, &root, &args.chain, &horizons, jobs)?;
                    print!("{}", report.describe());
                    reports.push(report);
                }
                if reports.is_empty() {
                    bail!("no run of the experiment is decided by an agent (agent.decisions)");
                }
                if reports.len() > 1 {
                    println!();
                    print!("{}", agent_eval::compare(&reports));
                }
                return Ok(());
            }
            let run = pick_run(&runs, args.run, args.agent.as_deref())?;
            let _command = if matches!(&command, AgentCommand::Brief(_) | AgentCommand::Next(_)) {
                None
            } else if matches!(&command, AgentCommand::Inbox(_) | AgentCommand::Answers(_)) {
                // Waiting for the board must not exclude the same agent's
                // concurrent next/answer process for up to two days.
                drop(agent_lock::command(
                    &agent_cmd::identity_of(run)?.decisions,
                )?);
                None
            } else {
                Some(agent_lock::command(
                    &agent_cmd::identity_of(run)?.decisions,
                )?)
            };
            if let AgentCommand::Decide(d) = &command {
                if let (Some(look), Some(at)) = (d.expected_look, d.expected_as_of) {
                    let pending = agent_cmd::read_pending(&agent_cmd::identity_of(run)?.decisions)?;
                    agent_cmd::verify_pending_context(
                        &pending,
                        look,
                        at,
                        agent_log::now_unix(),
                        args.forward(0)?.max_lag,
                    )?;
                }
            }
            match &command {
                AgentCommand::Next(n) => {
                    let wait = match &n.wait {
                        None => 0,
                        Some(text) => run_cmd::parse_duration(text).ok_or_else(|| {
                            anyhow::anyhow!(
                                "--wait {text}: give a duration such as 4h, 4h30m or 1d"
                            )
                        })?,
                    };
                    agent_cmd::next(run, &root, &args.chain, &args.forward(wait)?).map(|_| ())
                }
                AgentCommand::Peers(_) => agent_cmd::peers(run),
                AgentCommand::Brief(_) => {
                    let who = agent_cmd::identity_of(run)?;
                    let others: Vec<String> = panel
                        .iter()
                        .filter(|p| {
                            p.name != who.name
                                && agent_board::board_path(&p.decisions)
                                    == agent_board::board_path(&who.decisions)
                        })
                        .map(|p| p.name.clone())
                        .collect();
                    print!(
                        "{}",
                        agent_brief::brief(&args.experiment.experiment, run, &who, &others)?
                    );
                    Ok(())
                }
                AgentCommand::Ask(a) => {
                    let wait = duration("wait", &a.wait)?;
                    let ask = agent_board::ask_at(
                        run,
                        &panel,
                        &a.to,
                        &a.question,
                        wait,
                        agent_log::now_unix(),
                    )?;
                    let to: Vec<&str> = ask["to"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|t| t.as_str())
                        .collect();
                    println!(
                        "Asked {} (question {}). Answers are due by {}. `lpbot agent answers                          <experiment> --agent {} --wait` returns once all have answered or the                          deadline has passed; then decide, with or without them.",
                        to.join(", "),
                        ask["id"].as_str().unwrap_or_default(),
                        agent_cmd::clock(ask["deadline"].as_u64().unwrap_or_default()),
                        ask["agent"].as_str().unwrap_or_default()
                    );
                    Ok(())
                }
                AgentCommand::Inbox(i) => {
                    let wait = match &i.wait {
                        None => 0,
                        Some(text) => duration("wait", text)?,
                    };
                    agent_board::inbox(run, wait, poll).map(|_| ())
                }
                AgentCommand::Answer(a) => {
                    let would = a.would.as_ref().map(|action| agent_board::Would {
                        action: action.clone(),
                        lower_pct: a.lower,
                        upper_pct: a.upper,
                    });
                    if would.is_none() && (a.lower.is_some() || a.upper.is_some()) {
                        bail!("--lower and --upper go with --would enter");
                    }
                    let answer = agent_board::answer_at(
                        run,
                        &a.ask,
                        &a.answer,
                        would.as_ref(),
                        a.confidence,
                        agent_log::now_unix(),
                    )?;
                    println!(
                        "Answered {}{}.",
                        a.ask,
                        if answer["late"] == true {
                            ", after its deadline: it is kept and marked late"
                        } else {
                            ", in time"
                        }
                    );
                    Ok(())
                }
                AgentCommand::Answers(a) => agent_board::answers(run, a.wait, poll),
                AgentCommand::Board(_) => unreachable!("handled above"),
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
                        reasoning: &d.reasoning,
                        expect: agent_log::Expectation {
                            low_pct: d.expect_low,
                            high_pct: d.expect_high,
                            confidence: d.expect_confidence,
                            text: d.expect.clone(),
                        },
                        stance: d.stance.as_deref(),
                        toward: d.toward.as_deref(),
                    },
                    &args.forward(0)?,
                ),
                AgentCommand::Evaluate(_) => unreachable!("handled above"),
            }
        }
        Command::PoolStats(args) => {
            let _operations = agent_lock::lake(&backfill_cmd::data_dir(args.data_dir.as_deref()))?;
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
        Command::Supervisor(SupervisorCommand::Status(args)) => supervisor_status::run(&args),
        Command::Select(SelectCommand::WalkForward(args)) => {
            let fold = run_cmd::parse_duration(&args.fold)
                .filter(|seconds| *seconds >= 86_400)
                .ok_or_else(|| {
                    anyhow::anyhow!("--fold must be a duration of a day or more, such as 30d")
                })?;
            let fits = select_cmd::parse_fits(&args.fit)?;
            print!(
                "{}",
                select_cmd::walk_forward_report(&select_cmd::WalkForwardArgs {
                    results: &args.results_dir,
                    experiment: args.experiment.as_deref(),
                    fold,
                    fits: &fits,
                    min_fit: args.min_fit,
                    by: args.by.as_deref(),
                    across_pools: args.across_pools,
                })?
            );
            Ok(())
        }
        Command::Select(SelectCommand::List(args)) => {
            let plan = args
                .plan
                .as_deref()
                .map(|path| -> Result<select_cmd::Plan> {
                    let bytes = std::fs::read(path)
                        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
                    serde_json::from_slice(&bytes).map_err(|e| {
                        anyhow::anyhow!(
                            "{} is not a plan `lpbot select ladder` wrote: {e}",
                            path.display()
                        )
                    })
                })
                .transpose()?;
            let entries = select_ops::entries(&args.experiments, plan.as_ref())?;
            print!("{}", select_ops::list_text(&entries, plan.as_ref()));
            Ok(())
        }
        Command::Select(SelectCommand::Approve(args)) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            let approval = select_ops::approve(&args.plan, &args.by, now)?;
            println!(
                "Approved ladder `{}` as of {} (sha256 {}) by {}, written to {}.",
                approval.ladder,
                approval.as_of,
                approval.sha256,
                approval.approved_by,
                select_ops::approval_path(&args.plan).display()
            );
            Ok(())
        }
        Command::Select(SelectCommand::Check(args)) => {
            let max_age = run_cmd::parse_duration(&args.max_age).ok_or_else(|| {
                anyhow::anyhow!("--max-age {}: give a duration such as 36h", args.max_age)
            })?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            let (plan, approval) = select_ops::approved_plan(&args.plan, max_age, now)?;
            let taken: usize = plan
                .tracks
                .iter()
                .map(|t| t.lines.iter().filter(|l| l.share > 0.0).count())
                .sum();
            println!(
                "The trader takes {}: ladder `{}` as of {}, approved by {}, {taken} candidate(s) \
                 with a share.",
                args.plan.display(),
                plan.ladder,
                plan.as_of,
                approval.approved_by
            );
            Ok(())
        }
        Command::Select(SelectCommand::Ladder(args)) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            print!("{}", select_cmd::run_ladder(&args.file, &args.out, now)?);
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

/// The run an agent command acts on: by index, by the agent deciding it, or
/// the only one.
fn pick_run<'a>(
    runs: &'a [ResolvedRun],
    index: Option<usize>,
    agent: Option<&str>,
) -> Result<&'a ResolvedRun> {
    match (index, agent, runs.len()) {
        (Some(_), Some(_), _) => bail!("say which run with --run or --agent, not both"),
        (Some(i), None, n) => runs
            .get(i)
            .ok_or_else(|| anyhow::anyhow!("--run {i}: the experiment has {n} run(s)")),
        (None, Some(name), _) => {
            let mut names = Vec::new();
            for run in runs {
                if let Some(who) = run
                    .config
                    .strategies
                    .first()
                    .map(agent_log::identity)
                    .transpose()?
                    .flatten()
                {
                    if who.name == name {
                        return Ok(run);
                    }
                    names.push(who.name);
                }
            }
            if names.is_empty() {
                bail!("--agent {name}: no run of the experiment is decided by an agent")
            }
            bail!(
                "--agent {name}: the experiment's agents are {}",
                names.join(", ")
            )
        }
        (None, None, 1) => Ok(&runs[0]),
        (None, None, n) => {
            bail!("the experiment has {n} runs; say which with --agent <name> or --run <index>")
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
