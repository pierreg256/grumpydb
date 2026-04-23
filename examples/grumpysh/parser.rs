//! Command parser: tokenizes and parses GrumpyShell commands.

use grumpydb::Value;

use super::json_parser::parse_json;

/// A parsed GrumpyShell command.
#[derive(Debug)]
pub enum Command {
    /// `use <database>`
    Use(String),
    /// `db.createCollection("name")`
    CreateCollection(String),
    /// `db.dropCollection("name")`
    DropCollection(String),
    /// `db.collections()`
    ListCollections,
    /// `db.flush()`
    Flush,
    /// `db.<collection>.insert({ ... })`
    Insert(String, Value),
    /// `db.<collection>.get("id")`
    Get(String, String),
    /// `db.<collection>.find()` or `db.<collection>.find({ filter })`
    Find(String, Option<Value>),
    /// `db.<collection>.count()`
    Count(String),
    /// `db.<collection>.update("id", { ... })`
    Update(String, String, Value),
    /// `db.<collection>.delete("id")`
    Delete(String, String),
    /// `db.<collection>.createIndex("name", "field")`
    CreateIndex(String, String, String),
    /// `db.<collection>.dropIndex("name")`
    DropIndex(String, String),
    /// `db.<collection>.query("index", value)`
    Query(String, String, Value),
    /// `db.<collection>.queryRange("index", start, end)`
    QueryRange(String, String, Value, Value),
    /// `db.<collection>.indexes()`
    ListIndexes(String),
    /// `db.<collection>.compact()`
    Compact(String),
    /// `db.<collection>.stats()`
    Stats(String),
    /// `db.<collection>.resolve("id")`
    Resolve(String, String),
    /// `db.<collection>.resolveDeep("id"[, depth])`
    ResolveDeep(String, String, Option<usize>),
    /// `help` or `help <topic>`
    Help(Option<String>),
    /// `clear`
    Clear,
    /// `exit` or `quit`
    Exit,
}

/// Parses a line of input into a `Command`.
pub fn parse_command(input: &str) -> Result<Command, String> {
    let input = input.trim();

    if input.is_empty() {
        return Err(String::new()); // silent skip
    }

    // Simple commands
    if input == "exit" || input == "quit" {
        return Ok(Command::Exit);
    }
    if input == "clear" {
        return Ok(Command::Clear);
    }
    if input == "help" {
        return Ok(Command::Help(None));
    }
    if let Some(topic) = input.strip_prefix("help ") {
        return Ok(Command::Help(Some(topic.trim().to_string())));
    }
    if let Some(db_name) = input.strip_prefix("use ") {
        let name = db_name.trim().to_string();
        if name.is_empty() {
            return Err("usage: use <database_name>".into());
        }
        return Ok(Command::Use(name));
    }

    // db.method() or db.collection.method()
    if let Some(rest) = input.strip_prefix("db.") {
        return parse_db_command(rest);
    }

    Err(format!("unknown command: {input}"))
}

fn parse_db_command(input: &str) -> Result<Command, String> {
    // db.createCollection("name")
    if let Some(args) = strip_method(input, "createCollection") {
        let name = parse_single_string_arg(args)?;
        return Ok(Command::CreateCollection(name));
    }
    if let Some(args) = strip_method(input, "dropCollection") {
        let name = parse_single_string_arg(args)?;
        return Ok(Command::DropCollection(name));
    }
    if input.starts_with("collections()") {
        return Ok(Command::ListCollections);
    }
    if input.starts_with("flush()") {
        return Ok(Command::Flush);
    }

    // db.<collection>.<method>(...)
    let dot = input
        .find('.')
        .ok_or_else(|| format!("expected db.<collection>.<method>(), got: db.{input}"))?;
    let collection = &input[..dot];
    let rest = &input[dot + 1..];

    // Parse method
    if let Some(args) = strip_method(rest, "insert") {
        let value = parse_json(args.trim())?;
        return Ok(Command::Insert(collection.into(), value));
    }
    if let Some(args) = strip_method(rest, "get") {
        let id = parse_single_string_arg(args)?;
        return Ok(Command::Get(collection.into(), id));
    }
    if let Some(args) = strip_method(rest, "find") {
        let args = args.trim();
        if args.is_empty() {
            return Ok(Command::Find(collection.into(), None));
        }
        let filter = parse_json(args)?;
        return Ok(Command::Find(collection.into(), Some(filter)));
    }
    if rest.starts_with("count()") {
        return Ok(Command::Count(collection.into()));
    }
    if let Some(args) = strip_method(rest, "update") {
        let (id, value) = parse_id_and_json(args)?;
        return Ok(Command::Update(collection.into(), id, value));
    }
    if let Some(args) = strip_method(rest, "delete") {
        let id = parse_single_string_arg(args)?;
        return Ok(Command::Delete(collection.into(), id));
    }
    if let Some(args) = strip_method(rest, "createIndex") {
        let (name, field) = parse_two_string_args(args)?;
        return Ok(Command::CreateIndex(collection.into(), name, field));
    }
    if let Some(args) = strip_method(rest, "dropIndex") {
        let name = parse_single_string_arg(args)?;
        return Ok(Command::DropIndex(collection.into(), name));
    }
    if let Some(args) = strip_method(rest, "queryRange") {
        let (idx_name, start, end) = parse_index_range_args(args)?;
        return Ok(Command::QueryRange(collection.into(), idx_name, start, end));
    }
    if let Some(args) = strip_method(rest, "query") {
        let (idx_name, value) = parse_index_query_args(args)?;
        return Ok(Command::Query(collection.into(), idx_name, value));
    }
    if rest.starts_with("indexes()") {
        return Ok(Command::ListIndexes(collection.into()));
    }
    if rest.starts_with("compact()") {
        return Ok(Command::Compact(collection.into()));
    }
    if rest.starts_with("stats()") {
        return Ok(Command::Stats(collection.into()));
    }
    if let Some(args) = strip_method(rest, "resolveDeep") {
        let (id, depth) = parse_resolve_deep_args(args)?;
        return Ok(Command::ResolveDeep(collection.into(), id, depth));
    }
    if let Some(args) = strip_method(rest, "resolve") {
        let id = parse_single_string_arg(args)?;
        return Ok(Command::Resolve(collection.into(), id));
    }

    Err(format!("unknown method: db.{input}"))
}

