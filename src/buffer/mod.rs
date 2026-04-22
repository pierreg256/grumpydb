//! Buffer pool: LRU cache for pages in memory.
//!
//! The buffer pool caches frequently accessed pages to reduce disk I/O.
//! It uses LRU eviction when the pool is full and tracks dirty pages
//! for efficient flushing.

pub mod frame;
pub mod pool;
