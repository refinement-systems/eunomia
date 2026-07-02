// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

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
    if let Ok(bytes) = wire::encode_request(&req, wire::PROTO_VERSION) {
        let back = wire::decode_request(&bytes, wire::PROTO_VERSION)
            .expect("encoded request failed to decode");
        assert_eq!(
            back, req,
            "request did not survive encode/decode round-trip"
        );
    }
});