/// Strips `method_name(...)` and returns the inner args (without parens).
fn strip_method<'a>(input: &'a str, method: &str) -> Option<&'a str> {
    let rest = input.strip_prefix(method)?.strip_prefix('(')?;
    let end = rest.rfind(')')?;
    Some(&rest[..end])
}

fn parse_single_string_arg(args: &str) -> Result<String, String> {
    let args = args.trim();
    if (args.starts_with('"') && args.ends_with('"'))
        || (args.starts_with('\'') && args.ends_with('\''))
    {
        Ok(args[1..args.len() - 1].to_string())
    } else {
        // Unquoted
        Ok(args.to_string())
    }
}

fn parse_two_string_args(args: &str) -> Result<(String, String), String> {
    let parts: Vec<&str> = args.splitn(2, ',').collect();
    if parts.len() != 2 {
        return Err("expected two arguments".into());
    }
    Ok((
        parse_single_string_arg(parts[0].trim())?,
        parse_single_string_arg(parts[1].trim())?,
    ))
}

fn parse_id_and_json(args: &str) -> Result<(String, Value), String> {
    // Find the first comma after the string arg
    let args = args.trim();
    let comma = find_top_level_comma(args)?;
    let id = parse_single_string_arg(args[..comma].trim())?;
    let json = parse_json(args[comma + 1..].trim())?;
    Ok((id, json))
}

fn parse_index_query_args(args: &str) -> Result<(String, Value), String> {
    let comma = find_top_level_comma(args)?;
    let name = parse_single_string_arg(args[..comma].trim())?;
    let value = parse_json(args[comma + 1..].trim())?;
    Ok((name, value))
}

fn parse_index_range_args(args: &str) -> Result<(String, Value, Value), String> {
    let first_comma = find_top_level_comma(args)?;
    let name = parse_single_string_arg(args[..first_comma].trim())?;
    let rest = &args[first_comma + 1..];
    let second_comma = find_top_level_comma(rest)?;
    let start = parse_json(rest[..second_comma].trim())?;
    let end = parse_json(rest[second_comma + 1..].trim())?;
    Ok((name, start, end))
}

fn find_top_level_comma(s: &str) -> Result<usize, String> {
    let mut depth = 0;
    let mut in_string = false;
    let mut quote_char = '"';

    for (i, c) in s.char_indices() {
        if in_string {
            if c == '\\' {
                continue;
            }
            if c == quote_char {
                in_string = false;
            }
            continue;
        }

        match c {
            '"' | '\'' => {
                in_string = true;
                quote_char = c;
            }
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            ',' if depth == 0 => return Ok(i),
            _ => {}
        }
    }

    Err("expected ','".into())
}

fn parse_resolve_deep_args(args: &str) -> Result<(String, Option<usize>), String> {
    let args = args.trim();
    match find_top_level_comma(args) {
        Ok(comma) => {
            let id = parse_single_string_arg(args[..comma].trim())?;
            let depth_str = args[comma + 1..].trim();
            let depth: usize = depth_str
                .parse()
                .map_err(|_| format!("invalid depth: {depth_str}"))?;
            Ok((id, Some(depth)))
        }
        Err(_) => {
            // No comma — just the ID, default depth
            let id = parse_single_string_arg(args)?;
            Ok((id, None))
        }
    }
}
