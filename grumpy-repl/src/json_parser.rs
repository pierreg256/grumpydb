//! Relaxed JSON parser: converts JS-like syntax to GrumpyDB `Value`.
//!
//! Supports: unquoted keys, single/double quotes, trailing commas,
//! booleans, null, numbers (integer & float), strings, arrays, objects,
//! and `$ref("collection", "uuid")` references.

use grumpydb::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

/// Parses a relaxed JSON string into a `Value`.
pub fn parse_json(input: &str) -> Result<Value, String> {
    let mut chars = input.trim().chars().peekable();
    let result = parse_value(&mut chars)?;
    // Skip trailing whitespace
    skip_whitespace(&mut chars);
    Ok(result)
}

/// Pretty-prints a `Value` as JSON.
pub fn to_json_string(value: &Value, indent: usize) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_string(),
        Value::Integer(i) => i.to_string(),
        Value::Float(f) => format!("{f}"),
        Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Bytes(b) => format!("\"<{} bytes>\"", b.len()),
        Value::Ref(collection, uuid) => format!("$ref(\"{collection}\", \"{uuid}\")"),
        Value::Tombstone { deleted_at_hlc, .. } => {
            format!("$tombstone(hlc={deleted_at_hlc})")
        }
        Value::Array(arr) => {
            if arr.is_empty() {
                return "[]".to_string();
            }
            let inner: Vec<String> = arr
                .iter()
                .map(|v| {
                    format!(
                        "{}{}",
                        "  ".repeat(indent + 1),
                        to_json_string(v, indent + 1)
                    )
                })
                .collect();
            format!("[\n{}\n{}]", inner.join(",\n"), "  ".repeat(indent))
        }
        Value::Object(obj) => {
            if obj.is_empty() {
                return "{}".to_string();
            }
            let inner: Vec<String> = obj
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}\"{}\": {}",
                        "  ".repeat(indent + 1),
                        k,
                        to_json_string(v, indent + 1)
                    )
                })
                .collect();
            format!("{{\n{}\n{}}}", inner.join(",\n"), "  ".repeat(indent))
        }
    }
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn skip_whitespace(chars: &mut Chars) {
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else {
            break;
        }
    }
}

fn parse_value(chars: &mut Chars) -> Result<Value, String> {
    skip_whitespace(chars);
    match chars.peek() {
        Some('{') => parse_object(chars),
        Some('[') => parse_array(chars),
        Some('"') | Some('\'') => parse_string(chars),
        Some('$') => parse_ref(chars),
        Some('t') | Some('f') => parse_bool(chars),
        Some('n') => parse_null(chars),
        Some(c) if *c == '-' || c.is_ascii_digit() => parse_number(chars),
        Some(c) => Err(format!("unexpected character: '{c}'")),
        None => Err("unexpected end of input".into()),
    }
}

fn parse_object(chars: &mut Chars) -> Result<Value, String> {
    chars.next(); // consume '{'
    let mut map = BTreeMap::new();
    skip_whitespace(chars);

    if chars.peek() == Some(&'}') {
        chars.next();
        return Ok(Value::Object(map));
    }

    loop {
        skip_whitespace(chars);
        if chars.peek() == Some(&'}') {
            chars.next(); // trailing comma support
            break;
        }

        let key = parse_key(chars)?;
        skip_whitespace(chars);
        expect_char(chars, ':')?;
        let value = parse_value(chars)?;
        map.insert(key, value);

        skip_whitespace(chars);
        match chars.peek() {
            Some(',') => {
                chars.next();
            }
            Some('}') => {
                chars.next();
                break;
            }
            Some(c) => return Err(format!("expected ',' or '}}', got '{c}'")),
            None => return Err("unterminated object".into()),
        }
    }

    Ok(Value::Object(map))
}

fn parse_key(chars: &mut Chars) -> Result<String, String> {
    skip_whitespace(chars);
    match chars.peek() {
        Some('"') | Some('\'') => {
            if let Value::String(s) = parse_string(chars)? {
                Ok(s)
            } else {
                Err("expected string key".into())
            }
        }
        Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
            // Unquoted key
            let mut key = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' {
                    key.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            Ok(key)
        }
        Some(c) => Err(format!("expected key, got '{c}'")),
        None => Err("expected key, got end of input".into()),
    }
}

