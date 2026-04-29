//! Write-Ahead Log for crash recovery and durability.
//!
//! The WAL ensures that committed transactions survive crashes. Every page
//! modification is logged before being applied, and recovery replays
//! committed transactions on startup.
//!
//! Phase 40b adds a Hybrid Logical Clock and per-record vector clocks
//! (see [`hlc`] and [`vclock`]) and bumps the on-disk format to v2
//! (see [`record::WAL_VERSION_V2`]).

pub mod applied_set;
pub mod hlc;
pub mod record;
pub mod recovery;
pub mod vclock;
pub mod writer;
