//! Host tests for the mkfs directory walk (rev2§6 Baseline tier).
//!
//! The walk (`populate`) and the name rule (`name_acceptable`) are driven
//! in-process against an in-memory `MemDev` store (no per-case disk file).
//! Two tiers:
//!
//! * `name_acceptable` — golden boundary units + a Miri-able proptest over
//!   arbitrary bytes (the rev2§4.9 printable-ASCII rule, including the `'/'`
//!   and non-UTF-8 rejections, which the FS cannot materialize as a name).
//! * `populate` — proptests over generated temp-dir trees against a
//!   mount-equality / skip / count / total oracle, plus a creation-order
//!   determinism property (rev2§6 canonical-form prose) and a negative
//!   control proving the oracle has teeth.
//!
//! Platform note (the walk tier): the dev host is macOS (APFS) — a `'/'`/NUL
//! byte is not a materializable filename, non-UTF-8 names may be rejected,
//! and APFS is case-insensitive. So generated names are drawn from
//! FS-materializable, case-collision-free bytes: accepted = `[a-z0-9]`,
//! rejected = a control char in `0x01..0x20`/`0x7F` mixed with `[a-z0-9]`.
//! The `'/'`/non-UTF-8 rejections live in the `name_acceptable` proptest.

use super::*;
use cas::dev::MemDev;
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

// A device comfortably above the format floor (WAL_OFF + 1 MiB WAL +
// MIN_CHUNK_REGION); generated content is small, so this is plenty.
const DEV_LEN: usize = 8 * 1024 * 1024;

fn osb(bytes: &[u8]) -> &OsStr {
    OsStr::from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// name_acceptable — golden boundary units + the rev2§4.9 rule proptest
// ---------------------------------------------------------------------------

#[test]
fn name_acceptable_golden_boundaries() {
    // Accepted: printable ASCII 0x20..=0x7E.
    assert_eq!(name_acceptable(osb(b" ")), Some(" ")); // 0x20 (low boundary)
    assert_eq!(name_acceptable(osb(b"~")), Some("~")); // 0x7E (high boundary)
    assert_eq!(name_acceptable(osb(b"file.txt")), Some("file.txt"));
    assert_eq!(name_acceptable(osb(b"a b")), Some("a b")); // interior space ok

    // Rejected: control chars and the 0x7F/0x1F boundaries.
    assert!(name_acceptable(osb(&[0x1F])).is_none()); // unit separator
    assert!(name_acceptable(osb(&[0x7F])).is_none()); // DEL (above 0x7E)
    assert!(name_acceptable(osb(b"\n")).is_none()); // newline
    assert!(name_acceptable(osb(b"a\tb")).is_none()); // interior tab

    // Rejected: embedded '/' (0x2F is printable but excluded by the rule).
    assert!(name_acceptable(osb(b"a/b")).is_none());

    // Rejected: not UTF-8.
    assert!(name_acceptable(osb(&[0xFF])).is_none());
    assert!(name_acceptable(osb(&[0x80])).is_none());
    assert!(name_acceptable(osb(&[0xC3, 0x28])).is_none()); // invalid 2-byte seq
}

#[test]
fn name_acceptable_empty_is_vacuously_accepted() {
    // read_dir never yields an empty name, but the predicate is vacuously
    // true on it (`all` over no bytes, no '/'); pin current behaviour so a
    // future change to it is deliberate.
    assert_eq!(name_acceptable(OsStr::new("")), Some(""));
}

proptest! {
    // Miri-able (no FS): the name rule over arbitrary bytes.
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// `name_acceptable` accepts a byte string iff it is valid UTF-8, every
    /// byte is in `0x20..0x7F`, and it contains no `'/'` (rev2§4.9).
    #[test]
    fn name_acceptable_matches_rule(bytes in proptest::collection::vec(any::<u8>(), 0..16)) {
        let got = name_acceptable(osb(&bytes)).is_some();
        let want = std::str::from_utf8(&bytes).is_ok()
            && bytes.iter().all(|&b| (0x20..0x7F).contains(&b))
            && !bytes.contains(&b'/');
        prop_assert_eq!(got, want, "bytes={:?}", bytes);
    }
}

// ---------------------------------------------------------------------------
// In-memory tree model, materializer, and the expected-contents oracle
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Node {
    Dir(BTreeMap<Vec<u8>, Node>),
    File(Vec<u8>),
    /// A non-regular entry (materialized as a symlink) — must be skipped.
    Symlink,
}

/// A name acceptable to both the FS (lowercase ASCII + digits) and the
/// rev2§4.9 rule; case-collision-free for APFS.
fn accepted_name() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(prop_oneof![0x61u8..0x7B, 0x30u8..0x3A], 1..4)
}

