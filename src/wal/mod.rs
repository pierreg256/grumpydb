//! Write-Ahead Log for crash recovery and durability.
//!
//! The WAL ensures that committed transactions survive crashes. Every page
//! modification is logged before being applied, and recovery replays
//! committed transactions on startup.

pub mod record;
pub mod recovery;
pub mod writer;
