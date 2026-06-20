//! FastCDC chunker (spec rev1§4.1): gear-hash content-defined chunking,
//! target chunk size 16–64 KiB, with normalized chunking (a stricter
//! boundary mask below the target size, a looser one above it).
//!
//! Determinism is a format property: chunk boundaries decide chunk hashes,
//! so the gear table and the masks are part of the on-disk format. Changing
//! either is a migration (it breaks dedup against existing stores, not
//! correctness).
//!
//! Self-synchronization: a boundary depends only on the 64 bytes of gear
//! window preceding it (plus the min/avg/max position within the current
//! chunk), so two streams sharing a suffix realign within a few chunks of
//! the first boundary they agree on. Pathological inputs that never hit a
//! gear boundary degrade to fixed max-size cuts and may never realign —
//! inherent to CDC, accepted (same as upstream FastCDC).

use alloc::vec::Vec;

/// Gear table — format constant. splitmix64 stream seeded with 0.
const fn gear_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
}

const GEAR: [u64; 256] = gear_table();

#[derive(Clone, Copy, Debug)]
pub struct ChunkerParams {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
}

impl ChunkerParams {
    /// Production parameters (spec rev1§4.1: target ~16–64 KiB).
    pub const DEFAULT: ChunkerParams = ChunkerParams {
        min: 16 * 1024,
        avg: 32 * 1024,
        max: 64 * 1024,
    };

    fn assert_valid(&self) {
        assert!(self.avg.is_power_of_two(), "avg must be a power of two");
        assert!(self.avg >= 16, "avg too small for normalized masks");
        assert!(self.min >= 1 && self.min < self.avg && self.avg < self.max);
    }

    /// Strict mask below the target size, loose mask above it (FastCDC
    /// normalized chunking, ±2 bits around log2(avg)).
    fn masks(&self) -> (u64, u64) {
        let bits = self.avg.trailing_zeros();
        ((1u64 << (bits + 2)) - 1, (1u64 << (bits - 2)) - 1)
    }
}

/// Find the first cut point in `data` (offset 0 = chunk start).
/// Returns the chunk length, or `None` if more input is needed.
fn find_boundary(params: &ChunkerParams, data: &[u8]) -> Option<usize> {
    let (mask_s, mask_l) = params.masks();
    let scan_end = data.len().min(params.max);
    if scan_end <= params.min {
        return if data.len() >= params.max { Some(params.max) } else { None };
    }
    let mut fp: u64 = 0;
    for (i, &b) in data[params.min..scan_end].iter().enumerate() {
        let pos = params.min + i;
        fp = (fp << 1).wrapping_add(GEAR[b as usize]);
        let mask = if pos < params.avg { mask_s } else { mask_l };
        if fp & mask == 0 {
            return Some(pos + 1);
        }
    }
    if data.len() >= params.max {
        Some(params.max)
    } else {
        None
    }
}

/// Cut positions for a complete buffer (the final sub-min tail is always
/// emitted as a chunk). Returned positions are exclusive chunk ends; the
/// last position equals `data.len()` unless `data` is empty.
pub fn boundaries(params: &ChunkerParams, data: &[u8]) -> Vec<usize> {
    params.assert_valid();
    let mut cuts = Vec::new();
    let mut start = 0;
    while start < data.len() {
        let cut = find_boundary(params, &data[start..]).unwrap_or(data.len() - start);
        start += cut;
        cuts.push(start);
    }
    cuts
}

/// Streaming chunker: feed bytes with `push`, finish with `flush`.
pub struct Chunker {
    params: ChunkerParams,
    buf: Vec<u8>,
}

impl Chunker {
    pub fn new() -> Self {
        Self::with_params(ChunkerParams::DEFAULT)
    }

    pub fn with_params(params: ChunkerParams) -> Self {
        params.assert_valid();
        Chunker {
            params,
            buf: Vec::new(),
        }
    }

    /// Push bytes and emit every completed chunk via the callback.
    pub fn push(&mut self, data: &[u8], mut on_chunk: impl FnMut(&[u8])) {
        self.buf.extend_from_slice(data);
        let mut start = 0;
        while let Some(cut) = find_boundary(&self.params, &self.buf[start..]) {
            on_chunk(&self.buf[start..start + cut]);
            start += cut;
        }
        self.buf.drain(..start);
    }

    /// Flush the buffered tail as the final chunk (may be shorter than min).
    pub fn flush(self, mut on_chunk: impl FnMut(&[u8])) {
        // push() drained every boundary, so the tail is always below max.
        debug_assert!(self.buf.len() < self.params.max);
        if !self.buf.is_empty() {
            on_chunk(&self.buf);
        }
    }
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Small parameters so proptest cases stay fast; same code paths.
    const TEST_PARAMS: ChunkerParams = ChunkerParams {
        min: 64,
        avg: 256,
        max: 1024,
    };

    fn chunk_lengths(params: &ChunkerParams, data: &[u8]) -> Vec<usize> {
        let mut lens = Vec::new();
        let mut prev = 0;
        for cut in boundaries(params, data) {
            lens.push(cut - prev);
            prev = cut;
        }
        lens
    }

    #[test]
    fn empty_input_no_chunks() {
        let mut got = 0;
        let chunker = Chunker::with_params(TEST_PARAMS);
        chunker.flush(|_| got += 1);
        assert_eq!(got, 0);
        assert!(boundaries(&TEST_PARAMS, &[]).is_empty());
    }

