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

//! Wire encoding for the storage protocol (spec rev2§3.7): a fixed
//! hand-defined header (magic, protocol id, version) + a postcard body.
//! Messages fit the 256-byte inline channel payload (rev2§3.1); bulk data
//! rides in bounded Read/Write slices until the shared-memory bulk path
//! lands. Decoders treat payloads as untrusted and reject bad headers
//! and trailing bytes.
//!
//! **Verified by Verus (the header+version prefix).** [`check_header`] is the
//! deductive core every `decode_*` shares: total over all byte strings (never
//! panics / reads OOB), it refuses `BadHeader` exactly on a short buffer or bad
//! magic and `Version` exactly on good-magic-but-wrong-version — the magic check
//! strictly precedes the version check, composing on the already-verified
//! `ipc::version_ok`. The postcard body decode that follows stays the trusted
//! interpreted seam: it is serde-gated (`postcard` is an optional dependency
//! dropped under the `--no-default-features` verify config, like cas's), so it is
//! outside verified scope by feature-exclusion and guarded by the host tests
//! below — no `external_body`, no new trusted seam.
// The `verus!{}` macro + ghost vocabulary; Verus requires it imported in the
// module that carries a proof. In an ordinary build the macro erases ghost code,
// so this import is otherwise unused (same allow as lib.rs / kcore / ipc).
#[allow(unused_imports)]
use vstd::prelude::*;

verus! {

/// The fixed header prefix: magic 'E' + protocol id 0x51 (storage). The third
/// header byte is the **negotiated** wire version (rev2§3.7): version 2 (v2
/// carries the history-rewriting/GC opcodes) is the only version both peers
/// speak today, but it is selected at session establishment — the
/// shell offers `[PROTO_VERSION, PROTO_VERSION]` in the `ipc` connect request,
/// storaged picks the highest common version, and every message is then
/// *stamped* with it and *validated* per-message (`ipc::version_ok`). The
/// header layout never migrates (rev2§3.7); only the value of the version byte
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
    /// version (rev2§3.7/§2.7): refused cleanly, never a crash. Distinct from
    /// `BadHeader` (bad magic/proto) so a version mismatch is diagnosable.
    Version,
    Body,
    TooLarge,
}

pub const MAX_MSG: usize = 256;

/// Ghost model of [`check_header`]: total over every byte string. `BadHeader`
/// when the buffer is shorter than the 3-byte header or its first two bytes are
/// not `PROTO_MAGIC`; else `Version` when the stamped byte 2 is not the
/// `negotiated` value; else `Ok(3)` — the offset where the postcard body begins.
/// The magic check is structurally first, so it always wins over a version
/// mismatch.
pub open spec fn spec_check_header(buf: Seq<u8>, negotiated: u8) -> Result<usize, WireError> {
    if buf.len() < 3 || buf[0] != PROTO_MAGIC@[0] || buf[1] != PROTO_MAGIC@[1] {
        Err(WireError::BadHeader)
    } else if buf[2] != negotiated {
        Err(WireError::Version)
    } else {
        Ok(3)
    }
}

/// Validate the fixed wire header + per-message version prefix (rev2§3.7),
/// mechanized total for *all* `(buf, negotiated)`: it equals [`spec_check_header`]
/// — never panics or reads out of bounds. `BadHeader` fires exactly on a buffer
/// shorter than 3 bytes or a wrong magic; `Version` fires exactly on a good
/// magic whose stamped version byte is not `negotiated` (composing on the
/// already-verified `ipc::version_ok`, whose `ensures ok == (h == n)` carries the
/// equivalence); on a good header it returns the body offset `3`. The magic check
/// strictly precedes the version check — a decoder that reordered them would
/// disagree with the spec and fail to verify. The postcard body that begins at
/// the returned offset is the trusted, serde-gated seam.
pub fn check_header(buf: &[u8], negotiated: u8) -> (r: Result<usize, WireError>)
    ensures
        r == spec_check_header(buf@, negotiated),
{
    broadcast use vstd::slice::group_slice_axioms, vstd::array::group_array_axioms;

    if buf.len() < 3 || buf[0] != PROTO_MAGIC[0] || buf[1] != PROTO_MAGIC[1] {
        return Err(WireError::BadHeader);
    }
    if !ipc::version_ok(buf[2], negotiated) {
        return Err(WireError::Version);
    }
    Ok(3)
}

} // verus!
#[cfg(feature = "serde")]
use crate::{Request, Response};
#[cfg(feature = "serde")]
use alloc::vec::Vec;

