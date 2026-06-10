//! BLAKE3 content addressing (spec §4.1).
//! Hash = address internally; never authority at the boundary (spec §2.4).

/// A 32-byte BLAKE3 hash — the internal address of a chunk or tree node.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hash([u8; 32]);

impl Hash {
    pub fn of(_data: &[u8]) -> Self {
        todo!("M2: blake3::hash(data)")
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
