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

//! Property tests for the path resolver: random path-shaped byte
//! strings checked against a plain reference resolver, for the structural output
//! invariants, and for join↔resolve idempotence (the "presentation policy"
//! tier). This is the `cargo test` complement to the cargo-fuzz differential
//! oracle (`fuzz/fuzz_targets/path.rs`), which needs nightly + cargo-fuzz.

use eunomia_sys::path;
use proptest::prelude::*;

/// The obvious reference resolver, owning its components (differential oracle):
/// split on `/`; drop empty and `.`; pop on `..`, denying at depth 0; reject a
/// NUL / > 255-byte component; reject past `MAX_COMPONENTS`. On refusal it returns
/// the reject **tag** — `ESCAPE` for a depth-0 `..`, `MALFORMED` otherwise —
/// mirroring `path::RejectReason` so the differential also pins the
/// escape/malformed split, not just the accept/reject verdict.
const ESCAPE: u8 = 1;
const MALFORMED: u8 = 2;

fn reference(buf: &[u8]) -> Result<Vec<Vec<u8>>, u8> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for comp in buf.split(|&b| b == b'/') {
        if comp.is_empty() {
            continue;
        }
        if comp.len() == 1 && comp[0] == b'.' {
            continue;
        }
        if comp.len() == 2 && comp[0] == b'.' && comp[1] == b'.' {
            if out.is_empty() {
                return Err(ESCAPE);
            }
            out.pop();
            continue;
        }
        if comp.len() > 255 || comp.iter().any(|&b| b == 0 || b == b'/') {
            return Err(MALFORMED);
        }
        if out.len() >= path::MAX_COMPONENTS {
            return Err(MALFORMED);
        }
        out.push(comp.to_vec());
    }
    Ok(out)
}

/// `resolve` projected to the same shape as [`reference`]: owned components on
/// success, or the matching reject tag.
fn resolve_full(input: &[u8]) -> Result<Vec<Vec<u8>>, u8> {
    match path::resolve(input) {
        Ok(r) => Ok((0..r.n).map(|j| r.comps[j].to_vec()).collect()),
        Err(path::RejectReason::Escape) => Err(ESCAPE),
        Err(path::RejectReason::Malformed) => Err(MALFORMED),
    }
}

fn resolve_vec(input: &[u8]) -> Option<Vec<Vec<u8>>> {
    resolve_full(input).ok()
}

/// Join components with `/` — the presentation direction (std owns real display;
/// this is only for the idempotence property).
fn join(comps: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, c) in comps.iter().enumerate() {
        if i > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(c);
    }
    out
}

proptest! {
    /// Over a dense path alphabet (`/ . a b` + NUL), `resolve` agrees with the
    /// reference on the accept/reject verdict, the resolved components, *and* the
    /// reject reason (escape vs malformed) — the whole `Result` matches.
    #[test]
    fn matches_reference_structured(
        bytes in prop::collection::vec(
            prop_oneof![Just(b'/'), Just(b'.'), Just(b'a'), Just(b'b'), Just(0u8)],
            0..48usize,
        )
    ) {
        prop_assert_eq!(resolve_full(&bytes), reference(&bytes));
    }

    /// The same agreement over fully arbitrary bytes.
    #[test]
    fn matches_reference_arbitrary(bytes in prop::collection::vec(any::<u8>(), 0..64usize)) {
        prop_assert_eq!(resolve_full(&bytes), reference(&bytes));
    }

    /// Every accepted component is a storable name with no surviving `.`/`..`, and
    /// re-resolving the `/`-joined output reproduces it exactly (idempotence).
    #[test]
    fn output_wellformed_and_idempotent(
        bytes in prop::collection::vec(
            prop_oneof![Just(b'/'), Just(b'.'), Just(b'a'), Just(b'b')],
            0..48usize,
        )
    ) {
        if let Some(r) = resolve_vec(&bytes) {
            for c in &r {
                prop_assert!(!c.is_empty() && c.len() <= 255);
                prop_assert!(!c.iter().any(|&b| b == 0 || b == b'/'));
                prop_assert!(!(c.len() == 1 && c[0] == b'.'));
                prop_assert!(!(c.len() == 2 && c[0] == b'.' && c[1] == b'.'));
            }
            prop_assert_eq!(resolve_vec(&join(&r)), Some(r.clone()));
        }
    }
}
