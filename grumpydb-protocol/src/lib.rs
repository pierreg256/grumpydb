//! GrumpyDB Wire Protocol — RESP-like text protocol for client/server communication.
//!
//! This crate defines the command and response types, along with a parser and
//! serializer, for the GrumpyDB wire protocol. It is shared between the server
//! and the Rust client driver.
//!
//! ## Protocol overview
//!
//! Commands are single-line text terminated by `\r\n`. Responses use a
//! Redis-inspired RESP encoding with type prefixes (`+`, `-`, `:`, `$`, `*`).

pub mod command;
pub mod parser;
pub mod response;

pub use command::{Action, Command, Resource};
pub use parser::{ProtocolError, parse_command};
pub use response::Response;

/// Default TCP port for GrumpyDB.
pub const DEFAULT_PORT: u16 = 6380;

/// Protocol version string sent in the server banner.
pub const PROTOCOL_VERSION: &str = "4.0.0";

/// Maximum length of a single command line (1 MiB). Prevents DoS via memory exhaustion.
pub const MAX_LINE_LENGTH: usize = 1_048_576;

/// Maximum length of a bulk string payload (16 MiB). Limits document size on the wire.
pub const MAX_BULK_LENGTH: usize = 16_777_216;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(DEFAULT_PORT, 6380);
        assert_eq!(PROTOCOL_VERSION, "4.0.0");
        const _: () = assert!(MAX_LINE_LENGTH > 0);
        const _: () = assert!(MAX_BULK_LENGTH > MAX_LINE_LENGTH);
    }
}
