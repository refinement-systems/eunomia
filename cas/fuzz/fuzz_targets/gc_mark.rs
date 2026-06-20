#![no_main]
//! The GC mark walk (rev1§4.6) over adversarial *tree structure* — the
//! complement to `tree_node`, which fuzzes single-node decoding. The input is
//! a recipe (`cas::gc::build_recipe`) that builds a `MemStore` of well-formed
//! nodes wired into deep chains, wide fanout, shared subtrees, and dangling
//! references, then marks from the last-built node.
//!
//! Oracle (`cas::gc::check_recipe`): the walk must never panic or overflow;
//! on a clean refusal (a dangling reference → `FormatError`) it returns; on
//! success the mark set must be *sufficient* — every reachable entry reads
//! identically through the mark set alone and through the full store. Seed
//! with `cargo run -p cas --example gen_cas_corpus` (deep-chain, wide-fanout,
//! shared-subtree, chunked, and dangling recipes).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    cas::gc::check_recipe(data).unwrap();
});
