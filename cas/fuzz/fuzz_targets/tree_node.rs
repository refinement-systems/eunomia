#![no_main]
//! `parse_node` is the shallow node decoder the GC mark walk (§4.6) runs
//! on raw stored bytes — *below* the fetch-time hash check, so it must be
//! total on hostile input. The node hash gate lives above it, so this
//! harness feeds arbitrary bytes directly.
//!
//! For leaf nodes we also apply the canonical oracle: a level-0 node is
//! `[0][count u32][entry…]`, every part deterministic, so re-encoding the
//! parsed entries under the same header must reproduce the input byte for
//! byte. Internal nodes drop their separator keys into child hashes during
//! parse (the GC walk doesn't need them), so they get the totality check
//! only — there is no lossless single-node re-encoder for them.
use libfuzzer_sys::fuzz_target;

use cas::prolly::{parse_node, NodeRefs};

fuzz_target!(|data: &[u8]| {
    let Ok(refs) = parse_node(data) else { return };
    if let NodeRefs::Entries(entries) = refs {
        let mut re = alloc_node_header(entries.len());
        for e in &entries {
            re.extend_from_slice(&cas::tlv::encode(e));
        }
        assert_eq!(re, data, "leaf node decoder accepted non-canonical bytes");
    }
});

fn alloc_node_header(count: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + count);
    out.push(0u8); // level 0
    out.extend_from_slice(&(count as u32).to_le_bytes());
    out
}
