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

//! Seed-corpus generator for the cas/fuzz targets. Emits valid, canonical
//! artifacts built with the real encoders into `cas/fuzz/corpus/<target>/`,
//! so every fuzz run (and the committed-corpus replay test) starts warm on
//! the happy path the mutation fuzzer struggles to reach unaided
//! (checksum/hash gates). Run: `cargo run -p cas --example gen_cas_corpus`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use cas::dev::{BlockDev, CrashDev, MemDev};
use cas::disk::{
    encode_index, IndexEntry, RefEntry, RefTable, SnapRow, Superblock, WalOp, CLASS_AUTO,
    CLASS_KEEP,
};
use cas::hash::Hash;
use cas::prolly::{
    parse_node, Content, Dir, Entry, EntryKind, MemStore, NodeRefs, NodeStore, FLAG_EXECUTABLE,
    INLINE_MAX,
};
use cas::store::{Store, StoreOptions};

fn corpus_dir(target: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("fuzz");
    p.push("corpus");
    p.push(target);
    fs::create_dir_all(&p).unwrap();
    p
}

fn write_seed(target: &str, name: &str, bytes: &[u8]) {
    let mut p = corpus_dir(target);
    p.push(name);
    fs::write(&p, bytes).unwrap();
    println!("  {target}/{name}: {} bytes", bytes.len());
}

fn small_opts() -> StoreOptions {
    StoreOptions {
        wal_len: 4096,
        chunker: cas::chunk::ChunkerParams {
            min: 64,
            avg: 256,
            max: 1024,
        },
        global_budget: 16 * 1024,
        ..StoreOptions::default()
    }
}

fn main() {
    println!("seeding cas fuzz corpora:");
    tlv_entry_seeds();
    tree_node_seeds();
    gc_mark_seeds();
    index_frame_seeds();
    superblock_seeds();
    wal_seeds();
    chunker_seeds();
    ref_table_seeds();
    mount_recovery_seeds();
    println!("done.");
}

fn tlv_entry_seeds() {
    let inline = Entry {
        name: b"hello".to_vec(),
        kind: EntryKind::File,
        flags: 0,
        size: 5,
        mtime: 7,
        content: Content::Inline(b"world".to_vec()),
    };
    let exec = Entry {
        name: b"run.sh".to_vec(),
        kind: EntryKind::File,
        flags: FLAG_EXECUTABLE,
        size: 3,
        mtime: 42,
        content: Content::Inline(b"#!/".to_vec()),
    };
    let dir = Entry {
        name: b"etc".to_vec(),
        kind: EntryKind::Dir,
        flags: 0,
        size: 0,
        mtime: 1,
        content: Content::DirRoot(Hash::of(b"child")),
    };
    let chunked = Entry {
        name: b"big.bin".to_vec(),
        kind: EntryKind::File,
        flags: 0,
        size: 100_000,
        mtime: 9,
        content: Content::ChunkList(Hash::of(b"list")),
    };

    // ── Boundary / equivalence-class seeds (the encode_raw field edges) ──
    // The name length is a `u8`: the empty and the 255-byte ceiling.
    let empty_name = Entry {
        name: Vec::new(),
        kind: EntryKind::File,
        flags: 0,
        size: 0,
        mtime: 0,
        content: Content::Inline(Vec::new()),
    };
    let max_name = Entry {
        name: vec![b'n'; 255],
        kind: EntryKind::File,
        flags: 0,
        size: 1,
        mtime: 1,
        content: Content::Inline(b"x".to_vec()),
    };
    // Inline content length is a `u16`: the empty payload and the INLINE_MAX
    // (512-byte) ceiling — the two ends of the inline-tag length field.
    let empty_inline = Entry {
        name: b"e".to_vec(),
        kind: EntryKind::File,
        flags: 0,
        size: 0,
        mtime: 0,
        content: Content::Inline(Vec::new()),
    };
    let inline_max = Entry {
        name: b"m".to_vec(),
        kind: EntryKind::File,
        flags: 0,
        size: INLINE_MAX as u64,
        mtime: 0,
        content: Content::Inline(vec![0xAB; INLINE_MAX]),
    };
    // The fixed 8-byte size/mtime readers at the top of the `u64` range.
    let field_max = Entry {
        name: b"max".to_vec(),
        kind: EntryKind::File,
        flags: 0,
        size: u64::MAX,
        mtime: u64::MAX,
        content: Content::ChunkList(Hash::of(b"max")),
    };
    // A non-`FLAG_EXECUTABLE` flags word (all bits set): the optional-section
    // decode at a value distinct from the lone `exec` seed.
    let flags_multi = Entry {
        name: b"flagged".to_vec(),
        kind: EntryKind::Dir,
        flags: u32::MAX,
        size: 0,
        mtime: 3,
        content: Content::DirRoot(Hash::of(b"sub")),
    };
    for (n, e) in [
        ("inline", inline),
        ("exec", exec),
        ("dir", dir),
        ("chunked", chunked),
        ("empty_name", empty_name),
        ("max_name", max_name),
        ("empty_inline", empty_inline),
        ("inline_max", inline_max),
        ("field_max", field_max),
        ("flags_multi", flags_multi),
    ] {
        write_seed("tlv_entry", n, &cas::tlv::encode(&e));
    }
}

