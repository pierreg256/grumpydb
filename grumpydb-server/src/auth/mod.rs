//! Authentication and authorization subsystem.
//!
//! - [`role`] — Role names, actions, permissions, scope coverage
//! - [`user`] — User records, argon2 password hashing
//! - [`jwt`] — JWT token generation and verification (HS256)
//! - [`store`] — Persistent auth store (users + server secret on disk)

pub mod jwt;
pub mod role;
pub mod store;
pub mod user;
