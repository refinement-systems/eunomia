//! Wire encoding for the storage protocol (spec §3.7): a fixed
//! hand-defined header (magic, protocol id, version) + a postcard body.
//! Messages fit the 256-byte inline channel payload (§3.1); bulk data
//! rides in bounded Read/Write slices until the shared-memory bulk path
//! lands. Decoders treat payloads as untrusted and reject bad headers
//! and trailing bytes.

#![cfg(feature = "serde")]

use crate::{Request, Response};
use alloc::vec::Vec;

/// magic 'E', protocol 0x51 (storage), version 1.
const HEADER: [u8; 3] = [0x45, 0x51, 0x01];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    BadHeader,
    Body,
    TooLarge,
}

pub const MAX_MSG: usize = 256;

fn encode<T: serde::Serialize>(v: &T) -> Result<Vec<u8>, WireError> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&HEADER);
    let body = postcard::to_allocvec(v).map_err(|_| WireError::Body)?;
    out.extend_from_slice(&body);
    if out.len() > MAX_MSG {
        return Err(WireError::TooLarge);
    }
    Ok(out)
}

fn decode<T: serde::de::DeserializeOwned>(buf: &[u8]) -> Result<T, WireError> {
    if buf.len() < 3 || buf[..3] != HEADER {
        return Err(WireError::BadHeader);
    }
    let (v, rest) = postcard::take_from_bytes(&buf[3..]).map_err(|_| WireError::Body)?;
    if !rest.is_empty() {
        return Err(WireError::Body);
    }
    Ok(v)
}

pub fn encode_request(r: &Request) -> Result<Vec<u8>, WireError> {
    encode(r)
}

pub fn decode_request(buf: &[u8]) -> Result<Request, WireError> {
    decode(buf)
}

pub fn encode_response(r: &Response) -> Result<Vec<u8>, WireError> {
    encode(r)
}

pub fn decode_response(buf: &[u8]) -> Result<Response, WireError> {
    decode(buf)
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
        let bytes = encode_request(&req).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), req);

        let resp = Response::Err(ErrorCode::Stale);
        let bytes = encode_response(&resp).unwrap();
        assert_eq!(decode_response(&bytes).unwrap(), resp);

        // Bad header, truncated body, trailing bytes: all rejected.
        assert_eq!(decode_request(b"xx"), Err(WireError::BadHeader));
        let bytes = encode_request(&req).unwrap();
        assert!(decode_request(&bytes[..bytes.len() - 1]).is_err());
        let mut padded = bytes.clone();
        padded.push(0);
        assert!(decode_request(&padded).is_err());
    }
}
