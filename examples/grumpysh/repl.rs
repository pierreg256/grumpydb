//! REPL engine: read-eval-print loop for GrumpyShell.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use grumpydb::Value;
use grumpydb::database::Database;

use super::filter::matches_filter;
use super::json_parser::to_json_string;
use super::parser::{Command, parse_command};

/// State of the REPL session.
pub struct Repl {
    /// Root data directory.
    data_dir: PathBuf,
    /// Currently open database (if any).
    db: Option<Database>,
    /// Name of the current database.
    db_name: Option<String>,
}

impl Repl {
    /// Creates a new REPL with the given data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            db: None,
            db_name: None,
        }
    }

    /// Returns the current prompt string.
    pub fn prompt(&self) -> String {
        match &self.db_name {
            Some(name) => format!("grumpy [{}]> ", name),
            None => "grumpy> ".to_string(),
        }
    }

    /// Executes a single line of input. Returns the output to display.
    /// Returns `None` if the REPL should exit.
    pub fn execute(&mut self, line: &str) -> Option<String> {
        let cmd = match parse_command(line) {
            Ok(cmd) => cmd,
            Err(e) if e.is_empty() => return Some(String::new()),
            Err(e) => return Some(format!("Error: {e}")),
        };

        match cmd {
            Command::Exit => None,
            Command::Clear => Some("\x1B[2J\x1B[H".to_string()),
            Command::Help(topic) => Some(help_text(topic.as_deref())),
            Command::Use(name) => self.cmd_use(&name),
            Command::CreateCollection(name) => self.with_db(|db| {
                db.create_collection(&name)?;
                Ok(format!("Collection \"{name}\" created"))
            }),
            Command::DropCollection(name) => self.with_db(|db| {
                db.drop_collection(&name)?;
                Ok(format!("Collection \"{name}\" dropped"))
            }),
            Command::ListCollections => self.with_db(|db| {
                let colls = db.list_collections();
                Ok(serde_json::to_string_pretty(&colls).unwrap_or_else(|_| format!("{colls:?}")))
            }),
            Command::Flush => self.with_db(|db| {
                db.flush()?;
                Ok("Flushed".into())
            }),
            Command::Insert(coll, value) => self.cmd_insert(&coll, value),
            Command::Get(coll, id) => self.cmd_get(&coll, &id),
            Command::Find(coll, filter) => self.cmd_find(&coll, filter),
            Command::Count(coll) => self.with_db(|db| {
                let count = db.document_count(&coll)?;
                Ok(count.to_string())
            }),
            Command::Update(coll, id, value) => self.cmd_update(&coll, &id, value),
            Command::Delete(coll, id) => self.cmd_delete(&coll, &id),
            Command::CreateIndex(coll, name, field) => self.with_db(|db| {
                db.create_index(&coll, &name, &field)?;
                Ok(format!("Index \"{name}\" created on field \"{field}\""))
            }),
            Command::DropIndex(coll, name) => self.with_db(|db| {
                db.drop_index(&coll, &name)?;
                Ok(format!("Index \"{name}\" dropped"))
            }),
            Command::Query(coll, idx, value) => self.cmd_query(&coll, &idx, &value),
            Command::QueryRange(coll, idx, start, end) => {
                self.cmd_query_range(&coll, &idx, &start, &end)
            }
            Command::ListIndexes(coll) => self.with_db(|db| {
                let coll_ref = db.collection(&coll)?;
                let indexes: Vec<&str> = coll_ref
                    .list_indexes()
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect();
                Ok(serde_json::to_string_pretty(&indexes)
                    .unwrap_or_else(|_| format!("{indexes:?}")))
            }),
            Command::Compact(coll) => self.with_db(|db| {
                let count = db.compact(&coll)?;
                Ok(format!("Compacted: {count} documents preserved"))
            }),
            Command::Stats(coll) => self.with_db(|db| {
                let count = db.document_count(&coll)?;
                let coll_ref = db.collection(&coll)?;
                let (reads, writes, cached, capacity) = coll_ref.pool_stats();
                let stats = Value::Object(BTreeMap::from([
                    ("documents".into(), Value::Integer(count as i64)),
                    (
                        "pool".into(),
                        Value::Object(BTreeMap::from([
                            ("reads".into(), Value::Integer(reads as i64)),
                            ("writes".into(), Value::Integer(writes as i64)),
                            ("cached".into(), Value::Integer(cached as i64)),
                            ("capacity".into(), Value::Integer(capacity as i64)),
                        ])),
                    ),
                ]));
                Ok(to_json_string(&stats, 0))
            }),
            Command::Resolve(coll, id) => self.cmd_resolve(&coll, &id),
            Command::ResolveDeep(coll, id, depth) => {
                self.cmd_resolve_deep(&coll, &id, depth)
            }
        }
    }

    fn cmd_use(&mut self, name: &str) -> Option<String> {
        // Close current database if any
        if let Some(db) = self.db.take() {
            let _ = db.close();
        }

        let db_path = self.data_dir.join(name);
        match Database::open(&db_path) {
            Ok(db) => {
                self.db = Some(db);
                self.db_name = Some(name.to_string());
                Some(format!("Switched to database \"{name}\""))
            }
            Err(e) => Some(format!("Error: {e}")),
        }
    }

    fn cmd_insert(&mut self, coll: &str, value: Value) -> Option<String> {
        self.with_db(|db| {
            let key = Uuid::new_v4();
            db.insert(coll, key, value)?;
            Ok(format!("Inserted: {key}"))
        })
    }

    fn cmd_get(&mut self, coll: &str, id: &str) -> Option<String> {
        self.with_db(|db| {
            let uuid = resolve_uuid(db, coll, id)?;
            match db.get(coll, &uuid)? {
                Some(value) => {
                    let mut obj = BTreeMap::new();
                    obj.insert("_id".to_string(), Value::String(uuid.to_string()));
                    if let Value::Object(fields) = value {
                        for (k, v) in fields {
                            obj.insert(k, v);
                        }
                    } else {
                        obj.insert("_value".to_string(), value);
                    }
                    Ok(to_json_string(&Value::Object(obj), 0))
                }
                None => Ok("null".into()),
            }
        })
    }

    fn cmd_find(&mut self, coll: &str, filter: Option<Value>) -> Option<String> {
        self.with_db(|db| {
            let all = db.scan(coll, ..)?;
            let results: Vec<Value> = all
                .into_iter()
                .filter(|(_, v)| filter.as_ref().is_none_or(|f| matches_filter(v, f)))
                .map(|(key, value)| {
                    let mut obj = BTreeMap::new();
                    obj.insert("_id".to_string(), Value::String(key.to_string()));
                    if let Value::Object(fields) = value {
                        for (k, v) in fields {
                            obj.insert(k, v);
                        }
                    } else {
                        obj.insert("_value".to_string(), value);
                    }
                    Value::Object(obj)
                })
                .collect();
            Ok(to_json_string(&Value::Array(results), 0))
        })
    }

    fn cmd_update(&mut self, coll: &str, id: &str, value: Value) -> Option<String> {
        self.with_db(|db| {
            let uuid = resolve_uuid(db, coll, id)?;
            db.update(coll, &uuid, value)?;
            Ok(format!("Updated: {uuid}"))
        })
    }

    fn cmd_delete(&mut self, coll: &str, id: &str) -> Option<String> {
        self.with_db(|db| {
            let uuid = resolve_uuid(db, coll, id)?;
            db.delete(coll, &uuid)?;
            Ok(format!("Deleted: {uuid}"))
        })
    }

    fn cmd_resolve(&mut self, coll: &str, id: &str) -> Option<String> {
        self.with_db(|db| {
            let uuid = resolve_uuid(db, coll, id)?;
            let value = db
                .get(coll, &uuid)?
                .ok_or(grumpydb::GrumpyError::KeyNotFound(uuid))?;

            // Walk the value and resolve one level of Ref values
            let resolved = resolve_one_level(db, &value)?;
            let mut obj = BTreeMap::new();
            obj.insert("_id".to_string(), Value::String(uuid.to_string()));
            if let Value::Object(fields) = resolved {
                for (k, v) in fields {
                    obj.insert(k, v);
                }
            } else {
                obj.insert("_value".to_string(), resolved);
            }
            Ok(to_json_string(&Value::Object(obj), 0))
        })
    }

    fn cmd_resolve_deep(&mut self, coll: &str, id: &str, depth: Option<usize>) -> Option<String> {
        self.with_db(|db| {
            let uuid = resolve_uuid(db, coll, id)?;
            let value = db
                .get(coll, &uuid)?
                .ok_or(grumpydb::GrumpyError::KeyNotFound(uuid))?;

            let max_depth = depth.unwrap_or(16);
            let resolved = db.resolve_deep(&value, max_depth)?;
            let mut obj = BTreeMap::new();
            obj.insert("_id".to_string(), Value::String(uuid.to_string()));
            if let Value::Object(fields) = resolved {
                for (k, v) in fields {
                    obj.insert(k, v);
                }
            } else {
                obj.insert("_value".to_string(), resolved);
            }
            Ok(to_json_string(&Value::Object(obj), 0))
        })
    }

    fn cmd_query(&mut self, coll: &str, idx: &str, value: &Value) -> Option<String> {
        self.with_db(|db| {
            let results = db.query(coll, idx, value)?;
            let arr: Vec<Value> = results
                .into_iter()
                .map(|(key, v)| wrap_with_id(key, v))
                .collect();
            Ok(to_json_string(&Value::Array(arr), 0))
        })
    }

    fn cmd_query_range(
        &mut self,
        coll: &str,
        idx: &str,
        start: &Value,
        end: &Value,
    ) -> Option<String> {
        self.with_db(|db| {
            let results = db.query_range(coll, idx, start, end)?;
            let arr: Vec<Value> = results
                .into_iter()
                .map(|(key, v)| wrap_with_id(key, v))
                .collect();
            Ok(to_json_string(&Value::Array(arr), 0))
        })
    }

    /// Helper: runs a closure with the current database, or returns an error.
    fn with_db<F>(&mut self, f: F) -> Option<String>
    where
        F: FnOnce(&mut Database) -> grumpydb::Result<String>,
    {
        let Some(db) = &mut self.db else {
            return Some("No database selected. Use: use <database_name>".into());
        };
        match f(db) {
            Ok(output) => Some(output),
            Err(e) => Some(format!("Error: {e}")),
        }
    }
}

