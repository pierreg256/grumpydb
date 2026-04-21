//! Concurrency: SWMR (Single-Writer, Multi-Reader) thread-safe access.
//!
//! Provides [`SharedDb`](lock_manager::SharedDb), a thread-safe wrapper around
//! `GrumpyDb` that enables concurrent access from multiple threads.

pub mod lock_manager;
