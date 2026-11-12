//! Data: the zone map, and later the Parquet sinks and backfill.
//!
//! The zone map is built from a pool's price path. The event backfill and
//! the `pool_rank` and `zone_map` tables on disk are not implemented yet.
//! See `docs/DESIGN.md`, "Data ingestion and storage" and "Experiments and
//! results".

pub mod zone_map;
