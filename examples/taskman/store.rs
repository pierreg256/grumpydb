//! # TaskStore — GrumpyDB storage layer for tasks
//!
//! This module wraps [`GrumpyDb`] to provide a typed API for storing and
//! retrieving [`Task`] objects. It demonstrates the recommended pattern
//! for building a domain-specific storage layer on top of GrumpyDB.
//!
//! ## Pattern: typed wrapper around GrumpyDb
//!
//! ```text
//! Application code
//!   → TaskStore (typed API: add_task, get_task, ...)
//!     → GrumpyDb (generic API: insert, get, scan, ...)
//!       → Disk (data.db + index.db)
//! ```
//!
//! This separation keeps your application code clean: it works with `Task`
//! structs, not raw `Value` types. The conversion happens in one place.

use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// GrumpyDB imports:
// - `GrumpyDb` is the main storage engine (open, insert, get, delete, scan)
// - `GrumpyError` is the unified error type
// - `Value` is the schema-less document type
// ─────────────────────────────────────────────────────────────────────────────
use grumpydb::GrumpyDb;

use uuid::Uuid;

use super::task::Task;

/// A typed storage layer for tasks, backed by GrumpyDB.
///
/// Wraps a [`GrumpyDb`] instance and provides Task-specific operations.
/// All operations translate between `Task` structs and GrumpyDB `Value` types.
///
/// # Example
///
/// ```no_run
/// use std::path::Path;
/// let mut store = TaskStore::open(Path::new("./my_tasks")).unwrap();
/// let task = Task::new("Buy milk", None, vec![]);
/// let id = store.add_task(task).unwrap();
/// ```
pub struct TaskStore {
    /// The underlying GrumpyDB instance.
    /// We need `mut` access because all GrumpyDB operations require `&mut self`
    /// (the engine manages internal state like page caches and B+Tree cursors).
    db: GrumpyDb,
}

impl TaskStore {
    /// Opens or creates a task store at the given directory path.
    ///
    /// This calls [`GrumpyDb::open()`], which:
    /// - Creates the directory if it doesn't exist
    /// - Creates `data.db` (page-based document storage) if new
    /// - Creates `index.db` (B+Tree index) if new
    /// - Opens existing files if they already exist
    ///
    /// # Arguments
    ///
    /// * `path` — Directory where database files will be stored
    ///
    /// # Errors
    ///
    /// Returns an error if the directory can't be created or files can't be opened.
    pub fn open(path: &Path) -> Result<Self, String> {
        // GrumpyDb::open() is the entry point to the storage engine.
        // It takes a directory path — all database files are created inside it.
        let db = GrumpyDb::open(path).map_err(|e| format!("Failed to open database: {e}"))?;
        Ok(Self { db })
    }

    /// Adds a new task to the store. Returns the task's UUID.
    ///
    /// Demonstrates: [`GrumpyDb::insert()`]
    ///
    /// ## How it works
    ///
    /// 1. Convert the Task to a `Value::Object` (serialization)
    /// 2. Call `db.insert(key, value)` — this:
    ///    - Encodes the value to binary (compact format)
    ///    - Stores it in a slotted page (or overflow pages if > 8 KiB)
    ///    - Adds the key to the B+Tree index for O(log n) lookups
    /// 3. Return the UUID so the caller can reference this task later
    pub fn add_task(&mut self, task: Task) -> Result<Uuid, String> {
        let id = task.id;

        // Convert our Task struct to a GrumpyDB Value.
        // GrumpyDB is schema-less — it stores Value types, not Rust structs.
        let value = task.to_value();

        // Insert into the database.
        // - `id` (Uuid) is the document key — used for lookups via B+Tree
        // - `value` (Value) is the document body — stored in page-based storage
        self.db
            .insert(id, value)
            .map_err(|e| format!("Failed to add task: {e}"))?;

        Ok(id)
    }

    /// Retrieves a task by its UUID.
    ///
    /// Demonstrates: [`GrumpyDb::get()`]
    ///
    /// ## How it works
    ///
    /// 1. Call `db.get(&key)` — this:
    ///    - Searches the B+Tree index for the key (O(log n))
    ///    - Reads the page + slot pointed to by the index
    ///    - Decodes the binary data back to a `Value`
    /// 2. Convert the `Value` back to a `Task` struct
    /// 3. Returns `None` if the key doesn't exist (not an error)
    pub fn get_task(&mut self, id: &Uuid) -> Result<Option<Task>, String> {
        // db.get() returns Option<Value>:
        // - Some(value) if the key exists
        // - None if the key doesn't exist (this is NOT an error)
        let value = self
            .db
            .get(id)
            .map_err(|e| format!("Failed to get task: {e}"))?;

        // Convert Value back to Task.
        // We pass the UUID because it's stored separately (as the B+Tree key),
        // not inside the document value.
        Ok(value.and_then(|v| Task::from_value(*id, &v)))
    }

