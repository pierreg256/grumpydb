//! # TaskMan — A task manager powered by GrumpyDB
//!
//! This module defines the `Task` data model and provides conversions
//! to and from GrumpyDB's [`Value`] type.
//!
//! ## Why this file exists
//!
//! GrumpyDB stores **schema-less documents** — each document is a `Value`
//! (similar to JSON). Your application needs to convert its own types
//! (here, `Task`) to and from `Value`. This module shows the recommended
//! pattern for doing that.
//!
//! ## Data flow
//!
//! ```text
//! Task (Rust struct)
//!   → Task::to_value()    → Value::Object(BTreeMap)
//!   → GrumpyDb::insert()  → stored on disk
//!   → GrumpyDb::get()     → Value::Object(BTreeMap)
//!   → Task::from_value()  → Task (Rust struct)
//! ```

use std::collections::BTreeMap;
use std::fmt;

// ─────────────────────────────────────────────────────────────────────────────
// GrumpyDB imports — these are the types you need from the storage engine.
// `Value` is the schema-less document type (like JSON).
// `GrumpyError` is the unified error type for all storage operations.
// ─────────────────────────────────────────────────────────────────────────────
use grumpydb::Value;

use uuid::Uuid;

/// A task in our task management application.
///
/// This is a plain Rust struct — GrumpyDB doesn't know about it directly.
/// We convert it to/from [`Value`] for storage.
///
/// # Fields
///
/// - `id`: Unique identifier (UUID v4). Used as the document key in GrumpyDB.
/// - `title`: Short description of the task. Required.
/// - `description`: Optional longer description.
/// - `done`: Whether the task is completed.
/// - `created_at`: Unix timestamp (seconds since epoch) when the task was created.
/// - `tags`: List of string tags for categorization.
#[derive(Debug, Clone)]
pub struct Task {
    pub id: Uuid,
    pub title: String,
    pub description: Option<String>,
    pub done: bool,
    pub created_at: i64,
    pub tags: Vec<String>,
}

impl Task {
    /// Creates a new task with a generated UUID and the current timestamp.
    ///
    /// # Arguments
    ///
    /// * `title` — The task title (required)
    /// * `description` — Optional detailed description
    /// * `tags` — List of tags (can be empty)
    ///
    /// # Example
    ///
    /// ```
    /// let task = Task::new("Buy groceries", Some("Milk, bread, eggs"), vec!["shopping"]);
    /// assert!(!task.done);
    /// ```
    pub fn new(title: impl Into<String>, description: Option<&str>, tags: Vec<&str>) -> Self {
        Self {
            // Generate a unique ID for this task.
            // UUIDs are used as document keys in GrumpyDB — each must be unique.
            id: Uuid::new_v4(),

            title: title.into(),
            description: description.map(String::from),

            // New tasks start as not done.
            done: false,

            // Store the creation time as a Unix timestamp.
            // We use i64 because GrumpyDB's Value::Integer is i64.
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,

            tags: tags.into_iter().map(String::from).collect(),
        }
    }

    // ─────────────────────────────────────────────────────────────────────
    // CONVERSION: Task → Value (for storing in GrumpyDB)
    // ─────────────────────────────────────────────────────────────────────

    /// Converts this Task into a GrumpyDB `Value::Object`.
    ///
    /// This is the **serialization** step. GrumpyDB stores `Value` types,
    /// so we must convert our struct into a `Value::Object` (a key-value map,
    /// similar to a JSON object).
    ///
    /// ## How it works
    ///
    /// Each field of the Task becomes a key-value pair in the map:
    /// - `"title"` → `Value::String("Buy groceries")`
    /// - `"done"` → `Value::Bool(false)`
    /// - `"tags"` → `Value::Array([Value::String("shopping"), ...])`
    ///
    /// We use `BTreeMap` because GrumpyDB requires deterministic key ordering
    /// (same struct → same bytes on disk → same checksums).
    pub fn to_value(&self) -> Value {
        let mut map = BTreeMap::new();

        // Each field is stored with a string key.
        // GrumpyDB doesn't enforce a schema — you choose the field names.
        map.insert("title".into(), Value::String(self.title.clone()));
        map.insert("done".into(), Value::Bool(self.done));
        map.insert("created_at".into(), Value::Integer(self.created_at));

        // Optional fields: store Null if absent, String if present.
        // This mirrors JSON's handling of nullable fields.
        map.insert(
            "description".into(),
            match &self.description {
                Some(desc) => Value::String(desc.clone()),
                None => Value::Null,
            },
        );

        // Tags are stored as an Array of Strings.
        // GrumpyDB supports nested types: arrays can contain any Value.
        map.insert(
            "tags".into(),
            Value::Array(self.tags.iter().map(|t| Value::String(t.clone())).collect()),
        );

        Value::Object(map)
    }

    // ─────────────────────────────────────────────────────────────────────
    // CONVERSION: Value → Task (for reading from GrumpyDB)
    // ─────────────────────────────────────────────────────────────────────

    /// Reconstructs a Task from a GrumpyDB `Value` and its UUID key.
    ///
    /// This is the **deserialization** step. When you call `db.get(&key)`,
    /// GrumpyDB returns a `Value`. You must convert it back to your struct.
    ///
    /// ## Why we need the `id` parameter
    ///
    /// The UUID key is stored *separately* from the document value in GrumpyDB
    /// (it's the B+Tree key). So we pass it explicitly to reconstruct the full Task.
    ///
    /// ## Error handling
    ///
    /// Returns `None` if the Value is not an Object or if required fields are missing.
    /// In a production app, you might want a proper error type here.
    pub fn from_value(id: Uuid, value: &Value) -> Option<Self> {
        // The stored value should be an Object (a key-value map).
        // If it's not, someone stored something unexpected — return None.
        let obj = value.as_object()?;

        // Extract each field from the map.
        // We use `?` to short-circuit if a required field is missing.
        let title = obj.get("title")?.as_str()?.to_string();

        let done = obj.get("done")?.as_bool()?;

        let created_at = obj.get("created_at")?.as_i64()?;

        // Description is optional: it may be Null or absent entirely.
        let description = obj
            .get("description")
            .and_then(|v| if v.is_null() { None } else { v.as_str() })
            .map(String::from);

        // Tags: extract the array, then convert each element to a String.
        // If the array contains non-string elements, we skip them.
        let tags = obj
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        Some(Self {
            id,
            title,
            description,
            done,
            created_at,
            tags,
        })
    }
}

/// Pretty-print a Task for the CLI output.
impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Status indicator: ✓ for done, ○ for pending
        let status = if self.done { "✓" } else { "○" };

        // Short UUID for display (first 8 chars is enough for humans)
        let short_id = &self.id.to_string()[..8];

        write!(f, "[{status}] {short_id}  {}", self.title)?;

        // Show tags if any
        if !self.tags.is_empty() {
            let tags_str: Vec<&str> = self.tags.iter().map(|s| s.as_str()).collect();
            write!(f, "  ({})", tags_str.join(", "))?;
        }

        Ok(())
    }
}
