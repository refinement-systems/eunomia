//! FastCDC chunker stub (spec §4.1).
//! Target chunk size: 16–64 KiB using gear-hash CDC.

pub struct Chunker;

impl Chunker {
    pub fn new() -> Self {
        todo!("M2: implement FastCDC gear-hash chunker")
    }

    /// Push bytes and emit chunk boundaries via the callback.
    pub fn push(&mut self, _data: &[u8], _on_chunk: impl FnMut(&[u8])) {
        todo!("M2: gear-hash split")
    }

    /// Flush any buffered tail as a final chunk.
    pub fn flush(self, _on_chunk: impl FnMut(&[u8])) {
        todo!("M2: flush tail")
    }
}
