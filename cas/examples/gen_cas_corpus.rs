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
    for (n, e) in [
        ("inline", inline),
        ("exec", exec),
        ("dir", dir),
        ("chunked", chunked),
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