/// Resolves a UUID from a full or short prefix.
fn resolve_uuid(db: &mut Database, coll: &str, id: &str) -> grumpydb::Result<Uuid> {
    // Try full UUID first
    if let Ok(uuid) = id.parse::<Uuid>() {
        return Ok(uuid);
    }

    // Short prefix match — scan and find
    let all = db.scan(coll, ..)?;
    let matches: Vec<Uuid> = all
        .iter()
        .filter(|(key, _)| key.to_string().starts_with(id))
        .map(|(key, _)| *key)
        .collect();

    match matches.len() {
        0 => Err(grumpydb::GrumpyError::KeyNotFound(Uuid::nil())),
        1 => Ok(matches[0]),
        _ => Err(grumpydb::GrumpyError::Codec(format!(
            "ambiguous ID prefix '{id}': {} matches",
            matches.len()
        ))),
    }
}

fn wrap_with_id(key: Uuid, value: Value) -> Value {
    let mut obj = BTreeMap::new();
    obj.insert("_id".to_string(), Value::String(key.to_string()));
    if let Value::Object(fields) = value {
        for (k, v) in fields {
            obj.insert(k, v);
        }
    } else {
        obj.insert("_value".to_string(), value);
    }
    Value::Object(obj)
}

/// Resolves one level of Ref values in a Value tree.
fn resolve_one_level(db: &mut Database, value: &Value) -> grumpydb::Result<Value> {
    match value {
        Value::Ref(collection, uuid) => match db.resolve_ref(collection, uuid)? {
            Some(resolved) => Ok(resolved),
            None => Ok(value.clone()),
        },
        Value::Object(map) => {
            let mut resolved = BTreeMap::new();
            for (k, v) in map {
                resolved.insert(k.clone(), resolve_one_level(db, v)?);
            }
            Ok(Value::Object(resolved))
        }
        Value::Array(arr) => {
            let resolved: grumpydb::Result<Vec<Value>> =
                arr.iter().map(|v| resolve_one_level(db, v)).collect();
            Ok(Value::Array(resolved?))
        }
        _ => Ok(value.clone()),
    }
}

