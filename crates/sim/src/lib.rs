//! Simulation: the replay engine, cost models and result tables.
//!
//! The backtester replays real pool events through the same pool math and
//! the same strategy code that run live, with the bot's position inserted as
//! a virtual one. Every run writes the same six result tables.
//!
//! See `docs/DESIGN.md`, "Simulation and backtesting".

pub mod agent;
pub mod agent_tables;
pub mod engine;
pub mod live_db;
pub mod live_tables;
pub mod loader;
pub mod supervisor_db;
pub mod supervisor_tables;
pub mod tables;

pub use live_tables::LIVE_TABLES;

#[cfg(test)]
mod engine_tests;

/// Names of the tables every run writes.
pub const RESULT_TABLES: [&str; 6] = [
    "runs",
    "campaigns",
    "decisions",
    "series",
    "pool_rank",
    "zone_map",
];

/// The pool tables, keyed by pool instead of by run: what a pool looked
/// like over its stored history, written by `lpbot pool-stats`.
pub const POOL_TABLES: [&str; 2] = ["pool_series", "pool_liquidity"];

/// The agent tables, keyed by run like the result tables: what an agent
/// run's looks showed, decided and expected, and how its decisions scored
/// (`agent_tables`). Only agent runs write them.
pub const AGENT_TABLES: [&str; 2] = ["agent_looks", "agent_scores"];

/// The supervisor tables: what every forward agent is doing now, written
/// by `lpbot supervisor status --load` (`supervisor_tables`). Keyed by run,
/// with every row replaced at each pass.
pub const SUPERVISOR_TABLES: [&str; 1] = ["agent_status"];
