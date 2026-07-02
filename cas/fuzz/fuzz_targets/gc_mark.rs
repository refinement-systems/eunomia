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

#![no_main]
//! The GC mark walk (rev2§4.6) over adversarial *tree structure* — the
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
