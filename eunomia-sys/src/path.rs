//! Verified path resolver (std-port 4.2): raw `/`-separated `OsStr` bytes → a
//! `.`/`..`-resolved, root-confined storage tree-component list (rev2§4.9,
//! rev2§2.3).
//!
//! rev2§4.9 makes `.`/`..` *path syntax* — resolved by the walk, never stored
//! and never sent on the wire — so storaged hard-rejects any `.`/`..` component
//! (`cas::prolly::validate_name`). The client must therefore resolve them before
//! it sends: drop `.`, pop on `..`. rev2§2.3's subtree cap ("confinement by
//! unreachability") makes a `..` that would pop above the process root handle
//! *unnameable*, so it is **denied** (not clamped): [`resolve`] returns `None`.
//!
//! The verified theorem is **totality** — over every `&[u8]`, `resolve` returns
//! without panicking or reading out of bounds — plus **output well-formedness**:
//! each returned component satisfies the same predicate storaged's
//! `validate_name` checks (1..=255 bytes, no NUL, no `/`, and not `.`/`..`). So
//! nothing this decoder accepts can be a `..`, and prepending the result to the
//! handle's subtree (server-side `full_path`) can never escape it — the
//! confinement guarantee, machine-checked. The resolution *semantics* (that
//! `a/../b` resolves to `[b]`, etc.) are a lossy transform whose correctness is
//! checked by the fuzz/proptest oracle, not by Verus (doc/guidelines/verus.md
//! §8).
//!
//! Host-buildable and verified by `cargo verus verify -p eunomia-sys` — it is
//! **not** target-gated (unlike [`crate::fs`]), because Verus runs on the host
//! where the eunomia/bare-metal `cfg` is false. The gated `fs` client calls
//! [`resolve`] and copies the borrowed components into the `Vec<Vec<u8>>` wire
//! path (the target build has `alloc`; this no-alloc core does not).
use vstd::prelude::*;

