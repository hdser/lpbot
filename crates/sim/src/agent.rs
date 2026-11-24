//! The agent mode: a run whose entries and exits are decided by someone
//! looking at the pool, a person or a Claude session, one look at a time.
//!
//! At each look the engine stops and describes the pool (`Observation`):
//! how price moved over several time scales, where it sits in each range,
//! how busy the pool is, the position and its result. The decision for that
//! look (`AgentDecision`) says what to do and when to look again. The
//! decisions are kept in a file and the run is replayed from them, so a
//! finished session is an ordinary, reproducible run. Stops, time limits
//! and the loss limit still apply between looks.
//!
//! A look is due at its time and shown at the first block of the pool at or
//! after it. In a forward run (`Settings::agent_forward`, a window open at
//! the end) a look that is due and not yet decided is shown at the newest
//! stored block instead, so a session that comes back late sees the pool
//! as it is, and its decision records that block's time (`at`): the replay
//! acts on it there and nowhere earlier.

/// What the agent decided at one look.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDecision {
    /// The time of the look it answers: the time the engine asked at.
    pub look: u64,
    /// The time of the block the look was shown at, when recorded. The
    /// decision is carried out from the first block at or after it, never
    /// earlier than its look. `None` acts at the look's first block.
    pub at: Option<u64>,
    pub action: AgentAction,
    /// Seconds until the next look.
    pub next_look: u64,
    /// Why, in the agent's words, for the decisions table.
    pub note: String,
}

/// What to do at a look.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentAction {
    /// Flat, and stay flat.
    StayOut,
    /// Keep what is open as it is.
    Hold,
    /// Close what is open, back to the value token.
    Exit,
    /// Enter (or, with a position open, move to) the range between these
    /// two volatile-token prices, in percent from its price now; negative
    /// is below. A range wholly on the value token's side is entered
    /// one-sided; anywhere else the value token is swapped into its mix.
    Enter { lower_pct: f64, upper_pct: f64 },
}

impl AgentAction {
    pub fn name(&self) -> &'static str {
        match self {
            AgentAction::StayOut => "stay_out",
            AgentAction::Hold => "hold",
            AgentAction::Exit => "exit",
            AgentAction::Enter { .. } => "enter",
        }
    }
}

/// How the pool looks at a look. Prices are the volatile token's, in
/// percent from its price at the look; no dates and no price levels.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    /// The look's time, which its decision names.
    pub look: u64,
    /// Looks before this one in the run.
    pub looks_before: usize,
    /// Days since trading started.
    pub day: f64,
    /// The block the pool is seen at, and its time.
    pub block: u64,
    pub time: u64,
    /// One row per time scale, shortest first.
    pub scales: Vec<ScaleView>,
    /// The pool's fee and one tick spacing, in percent.
    pub fee_pct: f64,
    pub spacing_pct: f64,
    /// Gas now, and what entering and leaving cost, in percent of the
    /// run's equity.
    pub gas_gwei: f64,
    pub round_trip_gas_pct: f64,
    /// The run so far, in the value token: its result in percent, its
    /// campaigns, and seconds since the last one ended.
    pub result_pct: f64,
    pub campaigns: u32,
    pub since_last_close: Option<u64>,
    pub position: Option<PositionView>,
}

/// Price and activity over one time scale ending at the look.
#[derive(Debug, Clone, PartialEq)]
pub struct ScaleView {
    pub label: &'static str,
    pub seconds: u64,
    /// How far the price moved over the scale.
    pub change_pct: f64,
    /// The highest and lowest price over it, from the price now.
    pub high_pct: f64,
    pub low_pct: f64,
    /// Realized volatility over it, per day.
    pub vol_pct_per_day: Option<f64>,
    /// Swaps over it, and its volume per hour against the 30-day average.
    pub swaps: usize,
    pub volume_vs_30d: Option<f64>,
}

/// The open position at a look.
#[derive(Debug, Clone, PartialEq)]
pub struct PositionView {
    /// Its range, in percent from the price now.
    pub lower_pct: f64,
    pub upper_pct: f64,
    /// `in_range`, `resting` (all in the value token) or `beyond` (all in
    /// the other token).
    pub state: String,
    pub held_seconds: u64,
    /// The campaign's result and fees so far, in percent of what went in.
    pub result_pct: f64,
    pub fees_pct: f64,
    /// Share of the position held in the value token now.
    pub value_share_pct: f64,
}

/// The time scales an observation reports, in seconds.
pub const SCALES: [(&str, u64); 7] = [
    ("15m", 900),
    ("1h", 3_600),
    ("4h", 14_400),
    ("1d", 86_400),
    ("7d", 604_800),
    ("30d", 2_592_000),
    ("90d", 7_776_000),
];

/// The run's plan in the agent mode: the decisions so far, in the order of
/// the looks they answer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentPlan {
    pub decisions: Vec<AgentDecision>,
}

/// Percent change of the volatile token's price for a move of `ticks` in
/// the direction where it rises.
pub fn ticks_to_pct(ticks: f64) -> f64 {
    (1.0001f64.powf(ticks) - 1.0) * 100.0
}

/// Ticks of a move of `pct` percent in the volatile token's price,
/// in the direction where it rises.
pub fn pct_to_ticks(pct: f64) -> f64 {
    (1.0 + pct / 100.0).ln() / 1.0001f64.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_and_percent_convert_both_ways() {
        assert!((ticks_to_pct(pct_to_ticks(3.0)) - 3.0).abs() < 1e-9);
        assert!((ticks_to_pct(pct_to_ticks(-2.5)) + 2.5).abs() < 1e-9);
        assert!(
            (pct_to_ticks(1.0) - 99.5).abs() < 0.1,
            "about 100 ticks a percent"
        );
        assert_eq!(
            AgentAction::Enter {
                lower_pct: -2.0,
                upper_pct: -1.0
            }
            .name(),
            "enter"
        );
    }
}
