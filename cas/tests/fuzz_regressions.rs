//! Regression tests for findings surfaced by the storage-server and cas
//! fuzz targets, rooted in cas. Each pins the hardened behavior so the
//! finding cannot silently regress. See doc/results/1_fuzzing-findings.md.

use cas::chunk::ChunkerParams;
use cas::dev::{BlockDev, MemDev};
use cas::disk::{
    decode_chunk_header, decode_index, encode_chunk_frame, encode_index, IndexEntry, Superblock,
    WalOp, CHUNK_HEADER, SB_A_OFF, SB_B_OFF, SB_SIZE, WAL_OFF,
};
use cas::hash::Hash;
use cas::store::{Store, StoreError, StoreOptions};

fn small_opts() -> StoreOptions {
    StoreOptions {
        wal_len: 4096,
        chunker: ChunkerParams { min: 64, avg: 256, max: 1024 },
        overlay_budget: 16 * 1024,
    }
}

fn fresh() -> Store<MemDev> {
    let mut store = Store::format(MemDev::new(64 * 1024), small_opts()).unwrap();
    store.create_ref(b"main").unwrap();
    store
}

/// FINDING OVL-1 (fixed): a write at `offset` near u64::MAX used to panic
/// with an arithmetic overflow in `FileOverlay::insert` (`off + data.len()`,
/// overlay.rs), reachable from a `Write` request — a client with write
/// access could crash the storage server with one message (found by the
/// `request_dispatch` target). `Store::write` now rejects the extent before
/// it reaches the WAL.
#[test]
fn ovl1_write_offset_overflow_rejected() {
    let mut store = fresh();
    let path = vec![b"f".to_vec()];
    let r = store.write(b"main", &path, u64::MAX, b"x", 1);
    assert!(matches!(r, Err(StoreError::WriteOutOfRange)), "got {r:?}");
    // The store must survive the rejection intact.
    store.write(b"main", &path, 0, b"hello", 2).unwrap();
    assert_eq!(store.read(b"main", &path).unwrap(), Some(b"hello".to_vec()));
}

/// Companion to OVL-1: an extent that cannot overflow but exceeds the chunk
/// region capacity is rejected too — accepting it would ack a WAL record
/// that can never flush (and would materialize the whole extent in
/// `FileOverlay::apply`).
#[test]
fn ovl1_write_extent_beyond_capacity_rejected() {
    let mut store = fresh();
    let path = vec![b"f".to_vec()];
    let r = store.write(b"main", &path, 1 << 40, b"x", 1);
    assert!(matches!(r, Err(StoreError::WriteOutOfRange)), "got {r:?}");
    store.write(b"main", &path, 0, b"hello", 2).unwrap();
    assert_eq!(store.read(b"main", &path).unwrap(), Some(b"hello".to_vec()));
}

// ── MNT-1: mount must not trust superblock geometry ─────────────────────
//
// The superblock checksum is integrity, not authenticity: it distinguishes
// torn writes from complete ones, and anyone who can place bytes on the
// device can re-seal it (`Superblock::encode` recomputes it — as these
// forgeries do). Mount's offset/length fields are therefore untrusted until
// validated against the device length, the one ground truth mount has.

/// A small committed image, dumped to raw bytes for forging.
fn image() -> Vec<u8> {
    let mut store = fresh();
    store.write(b"main", &vec![b"f".to_vec()], 0, b"hello", 1).unwrap();
    store.sync_all().unwrap();
    let dev = store.into_dev();
    let mut img = vec![0u8; dev.len() as usize];
    dev.read(0, &mut img).unwrap();
    img
}

/// Decode both slots the way mount does and return the winner + its offset.
fn winner(img: &[u8]) -> (Superblock, usize) {
    let a = Superblock::decode(&img[SB_A_OFF as usize..][..SB_SIZE]);
    let b = Superblock::decode(&img[SB_B_OFF as usize..][..SB_SIZE]);
    match (a, b) {
        (Some(a), Some(b)) if a.generation >= b.generation => (a, SB_A_OFF as usize),
        (Some(_), Some(b)) => (b, SB_B_OFF as usize),
        (Some(a), None) => (a, SB_A_OFF as usize),
        (None, Some(b)) => (b, SB_B_OFF as usize),
        (None, None) => panic!("fixture image has no valid superblock"),
    }
}

fn replace_sb(img: &mut [u8], slot: usize, sb: &Superblock) {
    img[slot..slot + SB_SIZE].copy_from_slice(&sb.encode());
}

fn mount(img: Vec<u8>) -> Result<Store<MemDev>, StoreError> {
    Store::mount(MemDev::from_bytes(img), small_opts())
}

/// The reviewer-named regression: a checksum-valid superblock whose
/// `chunk_tail` claims more than the device holds must be rejected with a
/// specific error *before any sized allocation* — `chunk_tail` was the
/// "ground truth" the index-frame length gate validated against, and it was
/// itself validated against nothing (untrusted data vouching for untrusted
/// data). Pre-fix this mounted Ok and left a store whose first allocation
/// would trap on `tail + need`.
#[test]
fn mnt1_forged_chunk_tail_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    sb.chunk_tail = u64::MAX - 64;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("committed chunk region exceeds device"))),
        "got {r:?}"
    );

    // The wrapping variant: chunk_off + chunk_tail overflows u64, so an
    // unchecked bound would pass spuriously.
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    sb.chunk_tail = u64::MAX;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("committed chunk region exceeds device"))),
        "got {r:?}"
    );
}

