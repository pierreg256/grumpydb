//! Internal helpers for GrumpyDB end-to-end integration tests.
//!
//! This crate is not published; it exists solely to spawn an isolated
//! `grumpydb-server` process per test so the binary can be exercised through
//! the real TCP wire protocol.

mod server;

pub use server::TestServer;
