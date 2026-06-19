//! The wire codec (spec rev0§3.7): every message is the fixed
//! [`Header`](crate::header::Header) (`header.rs`, byte-stable and Verus-verified)
//! followed by a **postcard**-encoded body.
//!
//! The postcard backend sits behind a **module-private** [`Codec`] trait, so
//! servers and clients construct and consume plain typed bodies and never reach
//! the serializer — no ad-hoc encoding, no pre-encoded byte blobs smuggled
//! through (rev0§3.7) — and the future IDL is just a second `Codec` impl. Bodies are
//! kept deliberately *boring* (owned, no borrowed lifetimes, no `flatten`, no
//! untagged enums, no non-string-keyed maps).
//!
//! Decoders treat every byte as untrusted: a malformed header, a `body_len`
//! that does not match the frame, and trailing bytes after the body are all
//! rejected, and `decode` is **total** (never panics) — it is the cargo-fuzz
//! target.

use alloc::vec::Vec;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::header::{Header, HEADER_SIZE};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// The fixed header was malformed or shorter than `HEADER_SIZE`.
    Header,
    /// The body did not (de)serialize as the expected type, or was truncated.
    Body,
    /// Bytes remained after the declared body — a framing violation (rev0§3.7).
    Trailing,
}

/// The serializer seam (rev0§3.7): module-private so it can be swapped for an IDL
/// backend later without touching any server. `Postcard` is the only impl today.
trait Codec {
    fn encode_body<B: Serialize>(body: &B) -> Result<Vec<u8>, WireError>;
    fn decode_body<B: DeserializeOwned>(bytes: &[u8]) -> Result<B, WireError>;
}

struct Postcard;

impl Codec for Postcard {
    fn encode_body<B: Serialize>(body: &B) -> Result<Vec<u8>, WireError> {
        postcard::to_allocvec(body).map_err(|_| WireError::Body)
    }

    fn decode_body<B: DeserializeOwned>(bytes: &[u8]) -> Result<B, WireError> {
        // take_from_bytes returns the unconsumed tail; any leftover is trailing
        // junk inside the declared body region — reject it (rev0§3.7).
        let (body, rest) = postcard::take_from_bytes(bytes).map_err(|_| WireError::Body)?;
        if !rest.is_empty() {
            return Err(WireError::Trailing);
        }
        Ok(body)
    }
}

/// Encode `body` as a full message: the fixed header — with `body_len` set from
/// the encoding — followed by the postcard body.
pub fn encode<B: Serialize>(
    proto: u8,
    version: u8,
    opcode: u16,
    flags: u16,
    body: &B,
) -> Result<Vec<u8>, WireError> {
    let body = Postcard::encode_body(body)?;
    let header = Header { proto, version, opcode, flags, body_len: body.len() as u32 };
    let mut out = Vec::with_capacity(HEADER_SIZE + body.len());
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode a full message into its header and body. **Total** over arbitrary
/// bytes; rejects a malformed header, a `body_len`/length mismatch (a truncated
/// body or trailing bytes after it), and trailing junk inside the body.
///
/// The declared `body_len` is never used to size an allocation — the body is the
/// real remaining slice — so a forged length cannot drive an OOM here (a body
/// that itself claims an enormous inner length is postcard's concern, and a
/// cargo-fuzz target).
pub fn decode<B: DeserializeOwned>(buf: &[u8]) -> Result<(Header, B), WireError> {
    if buf.len() < HEADER_SIZE {
        return Err(WireError::Header);
    }
    let header = Header::decode(&buf[..HEADER_SIZE]).map_err(|_| WireError::Header)?;
    let body = &buf[HEADER_SIZE..];
    let declared = header.body_len as usize;
    if body.len() < declared {
        return Err(WireError::Body); // truncated body
    }
    if body.len() > declared {
        return Err(WireError::Trailing); // bytes after the declared body
    }
    let value = Postcard::decode_body::<B>(body)?;
    Ok((header, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    enum Body {
        A,
        B { x: u32, name: String },
        C(vec::Vec<u8>),
    }

    fn enc(b: &Body) -> Vec<u8> {
        encode(7, 1, 9, 0, b).unwrap()
    }

    #[test]
    fn roundtrip_carries_header_and_body() {
        let b = Body::B { x: 42, name: String::from("etc/conf") };
        let bytes = enc(&b);
        let (h, got) = decode::<Body>(&bytes).unwrap();
        assert_eq!((h.proto, h.version, h.opcode), (7, 1, 9));
        assert_eq!(h.body_len as usize, bytes.len() - HEADER_SIZE);
        assert_eq!(got, b);
    }

    #[test]
    fn rejects_short_header() {
        assert_eq!(decode::<Body>(&[0u8; HEADER_SIZE - 1]), Err(WireError::Header));
    }

    #[test]
    fn rejects_truncated_body() {
        let bytes = enc(&Body::C(vec![1, 2, 3, 4]));
        // Drop a body byte: frame is now shorter than the declared body_len.
        assert_eq!(decode::<Body>(&bytes[..bytes.len() - 1]), Err(WireError::Body));
    }

    #[test]
    fn rejects_trailing_bytes() {
        let mut bytes = enc(&Body::A);
        bytes.push(0); // an extra byte past the declared body
        assert_eq!(decode::<Body>(&bytes), Err(WireError::Trailing));
    }
}
