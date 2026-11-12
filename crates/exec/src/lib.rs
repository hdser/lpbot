//! Execution: planner, custody, signer client, submitter and journal.
//!
//! Custody is a Safe with the Zodiac Roles module. The executor reaches it
//! through the `Custody` trait, so a different custody could be added later
//! without touching strategy code.
//!
//! Not implemented yet. See `docs/DESIGN.md`, "Execution" and "Smart contracts".

/// Where the funds are held and how position changes reach the chain.
pub trait Custody: Send + Sync {
    /// Human-readable name, for logs and result tables.
    fn name(&self) -> &str;
}
