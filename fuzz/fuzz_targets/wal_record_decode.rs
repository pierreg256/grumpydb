#![no_main]
//! Fuzz the WAL record decoder.

use libfuzzer_sys::fuzz_target;
use grumpydb::wal::record::WalRecord;

fuzz_target!(|data: &[u8]| {
    // Try to decode any number of records from arbitrary bytes — must
    // never panic (only Ok or Err allowed).
    let mut offset = 0;
    while offset < data.len() {
        let chunk = &data[offset..];
        let consumed = WalRecord::from_bytes_v2(chunk)
            .or_else(|_| WalRecord::from_bytes_v1(chunk))
            .ok()
            .map(|(_record, consumed)| consumed)
            .unwrap_or(0);

        if consumed == 0 {
            break;
        }
        offset += consumed;
    }
});
