//! Execution: planner, custody, signer client, submitter and journal.
//!
//! Custody is a Safe with the Zodiac Roles module. The executor reaches it
//! through the `Custody` trait, so a different custody could be added later
//! without touching strategy code.
//!
//! Built so far: the calls (`calls`, over `abi`); the planner, which turns
//! what the engine did (`lpbot_core::StepRecord`) into them; the journal,
//! which records every step before it is taken and says what to do with an
//! action a crash left open; and the submitter, which owns the nonce and
//! replaces a stuck transaction. Reading positions back after a
//! transaction (reconciliation) is not written yet. See
//! `docs/DESIGN.md`, "Execution" and "Smart contracts", and `docs/PLAN.md`.

pub mod abi;
pub mod calls;
pub mod journal;
pub mod planner;
pub mod submit;

/// Where the funds are held and how position changes reach the chain.
pub trait Custody: Send + Sync {
    /// Human-readable name, for logs and result tables.
    fn name(&self) -> &str;
}
