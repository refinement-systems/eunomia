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

use cas::dev::FileDev;
use cas::store::{Store, StoreOptions};

fn p(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|s| s.as_bytes().to_vec()).collect()
}

#[test]
fn built_image_mounts_and_matches_source() {
    let base = std::env::temp_dir().join(format!("eunomia-mkfs-test-{}", std::process::id()));
    let src = base.join("src");
    let img = base.join("disk.img");
    std::fs::create_dir_all(src.join("sub")).unwrap();

    let small = b"hello eunomia".to_vec();
    // Large enough to take the chunk-list path several times over.
    let big: Vec<u8> = (0..1_500_000u32).flat_map(|i| i.to_le_bytes()).collect();
    std::fs::write(src.join("a.txt"), &small).unwrap();
    std::fs::write(src.join("sub").join("b.bin"), &big).unwrap();

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_mkfs"))
        .arg(&img)
        .arg(&src)
        .arg("32")
        .status()
        .unwrap();
    assert!(status.success());

    let store = Store::mount(FileDev::open(&img).unwrap(), StoreOptions::default()).unwrap();
    assert_eq!(store.read(b"main", &p(&["a.txt"])).unwrap().unwrap(), small);
    assert_eq!(
        store.read(b"main", &p(&["sub", "b.bin"])).unwrap().unwrap(),
        big
    );

    // mkfs takes snapshot #1, retention class keep.
    let snaps: Vec<_> = store.snapshots(b"main").collect();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].id, 1);
    assert_eq!(snaps[0].provenance, b"mkfs");

    // Snapshot reads resolve through the snapshot root.
    let root = store.snapshot_root(b"main", 1).unwrap();
    assert_eq!(
        store
            .read_at_root(&root, &p(&["sub", "b.bin"]))
            .unwrap()
            .unwrap(),
        big
    );

    std::fs::remove_dir_all(&base).ok();
}

#[test]
fn refuses_undersized_image_cleanly() {
    // rev2§4.5: an undersized device is refused on the clean
    // `run() -> Err -> main() -> ExitCode::FAILURE` path (exit code 1), never a
    // panic/abort (which would surface as 101 or a terminating signal).
    let base = std::env::temp_dir().join(format!("eunomia-mkfs-small-{}", std::process::id()));
    let src = base.join("src");
    let img = base.join("tiny.img");
    std::fs::create_dir_all(&src).unwrap();

    // 0 MiB: a zero-length device cannot hold the WAL + chunk floor, so
    // `Store::format` refuses before `populate` ever reads the source dir.
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_mkfs"))
        .arg(&img)
        .arg(&src)
        .arg("0")
        .status()
        .unwrap();
    assert!(!status.success(), "mkfs must fail on an undersized image");
    assert_eq!(
        status.code(),
        Some(1),
        "clean ExitCode::FAILURE, not a panic/abort"
    );

    std::fs::remove_dir_all(&base).ok();
}
