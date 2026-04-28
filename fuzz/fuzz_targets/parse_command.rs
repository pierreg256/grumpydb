#![no_main]
//! Fuzz the protocol parser. Goal: make it impossible to crash the server
//! with malformed input. ANY input must produce either Ok(Command) or
//! Err(ProtocolError) — never panic.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = grumpydb_protocol::parse_command(s);
    }
});