fn tree_node_seeds() {
    let mut store = MemStore::new();

    // Empty directory node (the one legal empty node).
    let empty = Dir::new().save(&mut store);
    write_seed("tree_node", "empty", &store.get(&empty).unwrap());

    // A small single-leaf directory.
    let mut small = Dir::new();
    for (name, body) in [(&b"a"[..], &b"x"[..]), (b"bb", b"yy"), (b"ccc", b"zzz")] {
        small
            .upsert(Entry {
                name: name.to_vec(),
                kind: EntryKind::File,
                flags: 0,
                size: body.len() as u64,
                mtime: 1,
                content: Content::Inline(body.to_vec()),
            })
            .unwrap();
    }
    let leaf = small.save(&mut store);
    write_seed("tree_node", "leaf", &store.get(&leaf).unwrap());

    // The minimal non-empty leaf: exactly one entry (count == 1).
    let mut single = Dir::new();
    single
        .upsert(Entry {
            name: b"only".to_vec(),
            kind: EntryKind::File,
            flags: 0,
            size: 3,
            mtime: 1,
            content: Content::Inline(b"one".to_vec()),
        })
        .unwrap();
    let single_root = single.save(&mut store);
    write_seed(
        "tree_node",
        "single_leaf",
        &store.get(&single_root).unwrap(),
    );

    // A leaf whose entries span all three Content variants (Inline / ChunkList
    // / DirRoot) so one node decode covers every content tag.
    let mut mixed = Dir::new();
    for e in [
        Entry {
            name: b"a_inline".to_vec(),
            kind: EntryKind::File,
            flags: 0,
            size: 2,
            mtime: 1,
            content: Content::Inline(b"hi".to_vec()),
        },
        Entry {
            name: b"b_chunk".to_vec(),
            kind: EntryKind::File,
            flags: 0,
            size: 100_000,
            mtime: 2,
            content: Content::ChunkList(Hash::of(b"chunks")),
        },
        Entry {
            name: b"c_dir".to_vec(),
            kind: EntryKind::Dir,
            flags: 0,
            size: 0,
            mtime: 3,
            content: Content::DirRoot(Hash::of(b"child")),
        },
    ] {
        mixed.upsert(e).unwrap();
    }
    let mixed_root = mixed.save(&mut store);
    write_seed("tree_node", "mixed_leaf", &store.get(&mixed_root).unwrap());

    // A single-entry leaf carrying the largest item the entry encoding allows
    // (255-byte name, INLINE_MAX inline payload, flags set) — the leaf-item
    // size boundary.
    let mut big = Dir::new();
    big.upsert(Entry {
        name: vec![b'z'; 255],
        kind: EntryKind::File,
        flags: FLAG_EXECUTABLE,
        size: INLINE_MAX as u64,
        mtime: 4,
        content: Content::Inline(vec![0xCD; INLINE_MAX]),
    })
    .unwrap();
    let big_root = big.save(&mut store);
    write_seed(
        "tree_node",
        "big_entry_leaf",
        &store.get(&big_root).unwrap(),
    );

    // A wide directory that forces an internal level; dump the top node and
    // one of its leaves so both decode paths are seeded.
    let mut wide = Dir::new();
    for i in 0..300u32 {
        let name = format!("file-{i:04}");
        wide.upsert(Entry {
            name: name.into_bytes(),
            kind: EntryKind::File,
            flags: 0,
            size: 4,
            mtime: 1,
            content: Content::Inline(i.to_le_bytes().to_vec()),
        })
        .unwrap();
    }
    let root = wide.save(&mut store);
    let root_bytes = store.get(&root).unwrap();
    write_seed("tree_node", "internal", &root_bytes);
    if let Ok(NodeRefs::Children(children)) = parse_node(&root_bytes) {
        if let Some(child) = children.first() {
            write_seed("tree_node", "wide_leaf", &store.get(child).unwrap());
        }
    }
}