/// A name the FS will materialize but the rule rejects: at least one control
/// char (valid single-byte UTF-8, so APFS accepts it; non-printable, so the
/// rule rejects it). `'/'` and non-UTF-8 names are not FS-materializable —
/// they are covered by `name_acceptable_matches_rule`.
fn rejected_name() -> impl Strategy<Value = Vec<u8>> {
    (
        proptest::collection::vec(0x61u8..0x7B, 0..3),
        prop_oneof![Just(0x01u8), Just(0x07u8), Just(0x1Fu8), Just(0x7Fu8)],
        proptest::collection::vec(0x61u8..0x7B, 0..3),
    )
        .prop_map(|(a, c, b)| {
            let mut v = a;
            v.push(c);
            v.extend(b);
            v
        })
}

fn name_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![3 => accepted_name(), 1 => rejected_name()]
}

fn node_strategy() -> impl Strategy<Value = Node> {
    let leaf = prop_oneof![
        3 => proptest::collection::vec(any::<u8>(), 0..1024).prop_map(Node::File),
        1 => Just(Node::Symlink),
    ];
    // depth 3, ~16 nodes, ~4 children per dir — bounded for proptest speed.
    leaf.prop_recursive(3, 16, 4, |inner| {
        proptest::collection::vec((name_strategy(), inner), 0..4)
            .prop_map(|kids| Node::Dir(kids.into_iter().collect()))
    })
}

/// The source directory: a map of named children (files/dirs/symlinks).
fn tree_strategy() -> impl Strategy<Value = BTreeMap<Vec<u8>, Node>> {
    proptest::collection::vec((name_strategy(), node_strategy()), 0..5)
        .prop_map(|kids| kids.into_iter().collect())
}

/// A temp dir removed on drop (survives a proptest panic mid-case).
struct TempTree(PathBuf);

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn unique_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "eunomia-mkfs-walk-{}-{}-{}",
        std::process::id(),
        tag,
        n
    ))
}

/// Materialize a model tree into `dir`. `reverse` flips the creation order at
/// every level (for the determinism property); `populate`'s `sort_by_key`
/// makes the resulting mount order-independent.
fn materialize_into(
    dir: &std::path::Path,
    tree: &BTreeMap<Vec<u8>, Node>,
    reverse: bool,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = tree.iter().collect();
    if reverse {
        entries.reverse();
    }
    for (name, node) in entries {
        let path = dir.join(osb(name));
        match node {
            Node::Dir(children) => {
                std::fs::create_dir(&path)?;
                materialize_into(&path, children, reverse)?;
            }
            Node::File(data) => std::fs::write(&path, data)?,
            // DirEntry::metadata does not follow symlinks, so a dangling
            // target is fine — the entry is non-regular and skipped.
            Node::Symlink => symlink("dangling-target", &path)?,
        }
    }
    Ok(())
}

/// Walk the model and split every leaf into the paths that must be present
/// (accepted regular files, all ancestors accepted → contents) and the paths
/// that must be absent (rejected names, symlinks, anything under a rejected
/// ancestor → the `continue`-before-push subtree skip).
fn classify(
    tree: &BTreeMap<Vec<u8>, Node>,
    prefix: &mut Vec<Vec<u8>>,
    ancestors_ok: bool,
    present: &mut BTreeMap<Vec<Vec<u8>>, Vec<u8>>,
    absent: &mut Vec<Vec<Vec<u8>>>,
) {
    for (name, node) in tree {
        let ok = ancestors_ok && name_acceptable(osb(name)).is_some();
        prefix.push(name.clone());
        match node {
            Node::Dir(children) => classify(children, prefix, ok, present, absent),
            Node::File(data) => {
                if ok {
                    present.insert(prefix.clone(), data.clone());
                } else {
                    absent.push(prefix.clone());
                }
            }
            Node::Symlink => absent.push(prefix.clone()),
        }
        prefix.pop();
    }
}

/// The mount-equality oracle: every present path reads back its contents,
/// every absent path reads back nothing, and the returned count equals the
/// number of accepted regular files. Returns `Err(reason)` on any mismatch
/// so the negative control can prove it has teeth.
fn check_mount(
    store: &Store<MemDev>,
    present: &BTreeMap<Vec<Vec<u8>>, Vec<u8>>,
    absent: &[Vec<Vec<u8>>],
    count: u64,
) -> Result<(), String> {
    for (path, want) in present {
        let got = store
            .read(b"main", path)
            .map_err(|e| format!("read({:?}): {e}", path))?;
        if got.as_deref() != Some(want.as_slice()) {
            return Err(format!(
                "content mismatch at {:?}: want {} bytes, got {:?} bytes",
                path,
                want.len(),
                got.map(|v| v.len())
            ));
        }
    }
    for path in absent {
        let got = store
            .read(b"main", path)
            .map_err(|e| format!("read({:?}): {e}", path))?;
        if got.is_some() {
            return Err(format!("path {:?} should be absent but is present", path));
        }
    }
    if count as usize != present.len() {
        return Err(format!(
            "count {} != accepted regular files {}",
            count,
            present.len()
        ));
    }
    Ok(())
}