fn parse_array(chars: &mut Chars) -> Result<Value, String> {
    chars.next(); // consume '['
    let mut arr = Vec::new();
    skip_whitespace(chars);

    if chars.peek() == Some(&']') {
        chars.next();
        return Ok(Value::Array(arr));
    }

    loop {
        skip_whitespace(chars);
        if chars.peek() == Some(&']') {
            chars.next(); // trailing comma support
            break;
        }

        arr.push(parse_value(chars)?);

        skip_whitespace(chars);
        match chars.peek() {
            Some(',') => {
                chars.next();
            }
            Some(']') => {
                chars.next();
                break;
            }
            Some(c) => return Err(format!("expected ',' or ']', got '{c}'")),
            None => return Err("unterminated array".into()),
        }
    }

    Ok(Value::Array(arr))
}

fn parse_string(chars: &mut Chars) -> Result<Value, String> {
    let quote = chars.next().unwrap(); // consume opening quote
    let mut s = String::new();

    loop {
        match chars.next() {
            Some('\\') => match chars.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('\\') => s.push('\\'),
                Some(c) if c == quote => s.push(c),
                Some(c) => {
                    s.push('\\');
                    s.push(c);
                }
                None => return Err("unterminated escape".into()),
            },
            Some(c) if c == quote => break,
            Some(c) => s.push(c),
            None => return Err("unterminated string".into()),
        }
    }

    Ok(Value::String(s))
}

fn parse_number(chars: &mut Chars) -> Result<Value, String> {
    let mut num_str = String::new();
    let mut is_float = false;

    if chars.peek() == Some(&'-') {
        num_str.push('-');
        chars.next();
    }

    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            num_str.push(c);
            chars.next();
        } else if c == '.' && !is_float {
            is_float = true;
            num_str.push(c);
            chars.next();
        } else {
            break;
        }
    }

    if is_float {
        let f: f64 = num_str
            .parse()
            .map_err(|_| format!("invalid float: {num_str}"))?;
        Ok(Value::Float(f))
    } else {
        let i: i64 = num_str
            .parse()
            .map_err(|_| format!("invalid integer: {num_str}"))?;
        Ok(Value::Integer(i))
    }
}

fn parse_bool(chars: &mut Chars) -> Result<Value, String> {
    let mut word = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_alphabetic() {
            word.push(c);
            chars.next();
        } else {
            break;
        }
    }
    match word.as_str() {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        _ => Err(format!("expected true/false, got '{word}'")),
    }
}

fn parse_null(chars: &mut Chars) -> Result<Value, String> {
    let mut word = String::new();
    for _ in 0..4 {
        if let Some(&c) = chars.peek()
            && c.is_ascii_alphabetic()
        {
            word.push(c);
            chars.next();
        }
    }
    if word == "null" {
        Ok(Value::Null)
    } else {
        Err(format!("expected null, got '{word}'"))
    }
}

/// Parses `$ref("collection", "uuid")` into `Value::Ref`.
fn parse_ref(chars: &mut Chars) -> Result<Value, String> {
    // Consume "$ref("
    let mut keyword = String::new();
    for _ in 0..4 {
        match chars.next() {
            Some(c) => keyword.push(c),
            None => return Err("unexpected end of input in $ref".into()),
        }
    }
    if keyword != "$ref" {
        return Err(format!("expected $ref, got '{keyword}'"));
    }
    skip_whitespace(chars);
    expect_char(chars, '(')?;

    // Parse collection name (string)
    skip_whitespace(chars);
    let collection = match parse_string(chars)? {
        Value::String(s) => s,
        _ => return Err("expected string for $ref collection".into()),
    };

    skip_whitespace(chars);
    expect_char(chars, ',')?;

    // Parse UUID (string)
    skip_whitespace(chars);
    let uuid_str = match parse_string(chars)? {
        Value::String(s) => s,
        _ => return Err("expected string for $ref uuid".into()),
    };

    skip_whitespace(chars);
    expect_char(chars, ')')?;

    let uuid: Uuid = uuid_str
        .parse()
        .map_err(|e| format!("invalid UUID in $ref: {e}"))?;

    Ok(Value::Ref(collection, uuid))
}