    #[test]
    fn gear_table_is_fixed() {
        // Format constant — if this changes, dedup against existing stores
        // silently stops working. Pin the first entries.
        assert_eq!(GEAR[0], 0xE220A8397B1DCDAF);
        assert_eq!(GEAR[255], 0x5A5832BB47BCF19E);
    }

    #[test]
    fn realigns_after_prefix_edit_on_random_data() {
        // Statistical self-synchronization check on pseudo-random data
        // (fixed seed, no flake): two streams sharing a suffix must end up
        // with identical boundaries within a few chunks.
        let mut state = 0x1234_5678_u64;
        let mut rand_bytes = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    (state >> 33) as u8
                })
                .collect()
        };
        let shared = rand_bytes(16 * 1024);
        let a: Vec<u8> = [rand_bytes(777).as_slice(), &shared].concat();
        let b: Vec<u8> = [rand_bytes(2048).as_slice(), &shared].concat();

        // Boundaries expressed relative to the start of the shared suffix.
        let rel = |data: &[u8]| -> Vec<i64> {
            let off = (data.len() - shared.len()) as i64;
            boundaries(&TEST_PARAMS, data)
                .into_iter()
                .map(|c| c as i64 - off)
                .filter(|&c| c > 0)
                .collect()
        };
        let ba = rel(&a);
        let bb = rel(&b);
        let common = ba.iter().find(|c| bb.contains(c));
        let first = *common.expect("streams never realigned on random data");
        // Realignment must happen within a few chunks of the suffix start.
        assert!(first < 4 * TEST_PARAMS.max as i64, "realigned too late: {first}");
        let ta: Vec<_> = ba.iter().filter(|&&c| c >= first).collect();
        let tb: Vec<_> = bb.iter().filter(|&&c| c >= first).collect();
        assert_eq!(ta, tb);
    }

    proptest! {
        // Miri: a few cases cover the same paths; native keeps the full sweep.
        #![proptest_config(ProptestConfig {
            cases: if cfg!(miri) { 4 } else { 256 },
            ..ProptestConfig::default()
        })]
        #[test]
        fn chunks_concatenate_to_input(data in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let mut out = Vec::new();
            let mut chunker = Chunker::with_params(TEST_PARAMS);
            chunker.push(&data, |c| out.extend_from_slice(c));
            chunker.flush(|c| out.extend_from_slice(c));
            prop_assert_eq!(out, data);
        }

        #[test]
        fn chunk_sizes_bounded(data in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let lens = chunk_lengths(&TEST_PARAMS, &data);
            for (i, &len) in lens.iter().enumerate() {
                prop_assert!(len <= TEST_PARAMS.max);
                if i + 1 != lens.len() {
                    prop_assert!(len > TEST_PARAMS.min);
                }
            }
        }

        #[test]
        fn segmentation_independent(
            data in proptest::collection::vec(any::<u8>(), 0..8192),
            splits in proptest::collection::vec(0usize..8192, 0..8),
        ) {
            // One-shot chunking.
            let mut whole = Vec::new();
            let mut c1 = Chunker::with_params(TEST_PARAMS);
            c1.push(&data, |c| whole.push(c.to_vec()));
            c1.flush(|c| whole.push(c.to_vec()));

            // Same bytes, pushed in arbitrary segments.
            let mut cuts: Vec<usize> = splits.iter().map(|&s| s % (data.len() + 1)).collect();
            cuts.sort_unstable();
            let mut pieces = Vec::new();
            let mut c2 = Chunker::with_params(TEST_PARAMS);
            let mut prev = 0;
            for cut in cuts.into_iter().chain([data.len()]) {
                c2.push(&data[prev..cut], |c| pieces.push(c.to_vec()));
                prev = cut;
            }
            c2.flush(|c| pieces.push(c.to_vec()));

            prop_assert_eq!(whole, pieces);
        }

        #[test]
        fn matches_boundaries_helper(data in proptest::collection::vec(any::<u8>(), 0..8192)) {
            let mut streamed = Vec::new();
            let mut pos = 0;
            let mut chunker = Chunker::with_params(TEST_PARAMS);
            let mut record = |c: &[u8]| { pos += c.len(); streamed.push(pos); };
            chunker.push(&data, &mut record);
            chunker.flush(record);
            prop_assert_eq!(streamed, boundaries(&TEST_PARAMS, &data));
        }

        #[test]
        fn shared_suffix_boundaries_agree_after_first_common(
            prefix_a in proptest::collection::vec(any::<u8>(), 0..2048),
            prefix_b in proptest::collection::vec(any::<u8>(), 0..2048),
            shared in proptest::collection::vec(any::<u8>(), 4096..8192),
        ) {
            let a: Vec<u8> = [prefix_a.as_slice(), &shared].concat();
            let b: Vec<u8> = [prefix_b.as_slice(), &shared].concat();
            let rel = |data: &[u8], plen: usize| -> Vec<i64> {
                boundaries(&TEST_PARAMS, data)
                    .into_iter()
                    .map(|c| c as i64 - plen as i64)
                    .filter(|&c| c > 0 && (c as usize) < shared.len())
                    .collect()
            };
            let ba = rel(&a, prefix_a.len());
            let bb = rel(&b, prefix_b.len());
            // CDC guarantee: once both streams cut at the same suffix
            // position, everything after is identical. (Existence of a
            // common cut is statistical, checked in the seeded test.)
            if let Some(&first) = ba.iter().find(|c| bb.contains(c)) {
                let ta: Vec<_> = ba.iter().filter(|&&c| c >= first).collect();
                let tb: Vec<_> = bb.iter().filter(|&&c| c >= first).collect();
                prop_assert_eq!(ta, tb);
            }
        }
    }
}
