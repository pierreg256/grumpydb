#![no_main]
//! Fuzz the WAL record decoder.

use libfuzzer_sys::fuzz_target;
use grumpydb::wal::record::WalRecord;

fuzz_target!(|data: &[u8]| {
    // Try to decode any number of records from arbitrary bytes — must
    // never panic (only Ok or Err allowed).
    let mut offset = 0;
    while offset < data.len() {
        match WalRecord::from_bytes(&data[offset..]) {
            Ok((_record, consumed)) if consumed > 0 => offset += consumed,
            _ => break,
        }
    }
});
