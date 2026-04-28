#![no_main]
//! Fuzz the response serializer for sanity. Even though Response is
//! constructed in code (not parsed), this guards against regressions
//! in serialize() — feed it arbitrary text payloads.

use libfuzzer_sys::fuzz_target;
use grumpydb_protocol::Response;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let resp = if s.starts_with('+') {
            Response::Ok(s[1..].to_string())
        } else if s.starts_with('-') {
            Response::Error(s[1..].to_string())
        } else if s.starts_with('$') {
            Response::Bulk(Some(s[1..].to_string()))
        } else {
            Response::Array(vec![Response::Bulk(Some(s.to_string()))])
        };
        let _serialized = resp.serialize();
    }
});
