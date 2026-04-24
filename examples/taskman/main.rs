//! # TaskMan — A simple task manager powered by GrumpyDB
//!
//! This is a **fully documented example application** that demonstrates how to
//! use GrumpyDB as a storage engine for a real application.
//!
//! ## Purpose
//!
//! TaskMan is a CLI task manager that stores tasks in a GrumpyDB database.
//! It showcases:
//! - Opening/creating a database
//! - CRUD operations (Create, Read, Update, Delete)
//! - Range scans and filtering
//! - Data model conversion (Rust structs ↔ GrumpyDB Values)
//! - Error handling patterns
//!
//! ## How to run
//!
//! ```bash
//! # Add a task
//! cargo run --example taskman -- add "Buy groceries" --desc "Milk, bread, eggs" --tags shopping,food
//!
//! # List all tasks
//! cargo run --example taskman -- list
//!
//! # Mark a task as done (use the short ID shown in `list`)
//! cargo run --example taskman -- done <id>
//!
//! # Show task details
//! cargo run --example taskman -- show <id>
//!
//! # Delete a task
//! cargo run --example taskman -- delete <id>
//!
//! # Show statistics
//! cargo run --example taskman -- stats
//! ```
//!
//! ## Architecture
//!
//! ```text
//! main.rs     → CLI parsing + command dispatch
//! task.rs        → Task struct + Value conversions
//! store.rs       → TaskStore wrapper around GrumpyDb
//! concurrent.rs  → SharedDb wrapper for multi-threaded access (Phase 7b)
//! ```

mod concurrent;
mod store;
mod task;

use std::env;
use std::path::PathBuf;

use store::TaskStore;
use task::Task;

// ─────────────────────────────────────────────────────────────────────────────
// Database location: tasks are stored in a `.taskman` directory
// in the current working directory. This is where GrumpyDB creates
// its data.db and index.db files.
// ─────────────────────────────────────────────────────────────────────────────
fn db_path() -> PathBuf {
    PathBuf::from(".taskman")
}

