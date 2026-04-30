//! GrumpyDB Server — networked multi-tenant database server.
//!
//! This crate provides TCP/TLS networking, JWT authentication, and RBAC
//! authorization on top of the GrumpyDB storage engine.

pub mod auth;
pub mod cluster;
pub mod config;
pub mod coordinator;
pub mod http;
pub mod limits;
pub mod session;
pub mod snapshot;
pub mod tcp;
