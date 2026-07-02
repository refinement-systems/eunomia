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

//! Regression tests for the path resolver. Each pins a hardened
//! behavior — the confinement denials and the `.`/`..` resolution semantics that
//! the Verus totality theorem does not (and cannot) state — so it cannot silently
//! regress. Mirrors `loader/tests/fuzz_regressions.rs`.

use eunomia_sys::path;

/// Collect a successful resolution into owned components, or `None` if refused.
fn resolve_vec(input: &[u8]) -> Option<Vec<Vec<u8>>> {
    let r = path::resolve(input).ok()?;
    Some((0..r.n).map(|j| r.comps[j].to_vec()).collect())
}

/// `true` iff `input` is refused as a confinement **escape**.
fn is_escape(input: &[u8]) -> bool {
    matches!(path::resolve(input), Err(path::RejectReason::Escape))
}

/// `true` iff `input` is refused as a **malformed** / too-deep path.
fn is_malformed(input: &[u8]) -> bool {
    matches!(path::resolve(input), Err(path::RejectReason::Malformed))
}

/// Owned component list from string literals, for terse expectations.
fn comps(parts: &[&[u8]]) -> Vec<Vec<u8>> {
    parts.iter().map(|p| p.to_vec()).collect()
}

/// A `..` that would pop above the process root handle names something
/// unreachable (rev2§2.3 confinement) — denied as an **escape** (
/// `RejectReason::Escape` → `ERR_FS_DENIED`), never clamped to root and never sent
/// to storaged.
#[test]
fn dotdot_escaping_root_is_denied() {
    assert!(is_escape(b".."));
    assert!(is_escape(b"../x"));
    assert!(is_escape(b"a/../../x")); // pop `a`, then escape
    assert!(is_escape(b"/../x"));
    assert!(is_escape(b"a/b/../../.."));
}

/// rev2§4.9: `.`/`..` are path syntax resolved by the walk, never stored/sent.
#[test]
fn dot_and_dotdot_resolved_never_stored() {
    assert_eq!(resolve_vec(b"a/./b"), Some(comps(&[b"a", b"b"])));
    assert_eq!(resolve_vec(b"a/../b"), Some(comps(&[b"b"])));
    assert_eq!(resolve_vec(b"a/b/../c"), Some(comps(&[b"a", b"c"])));
    assert_eq!(resolve_vec(b"./a"), Some(comps(&[b"a"])));
    assert_eq!(resolve_vec(b"a/b/.."), Some(comps(&[b"a"])));
    // `...` (three dots) is an ordinary name, not parent syntax.
    assert_eq!(resolve_vec(b"..."), Some(comps(&[b"..."])));
    assert_eq!(resolve_vec(b".x"), Some(comps(&[b".x"])));
}

/// Empty components (leading/trailing `/`, `//`) collapse; an all-empty path is
/// the handle root itself (`n == 0`).
#[test]
fn empty_components_collapsed() {
    assert_eq!(resolve_vec(b""), Some(comps(&[])));
    assert_eq!(resolve_vec(b"/"), Some(comps(&[])));
    assert_eq!(resolve_vec(b"///"), Some(comps(&[])));
    assert_eq!(resolve_vec(b"/a"), Some(comps(&[b"a"])));
    assert_eq!(resolve_vec(b"a/"), Some(comps(&[b"a"])));
    assert_eq!(resolve_vec(b"a//b"), Some(comps(&[b"a", b"b"])));
}

/// A NUL byte or a > 255-byte component is not a storable name
/// (`cas::prolly::validate_name`); refused client-side as **malformed**
/// (`RejectReason::Malformed` → `ERR_FS_BAD_PATH`, distinct from an escape)
/// rather than round-tripped into a server `BadPath`.
#[test]
fn malformed_components_refused() {
    assert!(is_malformed(b"a\0b"));
    assert!(is_malformed(&vec![b'a'; 256]));
    // Exactly 255 bytes is the largest valid name.
    let max = vec![b'a'; 255];
    assert_eq!(resolve_vec(&max), Some(vec![max.clone()]));
}

/// A path resolving deeper than `MAX_COMPONENTS` is refused; `..` re-pushes do
/// not count against the cap.
#[test]
fn depth_cap_enforced() {
    let n = path::MAX_COMPONENTS;
    let ok = (0..n)
        .map(|_| "a")
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes();
    assert_eq!(resolve_vec(&ok).map(|v| v.len()), Some(n));
    let too_deep = (0..n + 1)
        .map(|_| "a")
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes();
    // Over-deep is a malformed refusal, not an escape.
    assert!(is_malformed(&too_deep));
    // Churn far past the cap via `..` re-pushes stays within it (depth ≤ 1).
    let churn = (0..1000)
        .map(|_| "a/..")
        .collect::<Vec<_>>()
        .join("/")
        .into_bytes();
    assert_eq!(resolve_vec(&churn), Some(comps(&[])));
}