/// Recipe seeds for the `gc_mark` target (the mark walk over adversarial tree
/// structure). The input is a `cas::gc::build_recipe` stream of 1-byte commands
/// (`op = byte % 6`: inline-leaf, dir-root link, wide node, chunked-file leaf,
/// dangling reference, mixed leaf); these warm the fuzzer on the structural
/// shapes mutation struggles to assemble — deep chains, wide fanout, sharing,
/// chunk lists, and the clean-refusal (dangling) path.
fn gc_mark_seeds() {
    // A long run of dir-root links → a deep `DirRoot` chain (the stack-overflow
    // shape the bounded walk must complete).
    write_seed("gc_mark", "deep_chain", &vec![1u8; 300]);
    // Four chain nodes, then one wide node referencing them (fanout + sharing).
    write_seed(
        "gc_mark",
        "wide_fanout",
        &[1, 1, 1, 1, 2, 8, 0, 1, 2, 3, 0, 1, 2, 3, 0],
    );
    // One node, then a wide node pointing every entry at it (shared subtree).
    write_seed("gc_mark", "shared_subtree", &[1, 2, 5, 0, 0, 0, 0, 0, 0]);
    // Chunked-file leaf → mixed leaf → chain link (drives the chunk-list walk).
    write_seed("gc_mark", "chunked_mixed", &[3, 5, 5, 7, 1]);
    // A lone inline-file leaf.
    write_seed("gc_mark", "inline", &[0, 3, 9, 9, 9]);
    // A dangling reference: `mark` must refuse with `MissingNode`, not fault.
    write_seed("gc_mark", "dangling", &[4]);
}

fn index_frame_seeds() {
    let mut entries = BTreeMap::new();
    entries.insert(
        Hash::of(b"a"),
        IndexEntry {
            off: 48,
            len: 100,
            birth: 1,
        },
    );
    entries.insert(
        Hash::of(b"b"),
        IndexEntry {
            off: 196,
            len: 7,
            birth: 3,
        },
    );
    let mut free = BTreeMap::new();
    free.insert(300u64, 64u64);
    free.insert(512u64, 4096u64);
    write_seed("index_frame", "pad0", &encode_index(&entries, &free, 0));
    write_seed("index_frame", "pad17", &encode_index(&entries, &free, 17));
    write_seed(
        "index_frame",
        "empty",
        &encode_index(&BTreeMap::new(), &BTreeMap::new(), 0),
    );
}

