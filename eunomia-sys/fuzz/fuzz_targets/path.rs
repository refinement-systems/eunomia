#![no_main]
//! Path resolution on arbitrary bytes (std-port 4.2, rev2§4.9/§2.3). A path is an
//! attacker-influenced filename in the versioned store, resolved client-side
//! before it reaches storaged. Property set: `resolve` never panics; every
//! accepted component is well-formed (1..=255 bytes, no NUL, no `/`, and not
//! `.`/`..` — so no `..` ever survives into the output, the confinement fact) and
//! lies inside the input; the depth is within `MAX_COMPONENTS`; and — the semantic
//! oracle Verus's totality theorem cannot state — the accepted/rejected verdict,
//! the resolved components, *and* the reject reason (escape vs malformed, std-port
//! 4.3) match a straightforward reference resolver.
//!
//! Run: `cargo +nightly fuzz run path`.
use libfuzzer_sys::fuzz_target;

use eunomia_sys::path;

/// A plain, obvious reference resolver, the differential oracle for
/// [`path::resolve`]: split on `/`; drop empty and `.`; pop on `..`, denying at
/// depth 0 (escape); reject a component with NUL or > 255 bytes; reject past
/// `MAX_COMPONENTS`. Returns the borrowed component list, or the reject **tag** —
/// `ESCAPE` for a depth-0 `..`, `MALFORMED` otherwise — mirroring
/// `path::RejectReason` (std-port 4.3).
const ESCAPE: u8 = 1;
const MALFORMED: u8 = 2;

fn reference(buf: &[u8]) -> Result<Vec<&[u8]>, u8> {
    let mut out: Vec<&[u8]> = Vec::new();
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
        out.push(comp);
    }
    Ok(out)
}

fn reject_tag(reason: path::RejectReason) -> u8 {
    match reason {
        path::RejectReason::Escape => ESCAPE,
        path::RejectReason::Malformed => MALFORMED,
    }
}

fuzz_target!(|data: &[u8]| {
    let reference = reference(data);
    match path::resolve(data) {
        Err(reason) => {
            assert_eq!(
                Err(reject_tag(reason)),
                reference,
                "resolve's reject reason disagrees with the reference (or it rejected \
                 a path the reference accepted)",
            );
        }
        Ok(r) => {
            let ref_ok = reference.expect("resolve accepted a path the reference rejected");
            assert_eq!(r.n, ref_ok.len(), "component count differs from reference");
            assert!(r.n <= path::MAX_COMPONENTS, "depth over the cap");
            let range = data.as_ptr_range();
            for j in 0..r.n {
                let c = r.comps[j];
                // Component-for-component agreement with the reference.
                assert_eq!(c, ref_ok[j], "component {j} differs from reference");
                // Well-formed: 1..=255 bytes, no NUL, no `/`, not `.`/`..`.
                assert!(
                    !c.is_empty() && c.len() <= 255,
                    "component length out of range"
                );
                assert!(
                    !c.iter().any(|&b| b == 0 || b == b'/'),
                    "component has NUL or `/`",
                );
                assert!(!(c.len() == 1 && c[0] == b'.'), "unresolved `.` in output",);
                assert!(
                    !(c.len() == 2 && c[0] == b'.' && c[1] == b'.'),
                    "unresolved `..` in output (confinement broken)",
                );
                // Borrowed from inside the input slice.
                let cr = c.as_ptr_range();
                assert!(
                    cr.start >= range.start && cr.end <= range.end,
                    "component escapes the input buffer",
                );
            }
        }
    }
});