    /// Updates an existing task (full replacement).
    ///
    /// Demonstrates: [`GrumpyDb::update()`]
    ///
    /// ## How it works
    ///
    /// GrumpyDB's update is a **full replacement** — the entire document is replaced.
    /// There's no partial/field-level update. The pattern is:
    /// 1. (Optional) Read the current task with `get()`
    /// 2. Modify the fields you want
    /// 3. Call `update()` with the new value
    ///
    /// ## Errors
    ///
    /// Returns an error if the key doesn't exist (KeyNotFound).
    pub fn update_task(&mut self, task: &Task) -> Result<(), String> {
        let value = task.to_value();

        // db.update() replaces the entire document at the given key.
        // Internally it does: delete old + insert new.
        // If the key doesn't exist, it returns GrumpyError::KeyNotFound.
        self.db
            .update(&task.id, value)
            .map_err(|e| format!("Failed to update task: {e}"))
    }

    /// Marks a task as done or not done.
    ///
    /// This is a **read-modify-write** pattern:
    /// 1. Read the current task
    /// 2. Modify the `done` field
    /// 3. Write it back
    ///
    /// This is the standard way to do partial updates with GrumpyDB.
    pub fn set_task_done(&mut self, id: &Uuid, done: bool) -> Result<(), String> {
        // Step 1: Read the current task
        let mut task = self
            .get_task(id)?
            .ok_or_else(|| format!("Task {id} not found"))?;

        // Step 2: Modify the field
        task.done = done;

        // Step 3: Write it back (full replacement)
        self.update_task(&task)
    }

    /// Deletes a task by its UUID.
    ///
    /// Demonstrates: [`GrumpyDb::delete()`]
    ///
    /// ## How it works
    ///
    /// 1. Call `db.delete(&key)` — this:
    ///    - Finds the document via B+Tree
    ///    - Removes it from the slotted page (marks slot as tombstone)
    ///    - Frees overflow pages if the document was large
    ///    - Removes the key from the B+Tree index
    /// 2. The space is reclaimed for future inserts
    ///
    /// ## Errors
    ///
    /// Returns an error if the key doesn't exist (KeyNotFound).
    pub fn delete_task(&mut self, id: &Uuid) -> Result<(), String> {
        self.db
            .delete(id)
            .map_err(|e| format!("Failed to delete task: {e}"))
    }

    /// Lists all tasks in the store.
    ///
    /// Demonstrates: [`GrumpyDb::scan()`] with an unbounded range (`..`)
    ///
    /// ## How it works
    ///
    /// `scan(..)` iterates over ALL documents in key order:
    /// 1. Opens a B+Tree cursor at the first leaf
    /// 2. Follows the leaf linked-list to visit every entry
    /// 3. For each entry, reads the page + slot and decodes the document
    ///
    /// The results are sorted by UUID (lexicographic byte order).
    pub fn list_all_tasks(&mut self) -> Result<Vec<Task>, String> {
        // scan(..) with the unbounded range `..` returns ALL documents.
        // The range parameter accepts any std::ops::RangeBounds<Uuid>.
        let entries = self
            .db
            .scan(..)
            .map_err(|e| format!("Failed to list tasks: {e}"))?;

        // Convert each (Uuid, Value) pair back to a Task.
        // We use filter_map to skip any documents that can't be parsed
        // (e.g., if the schema changed between versions).
        Ok(entries
            .iter()
            .filter_map(|(key, value)| Task::from_value(*key, value))
            .collect())
    }

    /// Lists tasks filtered by completion status.
    ///
    /// Demonstrates: scan + application-level filtering.
    ///
    /// ## Note on filtering
    ///
    /// GrumpyDB doesn't support queries like "WHERE done = true" — it's a
    /// key-value store, not a SQL database. Filtering happens in your code
    /// after scanning. For small datasets (< 100K docs), this is perfectly fine.
    /// For larger datasets, consider maintaining a secondary index.
    pub fn list_by_status(&mut self, done: bool) -> Result<Vec<Task>, String> {
        // First, get all tasks...
        let all = self.list_all_tasks()?;

        // ...then filter in application code.
        // This is a full scan — O(n) — but simple and correct.
        Ok(all.into_iter().filter(|t| t.done == done).collect())
    }

    /// Returns statistics about the task store.
    ///
    /// Demonstrates: scan + aggregation.
    pub fn stats(&mut self) -> Result<(usize, usize, usize), String> {
        let all = self.list_all_tasks()?;
        let total = all.len();
        let done = all.iter().filter(|t| t.done).count();
        let pending = total - done;
        Ok((total, done, pending))
    }