fn main() {
    let args: Vec<String> = env::args().collect();

    // The first argument after the binary name is the subcommand.
    if args.len() < 2 {
        print_help();
        return;
    }

    let command = args[1].as_str();

    // Dispatch to the appropriate handler.
    // Each handler opens the database, performs its operation, and closes it.
    let result = match command {
        "add" => cmd_add(&args[2..]),
        "list" => cmd_list(&args[2..]),
        "done" => cmd_set_status(&args[2..], true),
        "undone" => cmd_set_status(&args[2..], false),
        "show" => cmd_show(&args[2..]),
        "delete" => cmd_delete(&args[2..]),
        "stats" => cmd_stats(),
        "export" => cmd_export(&args[2..]),
        "import" => cmd_import(&args[2..]),
        "flush" => cmd_flush(),
        "compact" => cmd_compact(),
        "count" => cmd_count(),
        "bench" => cmd_bench(&args[2..]),
        "serve" => cmd_serve(&args[2..]),
        "generate" => cmd_generate(&args[2..]),
        "search" => cmd_search(&args[2..]),
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        _ => {
            eprintln!("Unknown command: {command}");
            print_help();
            Err("unknown command".to_string())
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: add
//
// Usage: taskman add "Task title" [--desc "Description"] [--tags tag1,tag2]
//
// Demonstrates: GrumpyDb::insert()
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_add(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("Usage: taskman add \"Task title\" [--desc \"...\"] [--tags t1,t2]".into());
    }

    let title = &args[0];
    let mut description: Option<&str> = None;
    let mut tags: Vec<&str> = Vec::new();

    // Simple argument parsing without external dependencies.
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--desc" | "-d" => {
                i += 1;
                if i < args.len() {
                    description = Some(&args[i]);
                }
            }
            "--tags" | "-t" => {
                i += 1;
                if i < args.len() {
                    tags = args[i].split(',').collect();
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Create the task with a generated UUID and current timestamp.
    let task = Task::new(title, description, tags);

    // Open the database, insert the task, close it.
    // GrumpyDb::open() creates the directory and files if they don't exist.
    let mut store = TaskStore::open(&db_path())?;
    let id = store.add_task(task)?;
    store.close()?;

    println!("Created task {}", &id.to_string()[..8]);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: list
//
// Usage: taskman list [--done | --pending]
//
// Demonstrates: GrumpyDb::scan(..) + filtering
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_list(args: &[String]) -> Result<(), String> {
    let mut store = TaskStore::open(&db_path())?;

    let tasks = if args.first().map(|s| s.as_str()) == Some("--done") {
        store.list_by_status(true)?
    } else if args.first().map(|s| s.as_str()) == Some("--pending") {
        store.list_by_status(false)?
    } else {
        store.list_all_tasks()?
    };

    if tasks.is_empty() {
        println!("No tasks found. Add one with: taskman add \"My task\"");
    } else {
        println!("Tasks ({} total):", tasks.len());
        println!("{}", "-".repeat(60));
        for task in &tasks {
            // Task implements Display, which formats it nicely.
            println!("  {task}");
        }
    }

    store.close()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: done / undone
//
// Usage: taskman done <id>
//        taskman undone <id>
//
// Demonstrates: read-modify-write pattern (get → modify → update)
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_set_status(args: &[String], done: bool) -> Result<(), String> {
    if args.is_empty() {
        return Err("Usage: taskman done <task-id>".into());
    }

    let id = parse_task_id(&args[0])?;
    let mut store = TaskStore::open(&db_path())?;

    // This demonstrates the read-modify-write pattern:
    // 1. Read the task (get)
    // 2. Modify the `done` field
    // 3. Write it back (update = full replacement)
    store.set_task_done(&id, done)?;
    store.close()?;

    let status = if done { "done" } else { "pending" };
    println!("Task {} marked as {status}", &args[0]);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: show
//
// Usage: taskman show <id>
//
// Demonstrates: GrumpyDb::get() with full detail display
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_show(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("Usage: taskman show <task-id>".into());
    }

    let id = parse_task_id(&args[0])?;
    let mut store = TaskStore::open(&db_path())?;

    // db.get() returns None if the key doesn't exist — not an error.
    match store.get_task(&id)? {
        Some(task) => {
            println!("Task Details");
            println!("{}", "=".repeat(40));
            println!("  ID:          {}", task.id);
            println!("  Title:       {}", task.title);
            println!(
                "  Description: {}",
                task.description.as_deref().unwrap_or("(none)")
            );
            println!(
                "  Status:      {}",
                if task.done { "Done" } else { "Pending" }
            );
            println!("  Created:     {}", format_timestamp(task.created_at));
            if !task.tags.is_empty() {
                println!("  Tags:        {}", task.tags.join(", "));
            }
        }
        None => {
            println!("Task {} not found", &args[0]);
        }
    }

    store.close()?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: delete
//
// Usage: taskman delete <id>
//
// Demonstrates: GrumpyDb::delete()
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_delete(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("Usage: taskman delete <task-id>".into());
    }

    let id = parse_task_id(&args[0])?;
    let mut store = TaskStore::open(&db_path())?;

    // db.delete() removes the document and its index entry.
    // Returns KeyNotFound if the key doesn't exist.
    store.delete_task(&id)?;
    store.close()?;

    println!("Task {} deleted", &args[0]);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: stats
//
// Usage: taskman stats
//
// Demonstrates: scan(..) + aggregation
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_stats() -> Result<(), String> {
    let mut store = TaskStore::open(&db_path())?;

    // stats() internally calls scan(..) to get all tasks, then counts them.
    // This is a full table scan — O(n) — but simple and correct for small datasets.
    let (total, done, pending) = store.stats()?;
    store.close()?;

    println!("Task Statistics");
    println!("{}", "=".repeat(30));
    println!("  Total:   {total}");
    println!("  Done:    {done}");
    println!("  Pending: {pending}");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: compact
//
// Usage: taskman compact
//
// Demonstrates: GrumpyDb::compact()
// Defragments data pages and rebuilds the B+Tree index to reclaim space.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_compact() -> Result<(), String> {
    let mut store = TaskStore::open(&db_path())?;

    let start = std::time::Instant::now();
    let count = store.compact()?;
    let elapsed = start.elapsed();

    store.close()?;

    println!("Compaction complete in {elapsed:.2?}");
    println!("  Documents preserved: {count}");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: count
//
// Usage: taskman count
//
// Demonstrates: GrumpyDb::document_count()
// Returns the number of documents via B+Tree metadata (O(1), no scan needed).
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_count() -> Result<(), String> {
    let mut store = TaskStore::open(&db_path())?;
    let count = store.document_count()?;
    store.close()?;
    println!("{count} documents");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: bench
//
// Usage: taskman bench [--writers N] [--readers N] [--count N]
//
// Demonstrates: SharedDb concurrent access from multiple threads.
// Uses a temporary database to avoid polluting the main task store.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_bench(args: &[String]) -> Result<(), String> {
    let mut writers = 2;
    let mut readers = 4;
    let mut count = 1000;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--writers" | "-w" => {
                i += 1;
                if i < args.len() {
                    writers = args[i].parse().unwrap_or(2);
                }
            }
            "--readers" | "-r" => {
                i += 1;
                if i < args.len() {
                    readers = args[i].parse().unwrap_or(4);
                }
            }
            "--count" | "-n" => {
                i += 1;
                if i < args.len() {
                    count = args[i].parse().unwrap_or(1000);
                }
            }
            _ => {}
        }
        i += 1;
    }

    // Use a temporary directory for benchmarks (don't pollute the real task store).
    let bench_dir = std::env::temp_dir().join("taskman_bench");
    let _ = std::fs::remove_dir_all(&bench_dir);
    let result = concurrent::run_bench(&bench_dir, writers, readers, count);
    let _ = std::fs::remove_dir_all(&bench_dir);
    result
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: serve
//
// Usage: taskman serve [--port PORT]
//
// Demonstrates: SharedDb shared across client connection threads.
// Each TCP client gets its own thread with a cloned SharedDb handle.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_serve(args: &[String]) -> Result<(), String> {
    let mut port = "8080";
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--port" || args[i] == "-p" {
            i += 1;
            if i < args.len() {
                port = &args[i];
            }
        }
        i += 1;
    }
    let addr = format!("127.0.0.1:{port}");
    concurrent::run_server(&db_path(), &addr)
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: generate
//
// Usage: taskman generate --count N
//
// Demonstrates: bulk insert performance.
// Generates N synthetic tasks with predictable data for benchmarking.
// Shows ops/sec throughput — useful for measuring buffer pool impact.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_generate(args: &[String]) -> Result<(), String> {
    let mut count = 1000usize;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--count" || args[i] == "-n" {
            i += 1;
            if i < args.len() {
                count = args[i].parse().unwrap_or(1000);
            }
        }
        i += 1;
    }

    let mut store = TaskStore::open(&db_path())?;

    // Measure insert throughput.
    // Each insert: encode document → store in slotted page → index in B+Tree → WAL commit.
    // With a buffer pool, repeated page access is served from cache → fewer disk reads.
    let start = std::time::Instant::now();
    let tags_pool = [
        "work",
        "personal",
        "urgent",
        "low-priority",
        "meeting",
        "errand",
    ];

    for i in 0..count {
        let tag_idx = i % tags_pool.len();
        let task = Task::new(
            format!("Generated task #{i}"),
            Some(&format!("Auto-generated task for benchmarking (batch {i})")),
            vec![tags_pool[tag_idx]],
        );
        store
            .add_task(task)
            .map_err(|e| format!("Insert {i} failed: {e}"))?;

        // Progress indicator every 1000 tasks
        if (i + 1) % 1000 == 0 {
            print!("\r  Generated {}/{count}...", i + 1);
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }

    let elapsed = start.elapsed();
    let ops_sec = count as f64 / elapsed.as_secs_f64();

    store.close()?;

    println!("\r  Generated {count} tasks in {elapsed:.2?} ({ops_sec:.0} ops/sec)");
    println!("  Use 'taskman search --tag urgent' to test scan+filter performance.");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: search
//
// Usage: taskman search --tag <tag>
//
// Demonstrates: scan(..) + filter performance.
// Scans ALL documents (O(n)) and filters by tag.
// With a buffer pool, the B+Tree traversal and page reads hit the cache
// on repeated scans → significantly faster than hitting disk every time.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_search(args: &[String]) -> Result<(), String> {
    let mut tag_filter: Option<&str> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--tag" || args[i] == "-t" {
            i += 1;
            if i < args.len() {
                tag_filter = Some(&args[i]);
            }
        }
        i += 1;
    }

    let Some(tag) = tag_filter else {
        return Err("Usage: taskman search --tag <tag>".into());
    };

    let mut store = TaskStore::open(&db_path())?;

    // Measure scan + filter time.
    // This is a full table scan — O(n) — because GrumpyDB is a key-value store.
    // The buffer pool helps by caching B+Tree pages and data pages during the scan.
    let start = std::time::Instant::now();
    let all_tasks = store.list_all_tasks()?;
    let scan_time = start.elapsed();

    let start_filter = std::time::Instant::now();
    let matching: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| t.tags.iter().any(|t_tag| t_tag == tag))
        .collect();
    let filter_time = start_filter.elapsed();

    store.close()?;

    println!("Search results for tag '{tag}':");
    println!("{}", "-".repeat(50));
    if matching.is_empty() {
        println!("  No tasks found with tag '{tag}'");
    } else {
        for task in &matching {
            println!("  {task}");
        }
    }
    println!();
    println!("Performance:");
    println!(
        "  Scanned:  {} documents in {scan_time:.2?}",
        all_tasks.len()
    );
    println!(
        "  Filtered: {} matches in {filter_time:.2?}",
        matching.len()
    );
    println!("  Total:    {:.2?}", scan_time + filter_time);
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: export
//
// Usage: taskman export [file]
//
// Demonstrates: scan(..) for full data export
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_export(args: &[String]) -> Result<(), String> {
    let mut store = TaskStore::open(&db_path())?;
    let data = store.export_tasks()?;
    store.close()?;

    if let Some(file_path) = args.first() {
        std::fs::write(file_path, &data).map_err(|e| format!("Failed to write file: {e}"))?;
        println!("Exported to {file_path}");
    } else {
        print!("{data}");
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: import
//
// Usage: taskman import <file>
//
// Demonstrates: batch insert with crash safety
//
// Each task is inserted as a separate WAL transaction. If the process crashes
// mid-import, already-committed tasks are safe. The partially-written last
// task is rolled back automatically by WAL recovery on next open.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_import(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("Usage: taskman import <file>".into());
    }

    let data =
        std::fs::read_to_string(&args[0]).map_err(|e| format!("Failed to read file: {e}"))?;

    let mut store = TaskStore::open(&db_path())?;
    let count = store.import_tasks(&data)?;
    store.close()?;

    println!("Imported {count} tasks");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// COMMAND: flush
//
// Usage: taskman flush
//
// Demonstrates: explicit flush + WAL checkpoint.
// After this, all data is guaranteed durable on disk, and the WAL is truncated.
// ─────────────────────────────────────────────────────────────────────────────
fn cmd_flush() -> Result<(), String> {
    let mut store = TaskStore::open(&db_path())?;
    store.flush()?;
    store.close()?;
    println!("Database flushed and WAL checkpointed");
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// HELPERS
// ─────────────────────────────────────────────────────────────────────────────

/// Parses a task ID from a string. Accepts both full UUIDs and short (8-char) prefixes.
///
/// For short prefixes, we scan the database to find a matching task.
/// This is a convenience feature — full UUIDs always work.
fn parse_task_id(input: &str) -> Result<uuid::Uuid, String> {
    // Try parsing as a full UUID first.
    if let Ok(id) = uuid::Uuid::parse_str(input) {
        return Ok(id);
    }

    // If it's a short prefix (e.g., "a3b4c5d6"), scan for a match.
    if input.len() >= 4 {
        let mut store = TaskStore::open(&db_path())?;
        let tasks = store.list_all_tasks()?;
        store.close()?;

        let matches: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.id.to_string().starts_with(input))
            .collect();

        match matches.len() {
            0 => return Err(format!("No task found matching '{input}'")),
            1 => return Ok(matches[0].id),
            n => return Err(format!("{n} tasks match '{input}' — use more characters")),
        }
    }

    Err(format!("Invalid task ID: '{input}'"))
}

/// Formats a Unix timestamp as a human-readable date string.
fn format_timestamp(ts: i64) -> String {
    // Simple formatting without external crate.
    // In production, use `chrono` or `time` for proper formatting.
    let secs = ts as u64;
    let days = secs / 86400;
    let years = 1970 + days / 365; // approximate
    let remaining_days = days % 365;
    let months = remaining_days / 30 + 1;
    let day = remaining_days % 30 + 1;
    format!("{years:04}-{months:02}-{day:02}")
}

/// Prints the help message with usage examples.
fn print_help() {
    println!(
        r#"TaskMan — A task manager powered by GrumpyDB

USAGE:
    cargo run --example taskman -- <COMMAND> [OPTIONS]

COMMANDS:
    add <title> [--desc "..."] [--tags t1,t2]   Add a new task
    list [--done | --pending]                    List tasks
    show <id>                                    Show task details
    done <id>                                    Mark task as done
    undone <id>                                  Mark task as pending
    delete <id>                                  Delete a task
    stats                                        Show task statistics
    export [file]                                Export tasks (to stdout or file)
    import <file>                                Import tasks from file
    flush                                        Flush data + WAL checkpoint
    compact                                      Defragment data + rebuild index
    count                                        Document count (O(1), no scan)
    generate [--count N]                         Generate N synthetic tasks (with perf stats)
    search --tag <tag>                           Search tasks by tag (with perf stats)
    bench [--writers N] [--readers N] [--count N] Concurrent benchmark
    serve [--port PORT]                          Start TCP server (multi-client)
    help                                         Show this help

EXAMPLES:
    cargo run --example taskman -- add "Buy groceries" --tags shopping
    cargo run --example taskman -- list
    cargo run --example taskman -- done a3b4c5d6
    cargo run --example taskman -- generate --count 5000
    cargo run --example taskman -- search --tag urgent
    cargo run --example taskman -- bench --writers 4 --count 5000
    cargo run --example taskman -- flush
    cargo run --example taskman -- stats

DATA:
    Tasks are stored in .taskman/ in the current directory.
    Files: data.db (documents), index.db (B+Tree index), wal.log (Write-Ahead Log)

PERFORMANCE:
    GrumpyDB uses a buffer pool (LRU page cache, 256 frames = 2 MiB) to reduce
    disk I/O. The 'generate' and 'search' commands show buffer pool stats:
    - reads:  disk reads (cache misses)
    - writes: disk writes (dirty page flushes)
    - cached: pages currently in the pool
    Use 'generate --count 50000' then 'search --tag urgent' to see the cache in action.

CRASH SAFETY:
    Every write is protected by the Write-Ahead Log (WAL).
    If the process crashes, committed data is recovered automatically on next open.
    Use 'flush' to force a checkpoint and truncate the WAL.
"#
    );
}
