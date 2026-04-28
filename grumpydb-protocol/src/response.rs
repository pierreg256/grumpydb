//! Response types for the GrumpyDB wire protocol.
//!
//! Responses use a RESP-like encoding with type prefixes:
//! - `+` simple string
//! - `-` error
//! - `:` integer
//! - `$` bulk string (or null)
//! - `*` array

/// A response from the server.
#[derive(Debug, Clone, PartialEq)]
pub enum Response {
    /// Simple string: `+<message>\r\n`
    Ok(String),
    /// Error: `-ERR <message>\r\n`
    Error(String),
    /// Integer: `:<value>\r\n`
    Integer(i64),
    /// Bulk string: `$<len>\r\n<data>\r\n` or null: `$-1\r\n`
    Bulk(Option<String>),
    /// Array: `*<count>\r\n<elements...>`
    Array(Vec<Response>),
}

impl Response {
    /// Serialize this response to the wire format.
    pub fn serialize(&self) -> String {
        match self {
            Response::Ok(msg) => format!("+{msg}\r\n"),
            Response::Error(msg) => format!("-ERR {msg}\r\n"),
            Response::Integer(n) => format!(":{n}\r\n"),
            Response::Bulk(None) => "$-1\r\n".to_string(),
            Response::Bulk(Some(data)) => {
                format!("${}\r\n{}\r\n", data.len(), data)
            }
            Response::Array(items) => {
                let mut out = format!("*{}\r\n", items.len());
                for item in items {
                    out.push_str(&item.serialize());
                }
                out
            }
        }
    }

    /// Parse a response from wire format.
    ///
    /// Returns the parsed response and the number of bytes consumed.
    /// Returns `None` if the input is incomplete (not enough data yet).
    pub fn parse(input: &str) -> Result<(Response, usize), ResponseParseError> {
        if input.is_empty() {
            return Err(ResponseParseError::Incomplete);
        }

        let first = input.as_bytes()[0];
        match first {
            b'+' => parse_simple_string(input),
            b'-' => parse_error(input),
            b':' => parse_integer(input),
            b'$' => parse_bulk(input),
            b'*' => parse_array(input),
            _ => Err(ResponseParseError::InvalidPrefix(first as char)),
        }
    }
}

/// Error during response parsing.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResponseParseError {
    #[error("incomplete response data")]
    Incomplete,
    #[error("invalid response prefix: '{0}'")]
    InvalidPrefix(char),
    #[error("invalid integer: {0}")]
    InvalidInteger(String),
    #[error("invalid bulk length: {0}")]
    InvalidBulkLength(String),
}

/// Find the next `\r\n` in the input. Returns the index of `\r`.
fn find_crlf(input: &str) -> Option<usize> {
    input.find("\r\n")
}

fn parse_simple_string(input: &str) -> Result<(Response, usize), ResponseParseError> {
    let crlf = find_crlf(input).ok_or(ResponseParseError::Incomplete)?;
    let msg = &input[1..crlf];
    Ok((Response::Ok(msg.to_string()), crlf + 2))
}

fn parse_error(input: &str) -> Result<(Response, usize), ResponseParseError> {
    let crlf = find_crlf(input).ok_or(ResponseParseError::Incomplete)?;
    let msg = &input[1..crlf];
    // Strip "ERR " prefix if present
    let msg = msg.strip_prefix("ERR ").unwrap_or(msg);
    Ok((Response::Error(msg.to_string()), crlf + 2))
}

fn parse_integer(input: &str) -> Result<(Response, usize), ResponseParseError> {
    let crlf = find_crlf(input).ok_or(ResponseParseError::Incomplete)?;
    let num_str = &input[1..crlf];
    let n: i64 = num_str
        .parse()
        .map_err(|_| ResponseParseError::InvalidInteger(num_str.to_string()))?;
    Ok((Response::Integer(n), crlf + 2))
}

fn parse_bulk(input: &str) -> Result<(Response, usize), ResponseParseError> {
    let crlf = find_crlf(input).ok_or(ResponseParseError::Incomplete)?;
    let len_str = &input[1..crlf];
    let len: i64 = len_str
        .parse()
        .map_err(|_| ResponseParseError::InvalidBulkLength(len_str.to_string()))?;

    if len < 0 {
        // Null bulk string
        return Ok((Response::Bulk(None), crlf + 2));
    }

    let len = len as usize;
    let data_start = crlf + 2;
    let data_end = data_start + len;
    let total = data_end + 2; // trailing \r\n

    if input.len() < total {
        return Err(ResponseParseError::Incomplete);
    }

    let data = &input[data_start..data_end];
    Ok((Response::Bulk(Some(data.to_string())), total))
}

