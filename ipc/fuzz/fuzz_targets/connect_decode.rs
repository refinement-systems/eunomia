#![no_main]
//! The session connect codecs on arbitrary bytes (spec rev2§3.5/§3.7).
//! `ConnectReq`/`GrantReply::decode` are fixed-width, hand-written little-endian
//! codecs; Verus proves them total bijections (`ipc/src/session.rs`,
//! `lemma_req_encode_decode`/`lemma_grant_encode_decode`). This target is the
//! runtime witness that the proof's "total over arbitrary bytes" holds on the
//! exec path: decode never panics, and any input that decodes re-encodes
//! **byte-for-byte** — these codecs are canonical (a fixed layout), unlike the
//! postcard body in `wire_decode` where varints are not guaranteed minimal so
//! only the *value* round-trips.
use libfuzzer_sys::fuzz_target;

use ipc::{ConnectReq, GrantReply};

fuzz_target!(|data: &[u8]| {
    // ConnectReq: total decode; an accepted input is exactly REQ_LEN bytes and
    // re-encodes to those same bytes.
    if let Some(req) = ConnectReq::decode(data) {
        assert_eq!(
            &req.encode()[..],
            data,
            "ConnectReq decode/encode is not byte-stable"
        );
    }
    // GrantReply: total decode; an accepted input re-encodes to the same used
    // prefix (GRANT_LEN for a grant, REFUSED_LEN for a refusal).
    if let Some(reply) = GrantReply::decode(data) {
        let (buf, n) = reply.encode();
        assert_eq!(
            &buf[..n],
            data,
            "GrantReply decode/encode is not byte-stable"
        );
    }
});
