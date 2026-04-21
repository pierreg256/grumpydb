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
//! task.rs     → Task struct + Value conversions
//! store.rs    → TaskStore wrapper around GrumpyDb
//! ```

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
            println!("  Status:      {}", if task.done { "Done" } else { "Pending" });
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
    help                                         Show this help

EXAMPLES:
    cargo run --example taskman -- add "Buy groceries" --tags shopping
    cargo run --example taskman -- add "Write report" --desc "Q1 summary" --tags work,urgent
    cargo run --example taskman -- list
    cargo run --example taskman -- list --pending
    cargo run --example taskman -- done a3b4c5d6
    cargo run --example taskman -- stats

DATA:
    Tasks are stored in .taskman/ in the current directory.
    Files: data.db (documents), index.db (B+Tree index)
"#
    );
}