#[cfg(feature = "serde")]
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

#[cfg(feature = "serde")]
fn decode<T: serde::de::DeserializeOwned>(buf: &[u8], negotiated: u8) -> Result<T, WireError> {
    // The header + per-message version prefix is the Verus-verified gate
    // (`check_header`): total, refuses `BadHeader`/`Version` cleanly, and yields
    // the body offset. The postcard body decode below is the trusted interpreted
    // seam (serde-gated, outside verified scope).
    let off = check_header(buf, negotiated)?;
    let (v, rest) = postcard::take_from_bytes(&buf[off..]).map_err(|_| WireError::Body)?;
    if !rest.is_empty() {
        return Err(WireError::Body);
    }
    Ok(v)
}

#[cfg(feature = "serde")]
pub fn encode_request(r: &Request, version: u8) -> Result<Vec<u8>, WireError> {
    encode(r, version)
}

#[cfg(feature = "serde")]
pub fn decode_request(buf: &[u8], negotiated: u8) -> Result<Request, WireError> {
    decode(buf, negotiated)
}

#[cfg(feature = "serde")]
pub fn encode_response(r: &Response, version: u8) -> Result<Vec<u8>, WireError> {
    encode(r, version)
}

#[cfg(feature = "serde")]
pub fn decode_response(buf: &[u8], negotiated: u8) -> Result<Response, WireError> {
    decode(buf, negotiated)
}

// Header-prefix tests for the verified `check_header`, independent of serde so
// they compile (and pin the contract) in the `--no-default-features` build too.
#[cfg(test)]
mod header_tests {
    use super::*;

    #[test]
    fn check_header_cases() {
        // Short buffer (< 3 header bytes) → BadHeader, never a panic.
        assert_eq!(check_header(b"", PROTO_VERSION), Err(WireError::BadHeader));
        assert_eq!(check_header(b"E", PROTO_VERSION), Err(WireError::BadHeader));
        assert_eq!(
            check_header(&[PROTO_MAGIC[0], PROTO_MAGIC[1]], PROTO_VERSION),
            Err(WireError::BadHeader)
        );

        // Bad magic → BadHeader (regardless of the version byte).
        assert_eq!(
            check_header(&[0x00, PROTO_MAGIC[1], PROTO_VERSION], PROTO_VERSION),
            Err(WireError::BadHeader)
        );
        assert_eq!(
            check_header(&[PROTO_MAGIC[0], 0x00, PROTO_VERSION], PROTO_VERSION),
            Err(WireError::BadHeader)
        );

        // Good magic, wrong stamped version → Version.
        assert_eq!(
            check_header(
                &[
                    PROTO_MAGIC[0],
                    PROTO_MAGIC[1],
                    PROTO_VERSION.wrapping_add(1)
                ],
                PROTO_VERSION
            ),
            Err(WireError::Version)
        );

        // Good magic, right version → Ok at the body offset 3 (extra body bytes
        // are not inspected by the prefix check).
        assert_eq!(
            check_header(
                &[PROTO_MAGIC[0], PROTO_MAGIC[1], PROTO_VERSION],
                PROTO_VERSION
            ),
            Ok(3)
        );
        assert_eq!(
            check_header(
                &[PROTO_MAGIC[0], PROTO_MAGIC[1], PROTO_VERSION, 0xAA, 0xBB],
                PROTO_VERSION
            ),
            Ok(3)
        );
    }

    #[test]
    fn magic_strictly_precedes_version_has_teeth() {
        // Bad magic AND wrong version at once must report `BadHeader`, not
        // `Version`: the magic check wins. A decoder that checked the version
        // first (or skipped magic) would return `Version` here and fail.
        let bad_both = [0x00, 0x51, PROTO_VERSION.wrapping_add(1)];
        assert_eq!(
            check_header(&bad_both, PROTO_VERSION),
            Err(WireError::BadHeader)
        );
    }
}

#[cfg(all(test, feature = "serde"))]
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
