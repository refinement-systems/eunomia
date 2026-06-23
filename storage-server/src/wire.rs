//! Wire encoding for the storage protocol (spec rev1§3.7): a fixed
//! hand-defined header (magic, protocol id, version) + a postcard body.
//! Messages fit the 256-byte inline channel payload (rev1§3.1); bulk data
//! rides in bounded Read/Write slices until the shared-memory bulk path
//! lands. Decoders treat payloads as untrusted and reject bad headers
//! and trailing bytes.

#![cfg(feature = "serde")]

use crate::{Request, Response};
use alloc::vec::Vec;

/// The fixed header prefix: magic 'E' + protocol id 0x51 (storage). The third
/// header byte is the **negotiated** wire version (rev1§3.7): version 2 (v2
/// carries the history-rewriting/GC opcodes) is the only version both peers
/// speak today, but as of C3C it is selected at session establishment — the
/// shell offers `[PROTO_VERSION, PROTO_VERSION]` in the `ipc` connect request,
/// storaged picks the highest common version, and every message is then
/// *stamped* with it and *validated* per-message (`ipc::version_ok`). The
/// header layout never migrates (rev1§3.7); only the value of the version byte
/// is dynamic.
pub const PROTO_MAGIC: [u8; 2] = [0x45, 0x51];
/// The storage protocol version this tree speaks. **Distinct namespace** from
/// `ipc::PROTOCOL_VERSION` (the connect-codec's own version) — the connect
/// handshake negotiates *this* number, not ipc's.
pub const PROTO_VERSION: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    BadHeader,
    /// The header's stamped version did not equal the session's negotiated
    /// version (rev1§3.7/§2.7): refused cleanly, never a crash. Distinct from
    /// `BadHeader` (bad magic/proto) so a version mismatch is diagnosable.
    Version,
    Body,
    TooLarge,
}

pub const MAX_MSG: usize = 256;

fn encode<T: serde::Serialize>(v: &T, version: u8) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&PROTO_MAGIC);
    out.push(version);
    let body = postcard::to_allocvec(v).map_err(|_| WireError::Body)?;
    out.extend_from_slice(&body);
    if out.len() > MAX_MSG {
        return Err(WireError::TooLarge);
    }
    Ok(out)
}

fn decode<T: serde::de::DeserializeOwned>(buf: &[u8], negotiated: u8) -> Result<T, WireError> {
    if buf.len() < 3 || buf[..2] != PROTO_MAGIC {
        return Err(WireError::BadHeader);
    }
    // Per-message version validation (rev1§2.7/§3.7): the stamped version must
    // equal the session's negotiated value, else refuse — never a crash. This
    // is dispatch-discipline outside the header codec, so the header layout and
    // the `ipc` `header.rs` bijection proofs are untouched.
    if !ipc::version_ok(buf[2], negotiated) {
        return Err(WireError::Version);
    }
    let (v, rest) = postcard::take_from_bytes(&buf[3..]).map_err(|_| WireError::Body)?;
    if !rest.is_empty() {
        return Err(WireError::Body);
    }
    Ok(v)
}

pub fn encode_request(r: &Request, version: u8) -> Result<Vec<u8>, WireError> {
    encode(r, version)
}

pub fn decode_request(buf: &[u8], negotiated: u8) -> Result<Request, WireError> {
    decode(buf, negotiated)
}

pub fn encode_response(r: &Response, version: u8) -> Result<Vec<u8>, WireError> {
    encode(r, version)
}

pub fn decode_response(buf: &[u8], negotiated: u8) -> Result<Response, WireError> {
    decode(buf, negotiated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    #[test]
    fn roundtrip_and_strictness() {
        let req = Request::Read {
            handle: 3,
            path: alloc::vec![b"etc".to_vec(), b"conf".to_vec()],
            offset: 7,
            len: 100,
        };
        let bytes = encode_request(&req, PROTO_VERSION).unwrap();
        assert_eq!(decode_request(&bytes, PROTO_VERSION).unwrap(), req);

        let resp = Response::Err(ErrorCode::Stale);
        let bytes = encode_response(&resp, PROTO_VERSION).unwrap();
        assert_eq!(decode_response(&bytes, PROTO_VERSION).unwrap(), resp);

        // Bad header, truncated body, trailing bytes: all rejected.
        assert_eq!(
            decode_request(b"xx", PROTO_VERSION),
            Err(WireError::BadHeader)
        );
        let bytes = encode_request(&req, PROTO_VERSION).unwrap();
        assert!(decode_request(&bytes[..bytes.len() - 1], PROTO_VERSION).is_err());
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(decode_request(&padded, PROTO_VERSION).is_err());
    }

    #[test]
    fn version_is_stamped_and_validated() {
        let req = Request::Sync { handle: 0 };

        // Round-trips at whatever version both sides agree on — the value is
        // dynamic, the layout is not.
        for v in [PROTO_VERSION, 0, 7, 255] {
            let bytes = encode_request(&req, v).unwrap();
            assert_eq!(bytes[..2], PROTO_MAGIC, "magic prefix preserved");
            assert_eq!(bytes[2], v, "version byte is stamped");
            assert_eq!(decode_request(&bytes, v).unwrap(), req);
        }

        // A frame stamped with a different version than the session negotiated
        // is refused cleanly as a `Version` error (not `BadHeader`, not a
        // panic) — the anti-theater teeth: decode must actually look at the
        // version, so a decoder that ignored it would return `Ok` and fail here.
        let bytes = encode_request(&req, PROTO_VERSION.wrapping_add(1)).unwrap();
        assert_eq!(
            decode_request(&bytes, PROTO_VERSION),
            Err(WireError::Version)
        );
        // Good magic, wrong version, regardless of (here absent) body.
        let probe = [PROTO_MAGIC[0], PROTO_MAGIC[1], PROTO_VERSION ^ 0xFF, 0u8];
        assert_eq!(
            decode_request(&probe, PROTO_VERSION),
            Err(WireError::Version)
        );
        // Wrong magic still wins over the version check (header is checked first).
        let bad_magic = [0x00, 0x51, PROTO_VERSION];
        assert_eq!(
            decode_request(&bad_magic, PROTO_VERSION),
            Err(WireError::BadHeader)
        );
    }
}