verus! {

/// Maximum resolved depth a path may reach — the fixed arena [`resolve`] fills
/// and the bound it enforces. A path deeper than this is refused (`None`); `..`
/// re-pushes never grow it (`a/../a/../…` stays depth ≤ 1). The 256-byte
/// `MAX_MSG` wire cap (rev2§3.1) already bounds a *sendable* path near this
/// depth, so this is not the binding limit for real paths.
pub const MAX_COMPONENTS: usize = 64;

/// The path separator byte, `b'/'`.
pub const SEP: u8 = 0x2F;

/// The dot byte, `b'.'`.
pub const DOT: u8 = 0x2E;

/// A resolved path: the first `n` entries of `comps` are the meaningful tree
/// components (each borrowing into the input path bytes), in order. `n == 0` is
/// the handle root itself (the empty relative path).
pub struct ResolvedPath<'a> {
    pub comps: [&'a [u8]; MAX_COMPONENTS],
    pub n: usize,
}

/// `sub` is a contiguous subrange of `buf` — the provenance fact for a borrowed
/// component (the `loader::startup::subseq_of` twin).
pub open spec fn subseq_of(sub: Seq<u8>, buf: Seq<u8>) -> bool {
    exists|a: int, b: int| 0 <= a <= b <= buf.len() && sub == buf.subrange(a, b)
}

/// `c` is the `.` name (a single dot). Written as length + byte rather than a
/// `seq!` literal so the exec check `c.len() == 1 && c[0] == DOT` discharges it
/// by pure length/index reasoning.
pub open spec fn is_dot_name(c: Seq<u8>) -> bool {
    c.len() == 1 && c[0] == DOT
}

/// `c` is the `..` name (two dots).
pub open spec fn is_dotdot_name(c: Seq<u8>) -> bool {
    c.len() == 2 && c[0] == DOT && c[1] == DOT
}

/// The predicate storaged's `cas::prolly::validate_name` enforces on a stored
/// entry name: 1..=255 bytes, no NUL and no `/`, and not the reserved `.`/`..`.
/// A component satisfying this is provably accepted by the server's
/// `validate_path`; in particular it is **not** `..`, the confinement fact.
pub open spec fn well_formed_component(c: Seq<u8>) -> bool {
    &&& 1 <= c.len() <= 255
    &&& forall|i: int| 0 <= i < c.len() ==> (#[trigger] c[i]) != 0 && c[i] != SEP
    &&& !is_dot_name(c)
    &&& !is_dotdot_name(c)
}

/// What [`resolve`] guarantees of a returned path: the depth is within the
/// arena, and every meaningful component is well-formed and borrows from the
/// input (the `loader::startup::well_formed_startup` twin).
pub open spec fn well_formed_resolved(p: ResolvedPath, buf: Seq<u8>) -> bool {
    &&& p.n <= MAX_COMPONENTS
    &&& forall|j: int| 0 <= j < p.n ==> well_formed_component(#[trigger] p.comps@[j]@)
    &&& forall|j: int| 0 <= j < p.n ==> subseq_of(#[trigger] p.comps@[j]@, buf)
}

/// The index of the next `/` at or after `start`, or `buf.len()` if none — the
/// end of the component beginning at `start`. Total; the returned index stays
/// within `[start, buf.len()]`.
fn next_sep(buf: &[u8], start: usize) -> (r: usize)
    requires
        start <= buf@.len(),
    ensures
        start <= r <= buf@.len(),
{
    broadcast use vstd::slice::group_slice_axioms;

    let mut i = start;
    while i < buf.len()
        invariant
            start <= i <= buf@.len(),
        decreases buf@.len() - i,
    {
        if buf[i] == SEP {
            return i;
        }
        i += 1;
    }
    i
}

/// Whether `c` is a valid stored component name — the exec twin of
/// [`well_formed_component`]. One-directional: a `true` result proves the
/// component well-formed (all [`resolve`] needs to admit it); a `false` result
/// carries no obligation (it drives a clean `None`).
fn component_ok(c: &[u8]) -> (r: bool)
    ensures
        r ==> well_formed_component(c@),
{
    broadcast use vstd::slice::group_slice_axioms;

    if c.len() < 1 || c.len() > 255 {
        return false;
    }
    let mut i = 0;
    while i < c.len()
        invariant
            0 <= i <= c@.len(),
            forall|w: int| 0 <= w < i ==> (#[trigger] c@[w]) != 0 && c@[w] != SEP,
        decreases c@.len() - i,
    {
        if c[i] == 0 || c[i] == SEP {
            return false;
        }
        i += 1;
    }
    if c.len() == 1 && c[0] == DOT {
        return false;
    }
    if c.len() == 2 && c[0] == DOT && c[1] == DOT {
        return false;
    }
    true
}

/// Resolve raw `/`-separated path bytes into a confined tree-component list
/// (rev2§4.9, rev2§2.3). Mechanized **total** over arbitrary bytes: for every
/// `&[u8]`, `resolve` returns without panicking or reading out of bounds, and
/// every accepted path is [`well_formed_resolved`]. Empty components
/// (leading/trailing `/`, `//`) and `.` are dropped; `..` pops the previous
/// component; a `..` at depth 0 (which would escape the process root handle) is
/// **denied** with `None`, as are a malformed component (NUL or > 255 bytes) and
/// a path deeper than [`MAX_COMPONENTS`].
pub fn resolve(buf: &[u8]) -> (r: Option<ResolvedPath<'_>>)
    ensures
        r matches Some(p) ==> well_formed_resolved(p, buf@),
{
    broadcast use vstd::slice::group_slice_axioms;

    let empty = vstd::slice::slice_subrange(buf, 0, 0);
    let mut comps: [&[u8]; MAX_COMPONENTS] = [empty;MAX_COMPONENTS];
    let mut k: usize = 0;
    let mut start: usize = 0;

    loop
        invariant
            k <= MAX_COMPONENTS,
            start <= buf@.len(),
            forall|j: int| 0 <= j < k ==> well_formed_component(#[trigger] comps@[j]@),
            forall|j: int| 0 <= j < k ==> subseq_of(#[trigger] comps@[j]@, buf@),
        decreases buf@.len() - start,
    {
        let sep = next_sep(buf, start);
        let comp = vstd::slice::slice_subrange(buf, start, sep);
        if comp.len() == 0 {
            // Empty component (leading/trailing `/` or `//`): drop.
        } else if comp.len() == 1 && comp[0] == DOT {
            // `.` — the current directory: drop.
        } else if comp.len() == 2 && comp[0] == DOT && comp[1] == DOT {
            // `..` — the parent. Popping at depth 0 would name above the root
            // handle: unnameable (rev2§2.3), so denied.
            if k == 0 {
                return None;
            }
            k = k - 1;
        } else {
            if !component_ok(comp) {
                return None;
            }
            if k >= MAX_COMPONENTS {
                return None;
            }
            assert(subseq_of(comp@, buf@)) by {
                assert(comp@ == buf@.subrange(start as int, sep as int));
            }
            let ghost prev = comps@;
            let ghost prev_k = k;
            comps[k] = comp;
            proof {
                assert forall|j: int| 0 <= j < prev_k + 1 implies well_formed_component(
                    #[trigger] comps@[j]@,
                ) by {
                    if j < prev_k {
                        assert(comps@[j] == prev[j]);
                    } else {
                        assert(comps@[j] == comp);
                    }
                }
                assert forall|j: int| 0 <= j < prev_k + 1 implies subseq_of(
                    #[trigger] comps@[j]@,
                    buf@,
                ) by {
                    if j < prev_k {
                        assert(comps@[j] == prev[j]);
                    } else {
                        assert(comps@[j] == comp);
                    }
                }
            }
            k = k + 1;
        }
        if sep >= buf.len() {
            return Some(ResolvedPath { comps, n: k });
        }
        start = sep + 1;
    }
}

} // verus!