fn expect_char(chars: &mut Chars, expected: char) -> Result<(), String> {
    skip_whitespace(chars);
    match chars.next() {
        Some(c) if c == expected => Ok(()),
        Some(c) => Err(format!("expected '{expected}', got '{c}'")),
        None => Err(format!("expected '{expected}', got end of input")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_null() {
        assert_eq!(parse_json("null").unwrap(), Value::Null);
    }

    #[test]
    fn test_parse_bool() {
        assert_eq!(parse_json("true").unwrap(), Value::Bool(true));
        assert_eq!(parse_json("false").unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_parse_integer() {
        assert_eq!(parse_json("42").unwrap(), Value::Integer(42));
        assert_eq!(parse_json("-7").unwrap(), Value::Integer(-7));
        assert_eq!(parse_json("0").unwrap(), Value::Integer(0));
    }

    #[test]
    fn test_parse_float() {
        assert_eq!(parse_json("2.5").unwrap(), Value::Float(2.5));
        assert_eq!(parse_json("-0.5").unwrap(), Value::Float(-0.5));
    }

    #[test]
    fn test_parse_string() {
        assert_eq!(
            parse_json("\"hello\"").unwrap(),
            Value::String("hello".into())
        );
        assert_eq!(
            parse_json("'single'").unwrap(),
            Value::String("single".into())
        );
    }

    #[test]
    fn test_parse_array() {
        let v = parse_json("[1, 2, 3]").unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                Value::Integer(1),
                Value::Integer(2),
                Value::Integer(3)
            ])
        );
    }

    #[test]
    fn test_parse_object() {
        let v = parse_json(r#"{ "name": "Alice", "age": 30 }"#).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("name"), Some(&Value::String("Alice".into())));
        assert_eq!(obj.get("age"), Some(&Value::Integer(30)));
    }

    #[test]
    fn test_parse_unquoted_keys() {
        let v = parse_json("{ name: 'Bob', age: 25 }").unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("name"), Some(&Value::String("Bob".into())));
        assert_eq!(obj.get("age"), Some(&Value::Integer(25)));
    }

    #[test]
    fn test_parse_trailing_comma() {
        let v = parse_json("{ x: 1, y: 2, }").unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn test_parse_nested() {
        let v = parse_json(r#"{ user: { name: "Alice", tags: [1, 2] } }"#).unwrap();
        let obj = v.as_object().unwrap();
        assert!(obj.get("user").unwrap().as_object().is_some());
    }

    #[test]
    fn test_pretty_print() {
        let v = Value::Object(BTreeMap::from([
            ("name".into(), Value::String("Alice".into())),
            ("age".into(), Value::Integer(30)),
        ]));
        let json = to_json_string(&v, 0);
        assert!(json.contains("\"name\": \"Alice\""));
        assert!(json.contains("\"age\": 30"));
    }

    #[test]
    fn test_parse_ref() {
        let uuid = Uuid::from_u128(42);
        let input = format!("$ref(\"users\", \"{uuid}\")");
        let v = parse_json(&input).unwrap();
        assert_eq!(v, Value::Ref("users".into(), uuid));
    }

    #[test]
    fn test_parse_ref_in_object() {
        let uuid = Uuid::from_u128(99);
        let input = format!("{{ owner: $ref(\"users\", \"{uuid}\"), name: \"order\" }}");
        let v = parse_json(&input).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("owner"), Some(&Value::Ref("users".into(), uuid)));
        assert_eq!(obj.get("name"), Some(&Value::String("order".into())));
    }

    #[test]
    fn test_pretty_print_ref() {
        let uuid = Uuid::from_u128(42);
        let v = Value::Ref("tasks".into(), uuid);
        let s = to_json_string(&v, 0);
        assert!(s.starts_with("$ref("));
        assert!(s.contains("tasks"));
        assert!(s.contains(&uuid.to_string()));
    }
}
