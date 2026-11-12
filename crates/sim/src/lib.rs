//! Simulation: the replay engine, cost models and result tables.
//!
//! The backtester replays real pool events through the same pool math and
//! the same strategy code that run live, with the bot's position inserted as
//! a virtual one. Every run writes the same six result tables.
//!
//! See `docs/DESIGN.md`, "Simulation and backtesting".

pub mod engine;

#[cfg(test)]
mod engine_tests;

/// Names of the tables every run writes. The viewer knows only these.
pub const RESULT_TABLES: [&str; 6] = [
    "runs",
    "campaigns",
    "decisions",
    "series",
    "pool_rank",
    "zone_map",
];
