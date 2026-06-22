//! Replay the committed fuzz corpus through the same decoders the cargo-fuzz
//! targets drive. This keeps every fuzz-discovered (and seed) input alive as
//! an ordinary test even where libFuzzer doesn't run, and — the cheap
//! compounding trick — makes `cargo miri test -p cas` UB-check each one.
//!
//! Mirrors the harness oracles so the canonical-form properties are checked
//! here too. The mount target hashes whole images, so it is skipped under
//! Miri (prohibitively slow); the byte-level decoders it relies on are
//! exercised directly by the other targets.

use std::fs;
use std::path::PathBuf;

use cas::dev::MemDev;
use cas::disk::{decode_index, encode_index, RefTable, Superblock, WalOp};
use cas::prolly::{parse_node, NodeRefs};
use cas::store::{Store, StoreOptions};

fn corpus_files(target: &str) -> Vec<Vec<u8>> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus");
    dir.push(target);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new(); // corpus not present (e.g. fresh checkout): skip
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
        .map(|e| fs::read(e.path()).unwrap())
        .collect()
}

#[test]
fn tlv_entry() {
    for data in corpus_files("tlv_entry") {
        if let Ok(entry) = cas::tlv::decode(&data) {
            assert_eq!(
                cas::tlv::encode(&entry),
                data,
                "non-canonical entry in corpus"
            );
        }
    }
}

#[test]
fn tree_node() {
    for data in corpus_files("tree_node") {
        if let Ok(NodeRefs::Entries(entries)) = parse_node(&data) {
            let mut re = vec![0u8];
            re.extend_from_slice(&(entries.len() as u32).to_le_bytes());
            for e in &entries {
                re.extend_from_slice(&cas::tlv::encode(e));
            }
            assert_eq!(re, data, "non-canonical leaf node in corpus");
        }
    }
}

#[test]
fn gc_mark() {
    // Same driver the `gc_mark` target runs: each recipe builds a store of
    // tree nodes and marks it; the walk must never panic/overflow, and on
    // success the mark set must read back everything reachable (rev1§4.6/§6).
    for data in corpus_files("gc_mark") {
        cas::gc::check_recipe(&data).unwrap();
    }
}

#[test]
fn index_frame() {
    for data in corpus_files("index_frame") {
        if let Ok((entries, free)) = decode_index(&data) {
            let bytes = encode_index(&entries, &free, 0);
            assert_eq!(
                decode_index(&bytes).unwrap(),
                (entries, free),
                "index not stable"
            );
        }
    }
}

#[test]
fn ref_table() {
    for data in corpus_files("ref_table") {
        if let Ok(table) = RefTable::decode(&data) {
            assert_eq!(
                RefTable::decode(&table.encode()).unwrap(),
                table,
                "ref table not stable"
            );
        }
    }
}

#[test]
fn superblock() {
    for target in ["superblock", "superblock_fixup"] {
        for data in corpus_files(target) {
            if let Some(sb) = Superblock::decode(&data) {
                assert_eq!(
                    Superblock::decode(&sb.encode()),
                    Some(sb),
                    "superblock not stable"
                );
            }
        }
    }
}

#[test]
fn wal_replay_scan() {
    for target in ["wal_replay_scan", "wal_replay_scan_fixup"] {
        for data in corpus_files(target) {
            let mut off = 0usize;
            while off < data.len() {
                let Some((seq, op, rlen)) = WalOp::decode_record(&data[off..]) else {
                    break;
                };
                assert_eq!(op.encode_record(seq).as_slice(), &data[off..off + rlen]);
                off += rlen;
            }
        }
    }
}

/// Generator for the C2B tag-3 (Rename) corpus seed (`corpus/wal_replay_scan/
/// rename`): a Write then a Rename record so the fuzzer — and the Miri replay
/// in `wal_replay_scan` above — exercise the new rename decode path. `#[ignore]`d
/// because it rewrites a committed file; run explicitly to regenerate:
/// `cargo test -p cas --test fuzz_corpus gen_rename_corpus_seed -- --ignored`.
#[test]
#[ignore = "regenerates a committed corpus seed"]
fn gen_rename_corpus_seed() {
    let mut chain = WalOp::Write {
        ref_name: b"main".to_vec(),
        path: vec![b"a".to_vec()],
        offset: 0,
        mtime: 1,
        data: b"hello".to_vec(),
    }
    .encode_record(0);
    chain.extend_from_slice(
        &WalOp::Rename {
            ref_name: b"main".to_vec(),
            from: vec![b"a".to_vec()],
            to: vec![b"b".to_vec()],
            mtime: 2,
        }
        .encode_record(1),
    );
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dir.push("fuzz/corpus/wal_replay_scan");
    fs::write(dir.join("rename"), &chain).unwrap();
}

#[test]
fn chunker() {
    let params = cas::chunk::ChunkerParams {
        min: 64,
        avg: 256,
        max: 1024,
    };
    for data in corpus_files("chunker") {
        let cuts = cas::chunk::boundaries(&params, &data);
        let mut prev = 0;
        for &cut in &cuts {
            assert!(cut > prev && cut - prev <= params.max);
            prev = cut;
        }
        assert_eq!(prev, data.len());
    }
}

#[test]
fn mount_recovery() {
    if cfg!(miri) {
        return; // whole-image hashing under Miri is too slow
    }
    // The mount_reseal corpus is replayed raw (its re-seal helper is
    // fuzzing-feature-only) — same weakening as the superblock/wal `_fixup`
    // corpora above, and still the full total-mount property.
    for target in ["mount_recovery", "mount_reseal"] {
        for data in corpus_files(target) {
            let dev = MemDev::from_bytes(data);
            if let Ok(store) = Store::mount(dev, StoreOptions::default()) {
                let refs: Vec<Vec<u8>> = store.refs().map(|(n, _)| n.clone()).collect();
                for name in &refs {
                    let _ = store.list(name, &Vec::new());
                }
            }
        }
    }
}
