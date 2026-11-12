//! The `lpbot` command line.
//!
//! Working today: `plan` (resolve an experiment file into its runs) and `iv`
//! (implied volatility from fee, volume and in-range liquidity). `run` resolves
//! and validates, then stops, because the replay engine is not built yet.

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
    Run(ExperimentArgs),
    /// Implied volatility from fee tier, daily volume and in-range liquidity.
    Iv(IvArgs),
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
            let runs = resolve(&args)?;
            print_runs(&runs, args.show_config);
            bail!(
                "resolved {} run(s), but the replay engine is not implemented yet (build plan, phase 1)",
                runs.len()
            )
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
