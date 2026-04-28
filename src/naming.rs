//! Name validation for clients, databases, collections, and indexes.

use crate::error::{GrumpyError, Result};

/// Validates a name for use as a client, database, collection, or index identifier.
///
/// Rules: `[a-z0-9_]{1,64}`, no path separators, no dots, no spaces.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(GrumpyError::InvalidName("name cannot be empty".into()));
    }
    if name.len() > 64 {
        return Err(GrumpyError::InvalidName(format!(
            "name too long: {} chars (max 64)",
            name.len()
        )));
    }
    if name.starts_with('_') && name != "_default" && name != "_system" {
        return Err(GrumpyError::InvalidName(
            "names starting with '_' are reserved".into(),
        ));
    }
    for ch in name.chars() {
        if !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && ch != '_' {
            return Err(GrumpyError::InvalidName(format!(
                "invalid character '{ch}': only [a-z0-9_] allowed"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_names() {
        assert!(validate_name("users").is_ok());
        assert!(validate_name("my_collection").is_ok());
        assert!(validate_name("db01").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("_default").is_ok());
    }

    #[test]
    fn test_empty_name() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn test_too_long_name() {
        let long = "a".repeat(65);
        assert!(validate_name(&long).is_err());
    }

    #[test]
    fn test_invalid_chars() {
        assert!(validate_name("My-Collection").is_err());
        assert!(validate_name("db.name").is_err());
        assert!(validate_name("path/injection").is_err());
        assert!(validate_name("hello world").is_err());
        assert!(validate_name("UPPER").is_err());
    }

    #[test]
    fn test_reserved_underscore() {
        assert!(validate_name("_internal").is_err());
        assert!(validate_name("_default").is_ok()); // exception
    }
}
