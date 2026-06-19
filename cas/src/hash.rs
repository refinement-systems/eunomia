//! BLAKE3 content addressing (spec rev0§4.1).
//! Hash = address internally; never authority at the boundary (spec rev0§2.4).

/// A 32-byte BLAKE3 hash — the internal address of a chunk or tree node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash([u8; 32]);

impl Hash {
    pub fn of(data: &[u8]) -> Self {
        Hash(*blake3::hash(data).as_bytes())
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Hash(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl core::fmt::Debug for Hash {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // First 8 bytes are plenty for log/debug disambiguation.
        for b in &self.0[..8] {
            write!(f, "{b:02x}")?;
        }
        write!(f, "…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_blake3_reference() {
        // Reference value from the BLAKE3 test vectors (empty input).
        assert_eq!(
            Hash::of(b""),
            Hash::from_bytes([
                0xaf, 0x13, 0x49, 0xb9, 0xf5, 0xf9, 0xa1, 0xa6, 0xa0, 0x40, 0x4d, 0xea, 0x36,
                0xdc, 0xc9, 0x49, 0x9b, 0xcb, 0x25, 0xc9, 0xad, 0xc1, 0x12, 0xb7, 0xcc, 0x9a,
                0x93, 0xca, 0xe4, 0x1f, 0x32, 0x62,
            ])
        );
    }

    #[test]
    fn distinct_inputs_distinct_hashes() {
        assert_ne!(Hash::of(b"a"), Hash::of(b"b"));
    }
}
