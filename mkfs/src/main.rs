//! mkfs — host-side tool to build the initial disk image (spec §7, M2).
//!
//! Reuses the `cas` storage crates to construct:
//!   - A/B superblock slots at fixed offsets (spec §4.2)
//!   - Initial WAL region (empty)
//!   - Chunk store with an initial directory tree populated from a
//!     host filesystem path
//!   - Ref table entry for `main` pointing at the initial tree root
//!   - Initial snapshot in the snapshot log
//!
//! Usage: mkfs <image.img> <populate-from-dir>
//!
//! M2 work items: everything above; must pass the proptest canonical-form
//! suite in `cas` before use.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: mkfs <image.img> <source-dir>");
        std::process::exit(1);
    }
    todo!("M2: build initial disk image")
}
