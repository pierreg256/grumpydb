# TaskMan — A Task Manager Powered by GrumpyDB

A fully documented example application that demonstrates how to use GrumpyDB
as a storage engine for a real-world CLI application.

## Quick Start

```bash
# Add tasks
cargo run --example taskman -- add "Buy groceries" --tags shopping,food
cargo run --example taskman -- add "Write report" --desc "Q1 summary" --tags work

# List tasks
cargo run --example taskman -- list
cargo run --example taskman -- list --pending
cargo run --example taskman -- list --done

# Manage tasks
cargo run --example taskman -- done <id>          # Mark as done
cargo run --example taskman -- undone <id>        # Mark as pending
cargo run --example taskman -- show <id>          # Show details
cargo run --example taskman -- delete <id>        # Delete

# Data operations
cargo run --example taskman -- export tasks.bak   # Export to file
cargo run --example taskman -- import tasks.bak   # Import from file
cargo run --example taskman -- flush              # Force WAL checkpoint
cargo run --example taskman -- stats              # Show statistics
```

Task IDs can be shortened — just use the first 4+ characters (e.g., `a3b4`).

## Architecture

```
examples/taskman/
├── main.rs     CLI parsing + command dispatch
├── task.rs     Task struct + Value conversions (data model)
└── store.rs    TaskStore wrapper around GrumpyDb (storage layer)
```

### Data Flow

```
Task (Rust struct)
  → Task::to_value()    → Value::Object(BTreeMap)
  → GrumpyDb::insert()  → encoded to binary → stored on disk
  → GrumpyDb::get()     → read from disk → decoded to Value
  → Task::from_value()  → Task (Rust struct)
```

## Data Safety

TaskMan uses GrumpyDB's Write-Ahead Log (WAL) for crash protection.

### How WAL Protects Your Data

```
1. You call: taskman add "Important task"
2. GrumpyDB writes a WAL record (before + after images) → fsync
3. GrumpyDB writes the Commit record → fsync
4. The page is written to data.db (may be lazy)

If the process crashes at step 2: the WAL record exists but no commit
  → On next open, recovery UNDOES the partial write (before-image applied)

If the process crashes at step 4: commit is in WAL, page may not be written
  → On next open, recovery REDOES the committed write (after-image applied)
```

### When to Use `flush`

- `flush` forces all data to disk + writes a WAL checkpoint + truncates the WAL
- Call it when you want to guarantee all data is durable AND reduce WAL size
- Normal operations don't require explicit flush — the WAL commit (fsync) already
  guarantees durability of each individual transaction
- GrumpyDB also auto-checkpoints every 100 writes

### Import Crash Safety

When importing tasks from a file:
- Each task is inserted as a **separate WAL transaction**
- If the process crashes mid-import:
  - Tasks already committed are **safe** (WAL durability guarantee)
  - The partially-written last task is **rolled back** on recovery
  - You can re-run the import — duplicates are silently skipped

## GrumpyDB API Patterns Demonstrated

| Pattern | Where | GrumpyDB API |
|---------|-------|-------------|
| Open/create database | `store.rs: TaskStore::open()` | `GrumpyDb::open(path)` |
| Insert document | `store.rs: add_task()` | `db.insert(uuid, value)` |
| Get by key | `store.rs: get_task()` | `db.get(&uuid)` |
| Full replacement update | `store.rs: update_task()` | `db.update(&uuid, value)` |
| Read-modify-write | `store.rs: set_task_done()` | `get → modify → update` |
| Delete | `store.rs: delete_task()` | `db.delete(&uuid)` |
| Scan all documents | `store.rs: list_all_tasks()` | `db.scan(..)` |
| Scan + filter | `store.rs: list_by_status()` | `scan(..)` + `filter()` |
| Scan + aggregation | `store.rs: stats()` | `scan(..)` + `count()` |
| Batch insert | `store.rs: import_tasks()` | Loop of `insert()` |
| Flush + checkpoint | `store.rs: flush()` | `db.flush()` |
| Close database | `store.rs: close()` | `db.close()` |

## Files on Disk

Tasks are stored in `.taskman/` in the current directory:

| File | Purpose |
|------|---------|
| `data.db` | Page-based document storage (8 KiB pages) |
| `index.db` | B+Tree index (UUID → page location) |
| `wal.log` | Write-Ahead Log (crash recovery) |
