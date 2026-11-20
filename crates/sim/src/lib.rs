//! Simulation: the replay engine, cost models and result tables.
//!
//! The backtester replays real pool events through the same pool math and
//! the same strategy code that run live, with the bot's position inserted as
//! a virtual one. Every run writes the same six result tables.
//!
//! See `docs/DESIGN.md`, "Simulation and backtesting".

pub mod agent;
pub mod engine;
pub mod live_db;
pub mod live_tables;
pub mod loader;
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
