#![no_main]
//! Fuzz the document binary codec. Two invariants:
//!  1. `decode` on arbitrary bytes never panics (returns Err on garbage).
//!  2. Round-trip: encoding then decoding a valid value produces the same
//!     bytes. We compare encoded bytes (not the Value itself) to dodge
//!     IEEE-754 NaN inequality and any future variant equality quirks.

use grumpydb::document::codec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Invariant 1: never panic on garbage.
    let decoded = codec::decode(data);
    if let Ok(v) = decoded {
        // Invariant 2: encoding the decoded value, then decoding again,
        // produces the same byte representation. Stable second pass.
        let bytes1 = codec::encode_to_vec(&v);
        let v2 = codec::decode(&bytes1).expect("re-decode of just-encoded value");
        let bytes2 = codec::encode_to_vec(&v2);
        assert_eq!(
            bytes1, bytes2,
            "encode/decode/encode is not byte-stable"
        );
    }
});
