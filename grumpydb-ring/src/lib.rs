//! GrumpyDB consistent-hash ring with virtual nodes.
//!
//! Internal crate (`publish = false`): the API is shaped for v5 single-node
//! deployments AND v6 N-node clusters with no change at the call site.
//!
//! # Overview
//!
//! A [`Ring`] is a sorted collection of *vnodes* (virtual nodes). Each
//! physical node owns `vnodes_per_node` (default 256, Cassandra-style)
//! positions on a 64-bit hash circle. Keys are mapped to nodes by
//! hashing them with Murmur3 (x64_128, low 64 bits) and walking the
//! ring clockwise from that position.
//!
//! Routing keys are built from the canonical tuple
//! `(database, collection, key_bytes)` so identical document keys in
//! different collections never collide on the ring.
//!
//! # Example
//!
//! ```
//! use grumpydb_ring::{Ring, RingConfig, RoutingKey};
//!
//! let mut ring: Ring<&'static str> = Ring::new(RingConfig::default());
//! ring.add_node("node-A");
//! ring.add_node("node-B");
//! ring.add_node("node-C");
//!
//! let key = RoutingKey {
//!     database: "users",
//!     collection: "profiles",
//!     key_bytes: b"alice",
//! };
//!
//! let owners = ring.preference_list(&key, 2);
//! assert_eq!(owners.len(), 2);
//! ```

#![warn(missing_docs)]

pub use hash::{RoutingKey, murmur3_hash};
pub use ring::{KeyRange, NodeIdOpaque, Ring, RingConfig, RingError};

mod hash;
mod ring;