/// Build a fresh MemDev store, populate it from a freshly materialized copy
/// of `tree`, and return the (held) temp dir, the store, and the count.
fn build(
    tree: &BTreeMap<Vec<u8>, Node>,
    reverse: bool,
    tag: &str,
) -> (TempTree, Store<MemDev>, u64) {
    let dir = TempTree(unique_dir(tag));
    std::fs::create_dir(&dir.0).unwrap();
    materialize_into(&dir.0, tree, reverse).unwrap();

    let mut store = Store::format(MemDev::new(DEV_LEN), batch_store_options()).unwrap();
    store.create_ref(b"main").unwrap();
    let mut prefix = Vec::new();
    let count = populate(&mut store, &dir.0, &mut prefix).unwrap();
    (dir, store, count)
}

proptest! {
    // Native: the walk uses real `read_dir` over a temp tree. The cfg!(miri)
    // branch keeps it portable, but mkfs is not in the standing Miri sweep
    // (the walk's CAS path is Miri-covered under `-p cas`; the Miri-able mkfs
    // tier is `name_acceptable_matches_rule`).
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// The walk maps the host tree faithfully: accepted regular files are
    /// present with their contents, rejected names / symlinks / subtrees are
    /// absent, the count is right, and `populate` never fails (refuse-not-
    /// crash) on any adversarial tree.
    #[test]
    fn walk_maps_tree_faithfully(tree in tree_strategy()) {
        let (_dir, store, count) = build(&tree, false, "walk");

        let mut present = BTreeMap::new();
        let mut absent = Vec::new();
        classify(&tree, &mut Vec::new(), true, &mut present, &mut absent);

        let report = check_mount(&store, &present, &absent, count);
        prop_assert!(report.is_ok(), "{}", report.unwrap_err());
    }

    /// rev2§6 canonical-form: the same logical tree, materialized in opposite
    /// creation orders, mounts to identical logical contents (mkfs's half of
    /// the history-independence property; the prolly tree covers the other half).
    #[test]
    fn walk_is_creation_order_independent(tree in tree_strategy()) {
        let (_da, sa, ca) = build(&tree, false, "det-a");
        let (_db, sb, cb) = build(&tree, true, "det-b");
        prop_assert_eq!(ca, cb, "count differs across creation orders");

        let mut present = BTreeMap::new();
        let mut absent = Vec::new();
        classify(&tree, &mut Vec::new(), true, &mut present, &mut absent);
        for path in present.keys() {
            let ra = sa.read(b"main", path).unwrap();
            let rb = sb.read(b"main", path).unwrap();
            prop_assert_eq!(&ra, &rb, "path {:?} differs across creation orders", path);
        }
    }
}

/// Negative control (anti-theater): the oracle must reject a tampered
/// expectation, so a real walk regression cannot pass silently.
#[test]
fn oracle_has_teeth() {
    let mut tree = BTreeMap::new();
    tree.insert(b"a.txt".to_vec(), Node::File(b"hello".to_vec()));
    let mut sub = BTreeMap::new();
    sub.insert(b"b.bin".to_vec(), Node::File(b"world".to_vec()));
    tree.insert(b"sub".to_vec(), Node::Dir(sub));
    // A rejected sibling and a symlink, to give `absent` real entries.
    tree.insert(b"bad\x01name".to_vec(), Node::File(b"x".to_vec()));
    tree.insert(b"link".to_vec(), Node::Symlink);

    let (_dir, store, count) = build(&tree, false, "teeth");

    let mut present = BTreeMap::new();
    let mut absent = Vec::new();
    classify(&tree, &mut Vec::new(), true, &mut present, &mut absent);

    // The honest oracle passes.
    assert_eq!(count, 2);
    assert!(check_mount(&store, &present, &absent, count).is_ok());

    // Tamper 1: corrupt expected contents.
    let mut bad = present.clone();
    *bad.get_mut(&vec![b"a.txt".to_vec()]).unwrap() = b"TAMPERED".to_vec();
    assert!(
        check_mount(&store, &bad, &absent, count).is_err(),
        "corrupted contents must be caught"
    );

    // Tamper 2: claim a present file is absent.
    let mut bad_absent = absent.clone();
    bad_absent.push(vec![b"a.txt".to_vec()]);
    assert!(
        check_mount(&store, &present, &bad_absent, count).is_err(),
        "present file claimed absent must be caught"
    );

    // Tamper 3: wrong count.
    assert!(
        check_mount(&store, &present, &absent, count + 1).is_err(),
        "wrong count must be caught"
    );
}
