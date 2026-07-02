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

//! Replay the committed path-resolver fuzz corpus through `resolve`, re-checking
//! the invariants. Keeps the fuzz inputs alive as ordinary tests and lets `cargo
//! miri test --test fuzz_corpus` UB-check each one. Mirrors
//! `loader/tests/fuzz_corpus.rs`.

use std::fs;
use std::path::PathBuf;

use eunomia_sys::path;

fn corpus_files(target: &str) -> Vec<Vec<u8>> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus");
    dir.push(target);
    match fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| fs::read(e.path()).unwrap())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[test]
fn path() {
    for data in corpus_files("path") {
        let Ok(r) = path::resolve(&data) else {
            continue;
        };
        assert!(r.n <= path::MAX_COMPONENTS);
        let range = data.as_ptr_range();
        for j in 0..r.n {
            let c = r.comps[j];
            // Every accepted component is a well-formed, storable name…
            assert!(!c.is_empty() && c.len() <= 255);
            assert!(!c.iter().any(|&b| b == 0 || b == b'/'));
            // …and no `.`/`..` survives resolution (confinement).
            assert!(!(c.len() == 1 && c[0] == b'.'));
            assert!(!(c.len() == 2 && c[0] == b'.' && c[1] == b'.'));
            // …borrowed from inside the input slice.
            let cr = c.as_ptr_range();
            assert!(cr.start >= range.start && cr.end <= range.end);
        }
    }
}