fn superblock_seeds() {
    let sb = Superblock {
        generation: 7,
        ref_table: Hash::of(b"rt"),
        wal_head: 100,
        wal_next_seq: 42,
        wal_len: 4096,
        chunk_tail: 9999,
        index_off: 4242,
    };
    let bytes = sb.encode();
    write_seed("superblock", "valid", &bytes);
    write_seed("superblock_fixup", "valid", &bytes);

    // Re-seal a patched slot: recompute the body checksum so the mutated slot
    // reaches the branches past the checksum gate (mirrors `Superblock::encode`).
    let reseal = |buf: &mut [u8]| {
        let sum = Hash::of(&buf[..cas::disk::SB_BODY]);
        buf[cas::disk::SB_BODY..cas::disk::SB_BODY + 32].copy_from_slice(sum.as_bytes());
    };

    // A slot from a superseded format version: valid magic and a re-sealed
    // checksum, but `version != SB_VERSION`, so decode returns `WrongVersion`
    // (the tick-era refusal — never a reinterpretation, rev2§2.6). Low versions
    // stay wrong across any future format bump; mutation cannot forge the blake3
    // checksum, so this past-the-checksum branch is unreachable without a seed.
    for (name, ver) in [("version_v2", 2u32), ("version_v4", 4u32)] {
        let mut buf = sb.encode();
        buf[8..12].copy_from_slice(&ver.to_le_bytes());
        reseal(&mut buf);
        write_seed("superblock", name, &buf);
    }

    // Bad magic: rejected at the magic gate, which precedes the checksum, so the
    // slot is left un-resealed.
    let mut bad_magic = sb.encode();
    bad_magic[0] ^= 0xFF;
    write_seed("superblock", "bad_magic", &bad_magic);

    // Valid magic but a corrupted checksum: rejected at the checksum gate.
    let mut bad_checksum = sb.encode();
    bad_checksum[cas::disk::SB_BODY + 31] ^= 0xFF;
    write_seed("superblock", "bad_checksum", &bad_checksum);

    // Wrong length: rejected at the size gate (buf.len() != SB_SIZE).
    write_seed("superblock", "truncated", &sb.encode()[..100]);

    // Every offset/length field at the top of the u64 range — the fixed-width
    // `read_u64_le` readers at their ceiling. A validly-sealed slot (geometry is
    // a mount-time concern, not part of the byte decode), so it decodes Ok under
    // both targets; the fixup target keeps it warm past its checksum re-seal.
    let extremes = Superblock {
        generation: u64::MAX,
        ref_table: Hash::of(b"ext"),
        wal_head: u64::MAX,
        wal_next_seq: u64::MAX,
        wal_len: u64::MAX,
        chunk_tail: u64::MAX,
        index_off: u64::MAX,
    }
    .encode();
    write_seed("superblock", "field_extremes", &extremes);
    write_seed("superblock_fixup", "field_extremes", &extremes);
}

fn wal_seeds() {
    let w = WalOp::Write {
        ref_name: b"main".to_vec(),
        path: vec![b"etc".to_vec(), b"conf".to_vec()],
        offset: 512,
        mtime: 1234,
        data: b"hello".to_vec(),
    };
    let u = WalOp::Unlink {
        ref_name: b"main".to_vec(),
        path: vec![b"tmp".to_vec()],
        mtime: 5678,
    };
    let mut chain = Vec::new();
    chain.extend_from_slice(&w.encode_record(1));
    chain.extend_from_slice(&u.encode_record(2));
    chain.extend_from_slice(&w.encode_record(3));
    for t in ["wal_replay_scan", "wal_replay_scan_fixup"] {
        write_seed(t, "single", &w.encode_record(1));
        write_seed(t, "chain", &chain);
    }
}

fn chunker_seeds() {
    write_seed("chunker", "empty", &[]);
    write_seed("chunker", "tiny", b"hello world");
    let mid: Vec<u8> = (0..2000u32).map(|i| (i * 7) as u8).collect();
    write_seed("chunker", "multi", &mid);
}

/// The format-v4 ref table (rev2§4.7): refs (now carrying `edit_version`),
/// snapshot rows, and tags. Built with the real `RefTable::encode`, with
/// non-zero `edit_version`s so the new field is exercised on the happy path.
fn ref_table_seeds() {
    // Smallest valid table: magic + three zero counts.
    write_seed("ref_table", "empty", &RefTable::default().encode());

    // One ref, one snapshot row, one tag pinning it — the common shape.
    let mut simple = RefTable::default();
    simple.refs.insert(
        b"main".to_vec(),
        RefEntry {
            root: Hash::of(b"root"),
            generation: 0,
            next_snap_id: 2,
            edit_version: 3,
        },
    );
    simple.snaps.insert(
        (b"main".to_vec(), 1),
        SnapRow {
            id: 1,
            root: Hash::of(b"snap1"),
            timestamp: 1_700_000_000_000_000_000,
            provenance: b"session=1".to_vec(),
            parent: None,
            message: b"initial".to_vec(),
            class: CLASS_KEEP,
        },
    );
    simple
        .tags
        .insert(b"release".to_vec(), (b"main".to_vec(), 1));
    write_seed("ref_table", "simple", &simple.encode());

    // Several refs with distinct edit versions, plus a parented snapshot
    // chain — width and the parent/Option<u64> path together.
    let mut multi = RefTable::default();
    for i in 0..5u64 {
        let name = format!("ref-{i}");
        multi.refs.insert(
            name.into_bytes(),
            RefEntry {
                root: Hash::of(&[i as u8]),
                generation: i,
                next_snap_id: 3,
                edit_version: i * 2 + 1,
            },
        );
    }
    multi.snaps.insert(
        (b"ref-0".to_vec(), 1),
        SnapRow {
            id: 1,
            root: Hash::of(b"a"),
            timestamp: 10,
            provenance: b"session=7".to_vec(),
            parent: None,
            message: Vec::new(),
            class: CLASS_AUTO,
        },
    );
    multi.snaps.insert(
        (b"ref-0".to_vec(), 2),
        SnapRow {
            id: 2,
            root: Hash::of(b"b"),
            timestamp: 20,
            provenance: b"session=7".to_vec(),
            parent: Some(1),
            message: b"second".to_vec(),
            class: CLASS_AUTO,
        },
    );
    write_seed("ref_table", "multi_ref", &multi.encode());
}

