#![no_main]
//! The encoder direction. `#[derive(Arbitrary)]` builds a *typed* `Request`
//! straight from fuzz bytes; we encode it and decode it back. Driving the
//! encoder from structured values reaches request shapes the raw-bytes
//! decoder can't construct on its own, so it catches encoder/serializer
//! bugs the `request_dispatch` direction is blind to.
//!
//! Property: whenever a request fits the wire envelope (encode succeeds —
//! it can legitimately fail with `TooLarge` past the 256-byte inline
//! limit), decoding its bytes reproduces the original request exactly.
use libfuzzer_sys::fuzz_target;

use storage_server::{wire, Request};

fuzz_target!(|req: Request| {
    if let Ok(bytes) = wire::encode_request(&req) {
        let back = wire::decode_request(&bytes).expect("encoded request failed to decode");
        assert_eq!(
            back, req,
            "request did not survive encode/decode round-trip"
        );
    }
});