fn parse_array(input: &str) -> Result<(Response, usize), ResponseParseError> {
    let crlf = find_crlf(input).ok_or(ResponseParseError::Incomplete)?;
    let count_str = &input[1..crlf];
    let count: usize = count_str
        .parse()
        .map_err(|_| ResponseParseError::InvalidBulkLength(count_str.to_string()))?;

    let mut offset = crlf + 2;
    let mut items = Vec::with_capacity(count);

    for _ in 0..count {
        if offset >= input.len() {
            return Err(ResponseParseError::Incomplete);
        }
        let (item, consumed) = Response::parse(&input[offset..])?;
        items.push(item);
        offset += consumed;
    }

    Ok((Response::Array(items), offset))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_ok() {
        let r = Response::Ok("hello".into());
        assert_eq!(r.serialize(), "+hello\r\n");
    }

    #[test]
    fn test_serialize_error() {
        let r = Response::Error("not found".into());
        assert_eq!(r.serialize(), "-ERR not found\r\n");
    }

    #[test]
    fn test_serialize_integer() {
        let r = Response::Integer(42);
        assert_eq!(r.serialize(), ":42\r\n");

        let r = Response::Integer(-1);
        assert_eq!(r.serialize(), ":-1\r\n");
    }

    #[test]
    fn test_serialize_bulk() {
        let r = Response::Bulk(Some("hello".into()));
        assert_eq!(r.serialize(), "$5\r\nhello\r\n");
    }

    #[test]
    fn test_serialize_bulk_null() {
        let r = Response::Bulk(None);
        assert_eq!(r.serialize(), "$-1\r\n");
    }

    #[test]
    fn test_serialize_array() {
        let r = Response::Array(vec![
            Response::Bulk(Some("foo".into())),
            Response::Bulk(Some("bar".into())),
        ]);
        assert_eq!(r.serialize(), "*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n");
    }

    #[test]
    fn test_serialize_empty_array() {
        let r = Response::Array(vec![]);
        assert_eq!(r.serialize(), "*0\r\n");
    }

    #[test]
    fn test_parse_ok() {
        let (r, n) = Response::parse("+OK\r\n").unwrap();
        assert_eq!(r, Response::Ok("OK".into()));
        assert_eq!(n, 5);
    }

    #[test]
    fn test_parse_error() {
        let (r, n) = Response::parse("-ERR key not found\r\n").unwrap();
        assert_eq!(r, Response::Error("key not found".into()));
        assert_eq!(n, 20);
    }

    #[test]
    fn test_parse_integer() {
        let (r, n) = Response::parse(":42\r\n").unwrap();
        assert_eq!(r, Response::Integer(42));
        assert_eq!(n, 5);
    }

    #[test]
    fn test_parse_negative_integer() {
        let (r, _) = Response::parse(":-100\r\n").unwrap();
        assert_eq!(r, Response::Integer(-100));
    }

    #[test]
    fn test_parse_bulk() {
        let (r, n) = Response::parse("$5\r\nhello\r\n").unwrap();
        assert_eq!(r, Response::Bulk(Some("hello".into())));
        assert_eq!(n, 11);
    }

    #[test]
    fn test_parse_bulk_null() {
        let (r, n) = Response::parse("$-1\r\n").unwrap();
        assert_eq!(r, Response::Bulk(None));
        assert_eq!(n, 5);
    }

    #[test]
    fn test_parse_array() {
        let input = "*2\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
        let (r, n) = Response::parse(input).unwrap();
        assert_eq!(
            r,
            Response::Array(vec![
                Response::Bulk(Some("foo".into())),
                Response::Bulk(Some("bar".into())),
            ])
        );
        assert_eq!(n, input.len());
    }

    #[test]
    fn test_parse_empty_array() {
        let (r, _) = Response::parse("*0\r\n").unwrap();
        assert_eq!(r, Response::Array(vec![]));
    }

    #[test]
    fn test_round_trip_all_types() {
        let responses = vec![
            Response::Ok("hello world".into()),
            Response::Error("bad request".into()),
            Response::Integer(999),
            Response::Bulk(Some("{\"name\":\"bob\"}".into())),
            Response::Bulk(None),
            Response::Array(vec![
                Response::Ok("item1".into()),
                Response::Integer(2),
                Response::Bulk(Some("item3".into())),
            ]),
        ];

        for original in &responses {
            let wire = original.serialize();
            let (parsed, _) = Response::parse(&wire).unwrap();
            assert_eq!(&parsed, original, "round-trip failed for {wire:?}");
        }
    }

    #[test]
    fn test_parse_incomplete() {
        assert_eq!(
            Response::parse("+hello"),
            Err(ResponseParseError::Incomplete)
        );
        assert_eq!(
            Response::parse("$5\r\nhel"),
            Err(ResponseParseError::Incomplete)
        );
        assert_eq!(Response::parse(""), Err(ResponseParseError::Incomplete));
    }

    #[test]
    fn test_parse_invalid_prefix() {
        assert!(matches!(
            Response::parse("!garbage\r\n"),
            Err(ResponseParseError::InvalidPrefix('!'))
        ));
    }

    #[test]
    fn test_parse_invalid_integer() {
        assert!(matches!(
            Response::parse(":abc\r\n"),
            Err(ResponseParseError::InvalidInteger(_))
        ));
    }

    #[test]
    fn test_serialize_bulk_multibyte() {
        // Bulk length is byte length, not char count
        let r = Response::Bulk(Some("héllo".into()));
        let wire = r.serialize();
        // "héllo" is 6 bytes in UTF-8 (é = 2 bytes)
        assert!(wire.starts_with("$6\r\n"));
        let (parsed, _) = Response::parse(&wire).unwrap();
        assert_eq!(parsed, r);
    }
}