/// Dump a device's whole byte image.
fn dump<D: BlockDev>(dev: &D) -> Vec<u8> {
    let len = dev.len() as usize;
    let mut buf = vec![0u8; len];
    dev.read(0, &mut buf).unwrap();
    buf
}

fn mount_recovery_seeds() {
    // mount_reseal mounts the same kind of whole images, just re-sealed
    // after mutation — the same seeds start both warm.
    let seed_both = |name: &str, bytes: &[u8]| {
        write_seed("mount_recovery", name, bytes);
        write_seed("mount_reseal", name, bytes);
    };

    // A clean, committed image with a nested dir, a chunked file, and a
    // snapshot — the consistency pass walks all of it.
    let mut store = Store::format(MemDev::new(32 * 1024), small_opts()).unwrap();
    store.create_ref(b"main").unwrap();
    store
        .write(b"main", &vec![b"readme".to_vec()], 0, b"hi", 1)
        .unwrap();
    let big: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    store
        .write(
            b"main",
            &vec![b"data".to_vec(), b"big".to_vec()],
            0,
            &big,
            2,
        )
        .unwrap();
    store.sync_all().unwrap();
    store
        .snapshot(b"main", b"gen", b"v1", cas::disk::CLASS_KEEP, 100)
        .unwrap();
    seed_both("clean", &dump(&store.into_dev()));

    // An image with an acked-but-unflushed write living only in the WAL —
    // mount must replay it. (No sync after the write.)
    let mut store = Store::format(MemDev::new(32 * 1024), small_opts()).unwrap();
    store.create_ref(b"main").unwrap();
    store
        .write(b"main", &vec![b"pending".to_vec()], 0, b"unflushed", 1)
        .unwrap();
    seed_both("wal_pending", &dump(&store.into_dev()));

    // Torn images straight out of the crash device: durable state plus a
    // random kept/dropped/torn subset of unflushed writes (rev2§4.5).
    for seed in [0xDEADu64, 0x1234, 0xF00D] {
        let mut store = Store::format(CrashDev::new(32 * 1024), small_opts()).unwrap();
        store.create_ref(b"main").unwrap();
        store
            .write(b"main", &vec![b"a".to_vec()], 0, b"committed", 1)
            .unwrap();
        store.sync_all().unwrap();
        store
            .write(b"main", &vec![b"b".to_vec()], 0, b"in flight", 2)
            .unwrap();
        let mut dev = store.into_dev();
        dev.crash(seed);
        seed_both(&format!("torn_{seed:x}"), &dump(&dev));
    }

    // A deliberately old-format image, intact but re-stamped v2: the
    // refusal branch (pre-v3 images get a version error, never a
    // reinterpretation) is code under test, and without a committed seed
    // a format bump silently rots mount coverage toward the live path.
    // (Under mount_reseal the fix-up re-stamps the current version, so
    // there this seed mutates into a live-path image — also useful.)
    let mut store = Store::format(MemDev::new(32 * 1024), small_opts()).unwrap();
    store.create_ref(b"main").unwrap();
    store
        .write(b"main", &vec![b"old".to_vec()], 0, b"tick era", 1)
        .unwrap();
    store.sync_all().unwrap();
    let mut img = dump(&store.into_dev());
    for off in [cas::disk::SB_A_OFF as usize, cas::disk::SB_B_OFF as usize] {
        img[off + 8..off + 12].copy_from_slice(&2u32.to_le_bytes());
        let sum = Hash::of(&img[off..off + cas::disk::SB_BODY]);
        img[off + cas::disk::SB_BODY..off + cas::disk::SB_BODY + 32]
            .copy_from_slice(sum.as_bytes());
    }
    seed_both("v2_refused", &img);
}