    /// Flushes all data to disk.
    ///
    /// Demonstrates: [`GrumpyDb::flush()`]
    ///
    /// Call this to ensure all data is written to disk before exiting.
    /// Without flush, data may be buffered in memory.
    ///
    /// ## What flush does (with WAL)
    ///
    /// 1. Syncs `data.db` and `index.db` to disk (fsync)
    /// 2. Writes a WAL checkpoint record
    /// 3. Truncates the WAL file (no longer needed after checkpoint)
    ///
    /// After flush(), even a power failure won't lose data.
    pub fn flush(&mut self) -> Result<(), String> {
        self.db.flush().map_err(|e| format!("Failed to flush: {e}"))
    }

    /// Closes the task store, flushing all data.
    ///
    /// Demonstrates: [`GrumpyDb::close()`]
    pub fn close(self) -> Result<(), String> {
        self.db
            .close()
            .map_err(|e| format!("Failed to close database: {e}"))
    }

    /// Returns buffer pool statistics: `(reads, writes, cached_pages, capacity)`.
    ///
    /// Demonstrates: [`GrumpyDb::pool_stats()`]
    ///
    /// Useful for understanding caching behavior:
    /// - `reads`: number of disk reads (cache misses)
    /// - `writes`: number of disk writes (dirty page flushes)
    /// - `cached_pages`: pages currently in the buffer pool
    /// - `capacity`: maximum number of pages the pool can hold
    pub fn pool_stats(&self) -> (u64, u64, usize, usize) {
        self.db.pool_stats()
    }

    /// Compacts the database: defragments data pages and rebuilds the index.
    ///
    /// Demonstrates: [`GrumpyDb::compact()`]
    ///
    /// Use after many deletes to reclaim disk space.
    pub fn compact(&mut self) -> Result<grumpydb::CompactResult, String> {
        self.db
            .compact()
            .map_err(|e| format!("Failed to compact: {e}"))
    }

    /// Returns the number of documents (O(1) via B+Tree metadata).
    ///
    /// Demonstrates: [`GrumpyDb::document_count()`]
    pub fn document_count(&self) -> u64 {
        self.db.document_count()
    }

    // ─────────────────────────────────────────────────────────────────────
    // BATCH OPERATIONS (Phase 5b)
    //
    // These demonstrate bulk insert/export patterns with GrumpyDB.
    // In a real application, you'd use these for data migration, backup, etc.
    // ─────────────────────────────────────────────────────────────────────

    /// Exports all tasks as a simple text format (one JSON-like line per task).
    ///
    /// Demonstrates: `scan(..)` for full data export.
    ///
    /// ## Format
    ///
    /// Each line: `UUID|title|description|done|created_at|tag1,tag2,...`
    /// This is a simple pipe-delimited format for portability.
    pub fn export_tasks(&mut self) -> Result<String, String> {
        let tasks = self.list_all_tasks()?;
        let mut output = String::new();
        for task in &tasks {
            let desc = task.description.as_deref().unwrap_or("");
            let tags = task.tags.join(",");
            output.push_str(&format!(
                "{}|{}|{}|{}|{}|{}\n",
                task.id, task.title, desc, task.done, task.created_at, tags
            ));
        }
        Ok(output)
    }

    /// Imports tasks from the pipe-delimited export format.
    ///
    /// Demonstrates: batch `insert()` — each task is inserted individually.
    ///
    /// ## Crash safety
    ///
    /// Each insert is a separate WAL transaction. If the process crashes mid-import:
    /// - Tasks already committed are safe (WAL guarantees durability)
    /// - The partially-written last task is rolled back on recovery
    /// - You can re-run the import for remaining tasks (duplicates are rejected)
    ///
    /// Returns the number of tasks successfully imported.
    pub fn import_tasks(&mut self, data: &str) -> Result<usize, String> {
        let mut count = 0;
        for line in data.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.splitn(6, '|').collect();
            if parts.len() < 5 {
                continue; // Skip malformed lines
            }

            let id = uuid::Uuid::parse_str(parts[0]).map_err(|e| format!("Invalid UUID: {e}"))?;
            let title = parts[1].to_string();
            let description = if parts[2].is_empty() {
                None
            } else {
                Some(parts[2].to_string())
            };
            let done = parts[3] == "true";
            let created_at: i64 = parts[4].parse().unwrap_or(0);
            let tags: Vec<String> = if parts.len() > 5 && !parts[5].is_empty() {
                parts[5].split(',').map(String::from).collect()
            } else {
                Vec::new()
            };

            let task = super::task::Task {
                id,
                title,
                description,
                done,
                created_at,
                tags,
            };

            let value = task.to_value();

            // Insert using the original UUID as key.
            // If the key already exists (duplicate import), skip it.
            match self.db.insert(id, value) {
                Ok(()) => count += 1,
                Err(grumpydb::GrumpyError::DuplicateKey(_)) => {
                    // Already imported — skip silently
                }
                Err(e) => return Err(format!("Import failed: {e}")),
            }
        }
        Ok(count)
    }
}
