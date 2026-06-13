//! The fixed, hand-defined message header (spec §3.7). Every IPC message is
//! this header followed by a postcard-encoded body. The header layout **never
//! migrates** — it is the layer that makes every other layer migratable — so
//! it is a byte-exact, hand-written codec (no serde), 10 bytes, little-endian:
//!
//! ```text
//!   off 0  proto    : u8    protocol id
//!   off 1  version  : u8    protocol version
//!   off 2  opcode   : u16   request/response selector
//!   off 4  flags    : u16   message flags
//!   off 6  body_len : u32   length of the postcard body that follows
//! ```
//!
//! `decode` is a *total bijection* on exactly `HEADER_SIZE` bytes: it does no
//! field-value validation (a server validates `proto`/`version`/`opcode`
//! against what it speaks — spec §3.7's "unknown opcode yields an error" is a
//! dispatch concern, not the header layer's), which keeps `encode`∘`decode`
//! the identity. Verified in `crate::proofs` (plan §4.7).

/// Wire size of the fixed header, in bytes.
pub const HEADER_SIZE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub proto: u8,
    pub version: u8,
    pub opcode: u16,
    pub flags: u16,
    pub body_len: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderError {
    /// `buf` was not exactly `HEADER_SIZE` bytes — too short, or trailing
    /// bytes after the fixed header.
    BadLength,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[0] = self.proto;
        b[1] = self.version;
        b[2..4].copy_from_slice(&self.opcode.to_le_bytes());
        b[4..6].copy_from_slice(&self.flags.to_le_bytes());
        b[6..10].copy_from_slice(&self.body_len.to_le_bytes());
        b
    }

    /// Decode exactly `HEADER_SIZE` bytes. Rejects any other length (short
    /// input *and* trailing bytes); otherwise total over all byte values.
    pub fn decode(buf: &[u8]) -> Result<Header, HeaderError> {
        if buf.len() != HEADER_SIZE {
            return Err(HeaderError::BadLength);
        }
        Ok(Header {
            proto: buf[0],
            version: buf[1],
            opcode: u16::from_le_bytes([buf[2], buf[3]]),
            flags: u16::from_le_bytes([buf[4], buf[5]]),
            body_len: u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let h = Header { proto: 0x51, version: 2, opcode: 7, flags: 0x8000, body_len: 123 };
        assert_eq!(Header::decode(&h.encode()), Ok(h));
    }

    #[test]
    fn wrong_length_rejected() {
        assert_eq!(Header::decode(&[0u8; HEADER_SIZE - 1]), Err(HeaderError::BadLength));
        assert_eq!(Header::decode(&[0u8; HEADER_SIZE + 1]), Err(HeaderError::BadLength));
    }
}