fn help_text(topic: Option<&str>) -> String {
    match topic {
        None => r#"GrumpyShell — Interactive GrumpyDB REPL

DATABASE:
  use <name>                            Open/create a database

COLLECTIONS:
  db.createCollection("name")           Create a collection
  db.dropCollection("name")             Drop a collection
  db.collections()                      List collections

CRUD:
  db.<coll>.insert({ ... })             Insert a document (auto UUID)
  db.<coll>.get("id")                   Get by ID (full or short prefix)
  db.<coll>.find()                      List all documents
  db.<coll>.find({ field: value })      Filter documents
  db.<coll>.count()                     Document count
  db.<coll>.update("id", { ... })       Replace a document
  db.<coll>.delete("id")                Delete a document

REFERENCES:
  $ref("collection", "uuid")            Reference syntax in JSON values
  db.<coll>.resolve("id")               Resolve refs (one level)
  db.<coll>.resolveDeep("id")           Resolve refs recursively (max 16)
  db.<coll>.resolveDeep("id", N)        Resolve refs recursively (max N)

INDEXES:
  db.<coll>.createIndex("name", "field")  Create secondary index
  db.<coll>.dropIndex("name")             Drop index
  db.<coll>.query("index", value)         Exact lookup via index
  db.<coll>.queryRange("index", s, e)     Range query [s, e)
  db.<coll>.indexes()                     List indexes

MAINTENANCE:
  db.<coll>.compact()                   Defragment + rebuild index
  db.<coll>.stats()                     Show document count + pool stats
  db.flush()                            Flush data + WAL checkpoint

OTHER:
  help                                  This help
  help <command>                        Detailed help (e.g., help insert)
  clear                                 Clear screen
  exit                                  Quit
"#.to_string(),
        Some("insert") => "db.<collection>.insert({ key: value, ... })\n\nInserts a JSON document with an auto-generated UUID.\nKeys can be unquoted. Values: strings, numbers, booleans, null, arrays, objects.\n\nExample: db.users.insert({ name: \"Alice\", age: 30 })".to_string(),
        Some("find") => "db.<collection>.find()\ndb.<collection>.find({ field: value })\n\nWithout a filter: returns all documents.\nWith a filter: returns documents where all fields match.\nNested fields: { \"address.city\": \"Paris\" }\n\nExample: db.users.find({ age: 30 })".to_string(),
        Some("query") => "db.<collection>.query(\"index_name\", value)\n\nLooks up documents via a secondary index (exact match).\nThe index must be created first with createIndex.\n\nExample:\n  db.users.createIndex(\"by_age\", \"age\")\n  db.users.query(\"by_age\", 30)".to_string(),
        Some("resolve") => "db.<collection>.resolve(\"id\")\n\nRetrieves a document and resolves one level of $ref() values.\nEach $ref(\"coll\", \"uuid\") is replaced by the target document's value.\n\nExample:\n  db.orders.resolve(\"abc123\")".to_string(),
        Some("resolveDeep") | Some("resolvedeep") => "db.<collection>.resolveDeep(\"id\"[, depth])\n\nRetrieves a document and recursively resolves $ref() values.\nDefault max depth is 16. Cycles are detected and reported as errors.\n\nExample:\n  db.orders.resolveDeep(\"abc123\")\n  db.orders.resolveDeep(\"abc123\", 5)".to_string(),
        Some("ref") => "$ref(\"collection\", \"uuid\")\n\nA reference to a document in another collection.\nUse in insert/update to create cross-collection links.\n\nExample:\n  db.orders.insert({ product: \"widget\", owner: $ref(\"users\", \"a3b4c5d6-...\") })".to_string(),
        Some(cmd) => format!("No detailed help for '{cmd}'. Try: help"),
    }
}
