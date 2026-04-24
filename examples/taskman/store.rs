//! # TaskStore — GrumpyDB storage layer for tasks
//!
//! This module wraps [`Database`] to provide a typed API for storing and
//! retrieving [`Task`] objects. It demonstrates the recommended pattern
//! for building a domain-specific storage layer on top of GrumpyDB v3.
//!
//! ## Pattern: typed wrapper around Database
//!
//! ```text
//! Application code
//!   → TaskStore (typed API: add_task, get_task, ...)
//!     → Database (multi-collection API: insert, get, scan, ...)
//!       → Disk (data.db + primary.idx + idx_*.idx per collection)
//! ```
//!
//! This separation keeps your application code clean: it works with `Task`
//! structs, not raw `Value` types. The conversion happens in one place.

use std::path::Path;

// ─────────────────────────────────────────────────────────────────────────────
// GrumpyDB imports:
// - `Database` is the multi-collection storage engine
// - `Value` is the schema-less document type
// ─────────────────────────────────────────────────────────────────────────────
use grumpydb::Database;

use uuid::Uuid;

use super::task::Task;

/// The collection name for tasks within the database.
const TASKS_COLLECTION: &str = "tasks";

/// A typed storage layer for tasks, backed by GrumpyDB's Database API.
///
/// Wraps a [`Database`] instance and provides Task-specific operations.
/// All operations translate between `Task` structs and GrumpyDB `Value` types.
/// Uses a secondary index on the `done` field for fast filtering.
pub struct TaskStore {
    db: Database,
}

impl TaskStore {
    /// Opens or creates a task store at the given directory path.
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut db = Database::open(path).map_err(|e| format!("Failed to open database: {e}"))?;

        // Ensure the tasks collection exists
        if !db.list_collections().contains(&TASKS_COLLECTION) {
            db.create_collection(TASKS_COLLECTION)
                .map_err(|e| format!("Failed to create collection: {e}"))?;
        }

        // Ensure the secondary index on "done" exists for fast filtering
        let coll = db
            .collection(TASKS_COLLECTION)
            .map_err(|e| format!("Failed to get collection: {e}"))?;
        let has_done_index = coll.list_indexes().iter().any(|d| d.name == "by_done");
        if !has_done_index {
            drop(coll);
            db.create_index(TASKS_COLLECTION, "by_done", "done")
                .map_err(|e| format!("Failed to create index: {e}"))?;
        }

        Ok(Self { db })
    }

    /// Adds a new task to the store. Returns the task's UUID.
    pub fn add_task(&mut self, task: Task) -> Result<Uuid, String> {
        let id = task.id;
        let value = task.to_value();
        self.db
            .insert(TASKS_COLLECTION, id, value)
            .map_err(|e| format!("Failed to add task: {e}"))?;
        Ok(id)
    }

    /// Retrieves a task by its UUID.
    pub fn get_task(&mut self, id: &Uuid) -> Result<Option<Task>, String> {
        let value = self
            .db
            .get(TASKS_COLLECTION, id)
            .map_err(|e| format!("Failed to get task: {e}"))?;
        Ok(value.and_then(|v| Task::from_value(*id, &v)))
    }

    /// Updates an existing task (full replacement).
    pub fn update_task(&mut self, task: &Task) -> Result<(), String> {
        let value = task.to_value();
        self.db
            .update(TASKS_COLLECTION, &task.id, value)
            .map_err(|e| format!("Failed to update task: {e}"))
    }

    /// Marks a task as done or not done (read-modify-write pattern).
    pub fn set_task_done(&mut self, id: &Uuid, done: bool) -> Result<(), String> {
        let mut task = self
            .get_task(id)?
            .ok_or_else(|| format!("Task {id} not found"))?;
        task.done = done;
        self.update_task(&task)
    }

    /// Deletes a task by its UUID.
    pub fn delete_task(&mut self, id: &Uuid) -> Result<(), String> {
        self.db
            .delete(TASKS_COLLECTION, id)
            .map_err(|e| format!("Failed to delete task: {e}"))
    }

    /// Lists all tasks in the store.
    pub fn list_all_tasks(&mut self) -> Result<Vec<Task>, String> {
        let entries = self
            .db
            .scan(TASKS_COLLECTION, ..)
            .map_err(|e| format!("Failed to list tasks: {e}"))?;
        Ok(entries
            .iter()
            .filter_map(|(key, value)| Task::from_value(*key, value))
            .collect())
    }

    /// Lists tasks filtered by completion status, using the secondary index.
    pub fn list_by_status(&mut self, done: bool) -> Result<Vec<Task>, String> {
        let results = self
            .db
            .query(TASKS_COLLECTION, "by_done", &grumpydb::Value::Bool(done))
            .map_err(|e| format!("Failed to query index: {e}"))?;
        Ok(results
            .into_iter()
            .filter_map(|(key, value)| Task::from_value(key, &value))
            .collect())
    }

    /// Returns statistics about the task store.
    pub fn stats(&mut self) -> Result<(usize, usize, usize), String> {
        let all = self.list_all_tasks()?;
        let total = all.len();
        let done = all.iter().filter(|t| t.done).count();
        let pending = total - done;
        Ok((total, done, pending))
    }

    /// Flushes all data to disk.
    pub fn flush(&mut self) -> Result<(), String> {
        self.db.flush().map_err(|e| format!("Failed to flush: {e}"))
    }

    /// Closes the task store, flushing all data.
    pub fn close(self) -> Result<(), String> {
        self.db
            .close()
            .map_err(|e| format!("Failed to close database: {e}"))
    }

    /// Returns the document count (via primary index metadata).
    pub fn document_count(&mut self) -> Result<u64, String> {
        self.db
            .document_count(TASKS_COLLECTION)
            .map_err(|e| format!("Failed to count: {e}"))
    }

    /// Compacts the tasks collection.
    pub fn compact(&mut self) -> Result<u64, String> {
        self.db
            .compact(TASKS_COLLECTION)
            .map_err(|e| format!("Failed to compact: {e}"))
    }

    // ─────────────────────────────────────────────────────────────────────
    // BATCH OPERATIONS
    // ─────────────────────────────────────────────────────────────────────

    /// Exports all tasks as a pipe-delimited format.
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
            match self.db.insert(TASKS_COLLECTION, id, value) {
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
