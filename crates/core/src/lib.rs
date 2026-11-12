//! Shared types, configuration and experiment files for lpbot.
//!
//! Everything here is pure and has no network or chain dependency, so it can
//! be built and tested anywhere.

pub mod config;
pub mod experiment;
pub mod metrics;
pub mod types;

pub use config::{
    Autonomy, ChainConfig, Config, ConfigError, Mode, PatienceMode, RunInputs, TokenConfig,
};
pub use experiment::{resolve_experiment, ResolvedRun, RunOverrides};
pub use types::{Campaign, EntrySide, Event, ExitReason, Intent, PoolId, RangeTarget, Tick};
