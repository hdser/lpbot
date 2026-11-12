//! Data: the event store, the backfill that fills it, and the zone map.
//!
//! See `docs/DESIGN.md`, "Data ingestion and storage" and "Experiments and
//! results".

pub mod backfill;
pub mod day;
pub mod fee_check;
pub mod lake;
pub mod odds;
pub mod snapshot;
pub mod store;
pub mod synth;
pub mod zone_map;
