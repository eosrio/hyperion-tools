//! wseg — build a frozen WormDB Light-API segment (`.wseg`).
//!
//! The [`Builder`] accumulates the Light-API tables from mapped Hyperion docs and writes the segment.
//! It is fed by either a Mongo source (the `wseg-build` binary) or a snapshot source (snapshot-load's
//! `--wseg` sink), so a servable segment can be built with or without MongoDB.

pub mod builder;
pub mod name;
pub mod wseg;

pub use builder::Builder;
pub use wseg::{write_segment, IndexEntry, Table};
