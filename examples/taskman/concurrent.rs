//! # Concurrent operations — demonstrating GrumpyDB's SharedDb
//!
//! This module shows how to use [`SharedDb`] for thread-safe access to GrumpyDB.
//! It demonstrates the SWMR (Single-Writer, Multi-Reader) concurrency model:
//!
//! - **Multiple readers** can access the database concurrently (shared lock)
//! - **One writer** at a time gets exclusive access (write lock)
//! - No deadlocks — `parking_lot::RwLock` is fair and efficient
//!
//! ## Key pattern: Arc<RwLock<GrumpyDb>> via SharedDb
//!
//! ```text
//! Thread 1 (reader)  ─┐
//! Thread 2 (reader)  ─┤── SharedDb::get()  → shared lock (non-blocking)
//! Thread 3 (reader)  ─┘
//! Thread 4 (writer)  ──── SharedDb::insert() → exclusive lock (blocks readers)
//! ```
//!
//! `SharedDb` is cheaply cloneable (it's an `Arc` wrapper). Pass clones to threads.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use grumpydb::{SharedDb, Value};
use uuid::Uuid;

use super::task::Task;

// ─────────────────────────────────────────────────────────────────────────────
// BENCH: concurrent read/write benchmark
//
// Demonstrates: spawning threads with SharedDb clones, measuring throughput.
// ─────────────────────────────────────────────────────────────────────────────

