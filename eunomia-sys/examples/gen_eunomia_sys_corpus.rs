// SPDX-License-Identifier: 0BSD
//! Seed-corpus generator for eunomia-sys/fuzz. The `path`
//! resolver takes raw, attacker-influenced filename bytes with no checksum/length
//! gate, so — unlike the ELF/startup/cas generators, which emit structures that
//! start warm past a gate — these seeds are just representative path strings, one
//! per equivalence class the differential fuzzer explores: `.`/`..` resolution,
//! confinement escapes, malformed components, and the length/depth boundaries.
//! The committed corpus is a superset of these plus the fuzzer-grown inputs.
//! Run: `cargo run -p eunomia-sys --example gen_eunomia_sys_corpus`.

use std::fs;
use std::path::PathBuf;

use eunomia_sys::path;

fn write_seed(name: &str, bytes: &[u8]) {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fuzz");
    p.push("corpus");
    p.push("path");
    fs::create_dir_all(&p).unwrap();
    p.push(name);
    fs::write(&p, bytes).unwrap();
    println!("  path/{name}: {} bytes", bytes.len());
}

fn main() {
    println!("seeding eunomia-sys fuzz corpus:");

    // Accepted, fully-resolved shapes.
    write_seed("normal", b"a/b/c");
    write_seed("deep", b"a/b/c/d/e/f/g/h");
    write_seed("dot", b"a/./b/./c"); // `.` dropped
    write_seed("dotdot_interior", b"a/b/../c"); // pop then descend
    write_seed("dotdot_midpop", b"a/b/c/../../d"); // two pops
    write_seed("dotdot_to_sibling", b"a/../b"); // pop to root, descend
    write_seed("dot_prefixed_name", b".hidden/file"); // `.hidden` != `.`
    write_seed("dotdot_prefixed_name", b"..foo/bar"); // `..foo` != `..`
    write_seed("mixed", b"/a/./b/../c//d/"); // every rule at once

    // Empty-yielding but accepted.
    write_seed("empty", b"");
    write_seed("root_only", b"/");
    write_seed("leading_slash", b"/a/b");
    write_seed("trailing_slash", b"a/b/");
    write_seed("double_slash", b"a//b"); // empty component dropped
    write_seed("many_slashes", b"///a///b///");
    write_seed("slash_run_then_dotdot", b"a///../b");

    // Confinement escapes → RejectReason::Escape (a `..` above the root).
    write_seed("dotdot_escape", b"../a");
    write_seed("dotdot_only", b"..");
    write_seed("dotdot_double_escape", b"a/../../b"); // pop to 0, then escape

    // Malformed → RejectReason::Malformed (NUL, over-long, over-deep).
    write_seed("nul_byte", b"a\0b");
    write_seed("nul_in_component", b"foo/ba\0r/baz");

    // Length boundary: a 255-byte component is the max accepted; 256 is malformed.
    write_seed("component_255", &vec![b'x'; 255]);
    write_seed("component_256", &vec![b'x'; 256]);

    // High (non-ASCII) bytes are legal in a component (only NUL/`/` are not).
    write_seed(
        "high_bytes",
        &(0x80u16..=0xFF).map(|b| b as u8).collect::<Vec<u8>>(),
    );

    // Depth boundary: exactly MAX_COMPONENTS accepted; one more is malformed.
    write_seed("max_components", &join_slashes(path::MAX_COMPONENTS));
    write_seed(
        "over_max_components",
        &join_slashes(path::MAX_COMPONENTS + 1),
    );

    println!("done.");
}

/// `a/a/.../a` with `n` single-byte components.
fn join_slashes(n: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        if i > 0 {
            v.push(b'/');
        }
        v.push(b'a');
    }
    v
}
