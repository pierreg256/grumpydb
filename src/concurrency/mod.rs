//! Concurrency: SWMR (Single-Writer, Multi-Reader) thread-safe access.
//!
//! Provides thread-safe wrappers for all GrumpyDB layers:
//!
//! - [`SharedDb`](lock_manager::SharedDb) — wraps single-collection `GrumpyDb` (backward compat)
//! - [`SharedDatabase`](shared::SharedDatabase) — wraps multi-collection `Database` (per-database SWMR)
//! - [`SharedServer`](shared::SharedServer) — wraps `GrumpyServer` (multi-tenant, per-database concurrency)

pub mod lock_manager;
pub mod shared;