/// Not every forgeable scalar is geometry: `generation` feeds
/// `birth_gen = generation + 1` at mount (and `generation + 1` at every
/// commit). A re-sealed superblock with `generation = u64::MAX` overflowed
/// that derive — the second crash `mount_reseal` found, after the geometry
/// fields were already covered.
#[test]
fn mnt1_forged_generation_max_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    sb.generation = u64::MAX;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("superblock generation exhausted"))),
        "got {r:?}"
    );
}

/// `wal_len` near u64::MAX used to overflow `WAL_OFF + sb.wal_len` at the
/// very first use of a forged field — the path the `mount_reseal` fuzz
/// target found within two minutes of being pointed at pre-fix code.
#[test]
fn mnt1_forged_wal_len_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    sb.wal_len = u64::MAX - 4096;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("wal region exceeds device"))),
        "got {r:?}"
    );
}

/// `index_off` near u64::MAX used to overflow `chunk_off + sb.index_off`
/// before the device read (a trap under overflow-checks, a wild read
/// without them).
#[test]
fn mnt1_forged_index_off_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    sb.index_off = u64::MAX - 8;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("index frame outside committed region"))),
        "got {r:?}"
    );
}

/// `wal_head` beyond the WAL region used to panic the replay scan's slice
/// (`&wal[off..]` with off > len).
#[test]
fn mnt1_forged_wal_head_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    sb.wal_head = sb.wal_len + 1;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("wal head beyond wal region"))),
        "got {r:?}"
    );
}

/// `wal_next_seq = u64::MAX` plus a (re-sealed) record at that seq drove
/// the replay loop's `seq += 1` past u64::MAX — the third `mount_reseal`
/// crash, in replay rather than setup. A forged WAL record at this seq is
/// rejected, not silently treated as an unacked tail.
#[test]
fn mnt1_forged_wal_seq_max_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    let rec = WalOp::Write {
        ref_name: b"main".to_vec(),
        path: vec![b"f".to_vec()],
        offset: 0,
        mtime: 9,
        data: vec![b'x'],
    }
    .encode_record(u64::MAX);
    let at = (WAL_OFF + sb.wal_head) as usize;
    img[at..at + rec.len()].copy_from_slice(&rec);
    sb.wal_next_seq = u64::MAX;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("wal sequence exhausted"))),
        "got {r:?}"
    );
}

/// A forged WAL record with an OVL-1-shaped extent. `Store::write` rejects
/// such an extent before it is logged, so no image this code produced
/// contains one — but WAL record checksums cover only the payload, so a
/// disk-writing adversary can plant a perfectly sealed record (the
/// superblock checksum does not cover the WAL region; no re-seal even
/// needed). Pre-fix, replay applied it straight into the overlay and
/// trapped on `off + data.len()` at the original OVL-1 site.
#[test]
fn mnt1_forged_wal_record_extent_rejected() {
    let mut img = image();
    let (sb, _) = winner(&img);
    let rec = WalOp::Write {
        ref_name: b"main".to_vec(),
        path: vec![b"f".to_vec()],
        offset: u64::MAX,
        mtime: 9,
        data: vec![b'x'],
    }
    .encode_record(sb.wal_next_seq);
    let at = (WAL_OFF + sb.wal_head) as usize;
    img[at..at + rec.len()].copy_from_slice(&rec);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("wal record extent out of range"))),
        "got {r:?}"
    );
}

/// The spurious-pass shape inside the index itself: an entry with
/// `off + len` wrapping u64 used to pass `off + len <= chunk_tail` (the sum
/// wrapped to a tiny value) and mount returned Ok with a poisoned index —
/// any later read of that hash issued a wild device access. The whole forged
/// frame is re-sealed (payload hash recomputed), as a disk-writer would.
#[test]
fn mnt1_forged_index_entry_wrap_rejected() {
    let mut img = image();
    let (mut sb, slot) = winner(&img);
    let chunk_off = (WAL_OFF + sb.wal_len) as usize;
    let hdr_at = chunk_off + sb.index_off as usize;
    let (ilen, birth, _) = decode_chunk_header(&img[hdr_at..hdr_at + CHUNK_HEADER]).unwrap();
    let payload = &img[hdr_at + CHUNK_HEADER..hdr_at + CHUNK_HEADER + ilen];
    let (mut entries, free) = decode_index(payload).unwrap();
    entries.insert(
        Hash::of(b"poison"),
        IndexEntry { off: u64::MAX - 4, len: 16, birth: 1 },
    );
    let forged = encode_index(&entries, &free, 0);
    let frame = encode_chunk_frame(&forged, birth, &Hash::of(&forged));
    let new_off = sb.chunk_tail;
    let at = chunk_off + new_off as usize;
    img[at..at + frame.len()].copy_from_slice(&frame);
    sb.index_off = new_off;
    sb.chunk_tail = new_off + frame.len() as u64;
    replace_sb(&mut img, slot, &sb);
    let r = mount(img).err();
    assert!(
        matches!(r, Some(StoreError::Corrupt("index entry out of bounds"))),
        "got {r:?}"
    );
}