/// Runs a concurrent benchmark: N writer threads + M reader threads.
///
/// ## How it works
///
/// 1. Opens a `SharedDb` (thread-safe GrumpyDB handle)
/// 2. Spawns writer threads — each inserts `count` tasks
/// 3. Spawns reader threads — each reads all keys written so far
/// 4. Measures total throughput (ops/sec)
///
/// ## What this demonstrates
///
/// - `SharedDb::clone()` is cheap (Arc clone) — pass to any thread
/// - Writers get exclusive access automatically
/// - Readers can run concurrently (though current impl uses write lock internally)
pub fn run_bench(db_path: &Path, writers: usize, readers: usize, count: usize) -> Result<(), String> {
    // SharedDb::open() creates a thread-safe database handle.
    // Unlike GrumpyDb, it can be cloned and shared across threads.
    let db = SharedDb::open(db_path).map_err(|e| format!("Failed to open: {e}"))?;

    println!("Benchmark: {writers} writers × {count} inserts + {readers} readers");
    println!("{}", "-".repeat(50));

    // ── Phase 1: Writers ────────────────────────────────────────────────
    let start = Instant::now();
    let mut writer_handles = Vec::new();

    for t in 0..writers {
        // Clone the handle — each thread gets its own Arc reference.
        // This is the standard pattern for sharing SharedDb across threads.
        let db = db.clone();
        writer_handles.push(std::thread::spawn(move || {
            let base = (t * count) as u128;
            for i in 0..count {
                let key = Uuid::from_u128(base + i as u128);
                let value = Value::String(format!("bench_task_{t}_{i}"));
                // SharedDb::insert() acquires an exclusive write lock.
                // Other threads (readers and writers) wait during this call.
                db.insert(key, value).unwrap();
            }
        }));
    }

    for h in writer_handles {
        h.join().map_err(|_| "Writer thread panicked")?;
    }

    let write_elapsed = start.elapsed();
    let total_writes = writers * count;
    let write_ops_sec = total_writes as f64 / write_elapsed.as_secs_f64();
    println!("  Writes: {total_writes} in {write_elapsed:.2?} ({write_ops_sec:.0} ops/sec)");

    // ── Phase 2: Readers ────────────────────────────────────────────────
    let start = Instant::now();
    let total_keys = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut reader_handles = Vec::new();

    for _ in 0..readers {
        let db = db.clone();
        let total_keys = total_keys.clone();
        reader_handles.push(std::thread::spawn(move || {
            // SharedDb::scan() acquires a lock to iterate all documents.
            // Multiple readers can interleave their scans.
            let results = db.scan(..).unwrap();
            total_keys.fetch_add(results.len(), std::sync::atomic::Ordering::Relaxed);
        }));
    }

    for h in reader_handles {
        h.join().map_err(|_| "Reader thread panicked")?;
    }

    let read_elapsed = start.elapsed();
    let total_reads = total_keys.load(std::sync::atomic::Ordering::Relaxed);
    let read_ops_sec = total_reads as f64 / read_elapsed.as_secs_f64();
    println!("  Reads:  {total_reads} docs across {readers} threads in {read_elapsed:.2?} ({read_ops_sec:.0} docs/sec)");

    // ── Cleanup ─────────────────────────────────────────────────────────
    db.flush().map_err(|e| format!("Flush failed: {e}"))?;
    db.close().map_err(|e| format!("Close failed: {e}"))?;
    println!("  Done.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// SERVE: simple TCP server demonstrating concurrent access
//
// Demonstrates: SharedDb shared across connection handler threads.
// Protocol: line-based text commands over TCP.
// ─────────────────────────────────────────────────────────────────────────────

/// Starts a simple TCP server that accepts task commands.
///
/// ## Protocol
///
/// One command per line (newline-terminated):
/// - `ADD <title>` → insert a task, returns UUID
/// - `GET <uuid>` → retrieve a task
/// - `LIST` → list all tasks
/// - `DONE <uuid>` → mark task as done
/// - `DELETE <uuid>` → delete a task
/// - `STATS` → count total/done/pending
/// - `QUIT` → close connection
///
/// ## Concurrency model
///
/// Each client connection is handled in a separate thread.
/// All threads share the same `SharedDb` handle.
/// The SWMR model ensures:
/// - Multiple LIST/GET clients don't block each other
/// - ADD/DONE/DELETE acquire exclusive access
pub fn run_server(db_path: &Path, addr: &str) -> Result<(), String> {
    let db = SharedDb::open(db_path).map_err(|e| format!("Failed to open: {e}"))?;

    let listener = TcpListener::bind(addr).map_err(|e| format!("Bind failed: {e}"))?;
    println!("TaskMan server listening on {addr}");
    println!("Connect with: nc {addr}");
    println!("Commands: ADD <title> | GET <uuid> | LIST | DONE <uuid> | DELETE <uuid> | STATS | QUIT");

    for stream in listener.incoming() {
        let stream = stream.map_err(|e| format!("Accept failed: {e}"))?;
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        println!("Client connected: {peer}");

        // Clone the SharedDb handle for this connection's thread.
        // This is the standard concurrency pattern with SharedDb.
        let db = db.clone();

        std::thread::spawn(move || {
            if let Err(e) = handle_client(stream, &db) {
                eprintln!("Client {peer} error: {e}");
            }
            println!("Client disconnected: {peer}");
        });
    }

    Ok(())
}

/// Handles a single client connection.
fn handle_client(
    stream: std::net::TcpStream,
    db: &SharedDb,
) -> Result<(), String> {
    let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream;

    for line in reader.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        let cmd = parts[0].to_uppercase();
        let arg = parts.get(1).copied().unwrap_or("");

        let response = match cmd.as_str() {
            "ADD" => handle_add(db, arg),
            "GET" => handle_get(db, arg),
            "LIST" => handle_list(db),
            "DONE" => handle_done(db, arg),
            "DELETE" => handle_delete(db, arg),
            "STATS" => handle_stats(db),
            "QUIT" => {
                let _ = writer.write_all(b"BYE\n");
                return Ok(());
            }
            _ => format!("ERR unknown command: {cmd}\n"),
        };

        writer.write_all(response.as_bytes()).map_err(|e| e.to_string())?;
    }

    Ok(())
}

fn handle_add(db: &SharedDb, title: &str) -> String {
    if title.is_empty() {
        return "ERR missing title\n".to_string();
    }
    let task = Task::new(title, None, vec![]);
    let id = task.id;
    match db.insert(id, task.to_value()) {
        Ok(()) => format!("OK {id}\n"),
        Err(e) => format!("ERR {e}\n"),
    }
}

fn handle_get(db: &SharedDb, id_str: &str) -> String {
    let Ok(id) = Uuid::parse_str(id_str) else {
        return format!("ERR invalid UUID: {id_str}\n");
    };
    match db.get(&id) {
        Ok(Some(value)) => {
            if let Some(task) = Task::from_value(id, &value) {
                format!("OK {task}\n")
            } else {
                "ERR malformed task\n".to_string()
            }
        }
        Ok(None) => "ERR not found\n".to_string(),
        Err(e) => format!("ERR {e}\n"),
    }
}

fn handle_list(db: &SharedDb) -> String {
    match db.scan(..) {
        Ok(entries) => {
            let mut out = format!("OK {} tasks\n", entries.len());
            for (key, value) in &entries {
                if let Some(task) = Task::from_value(*key, value) {
                    out.push_str(&format!("  {task}\n"));
                }
            }
            out
        }
        Err(e) => format!("ERR {e}\n"),
    }
}

fn handle_done(db: &SharedDb, id_str: &str) -> String {
    let Ok(id) = Uuid::parse_str(id_str) else {
        return format!("ERR invalid UUID: {id_str}\n");
    };
    // Read-modify-write with SharedDb: each step acquires the lock separately.
    // Between get() and update(), another thread could modify the data.
    // For a task manager this is fine. For stricter guarantees, you'd need
    // application-level locking or a compare-and-swap pattern.
    match db.get(&id) {
        Ok(Some(value)) => {
            if let Some(mut task) = Task::from_value(id, &value) {
                task.done = true;
                match db.update(&id, task.to_value()) {
                    Ok(()) => "OK done\n".to_string(),
                    Err(e) => format!("ERR {e}\n"),
                }
            } else {
                "ERR malformed task\n".to_string()
            }
        }
        Ok(None) => "ERR not found\n".to_string(),
        Err(e) => format!("ERR {e}\n"),
    }
}

fn handle_delete(db: &SharedDb, id_str: &str) -> String {
    let Ok(id) = Uuid::parse_str(id_str) else {
        return format!("ERR invalid UUID: {id_str}\n");
    };
    match db.delete(&id) {
        Ok(()) => "OK deleted\n".to_string(),
        Err(e) => format!("ERR {e}\n"),
    }
}

fn handle_stats(db: &SharedDb) -> String {
    match db.scan(..) {
        Ok(entries) => {
            let total = entries.len();
            let done = entries
                .iter()
                .filter(|(k, v)| Task::from_value(*k, v).is_some_and(|t| t.done))
                .count();
            format!("OK total={total} done={done} pending={}\n", total - done)
        }
        Err(e) => format!("ERR {e}\n"),
    }
}
