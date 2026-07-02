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

//! Rights-lattice property tests (spec rev2§2.3). The example
//! tests in `sessions.rs` pin individual derivations; these pin the *laws*:
//!
//!  1. attenuation across arbitrary `OpenChild` chains is monotone — every
//!     child's rights are exactly `parent & mask`, so no chain ever gains a
//!     right (`stat-store` strip is the headline case);
//!  2. each gate matches its bit behaviorally — `statfs ⇔ stat-store`,
//!     `read ⇔ read`, `gc ⇔ rewrite-history`;
//!  3. `stat-store`'s scope ignores the subtree — it observes the whole store
//!     even through a deep subtree handle;
//!  4. `OpenSnapshot` collapses any handle to read/enumerate only.

use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use proptest::prelude::*;
use storage_server::*;

fn p(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|s| s.as_bytes().to_vec()).collect()
}

/// Same `top.txt` / `pub/readme` / `pub/deep/leaf` tree the `sessions.rs`
/// example tests use, so the directory shape the chains descend is familiar.
fn new_server() -> Server<MemDev> {
    let opts = StoreOptions {
        wal_len: 64 * 1024,
        ..StoreOptions::default()
    };
    let mut store = Store::format(MemDev::new(1 << 20), opts).unwrap();
    store.create_ref(b"main").unwrap();
    store
        .write(b"main", &p(&["top.txt"]), 0, b"top secret", 1)
        .unwrap();
    store
        .write(b"main", &p(&["pub", "readme"]), 0, b"public info", 2)
        .unwrap();
    store
        .write(b"main", &p(&["pub", "deep", "leaf"]), 0, b"leaf data", 3)
        .unwrap();
    store.sync_all().unwrap();
    Server::new(store, 0xC0FFEE)
}

proptest! {
    // Miri: a handful of cases cover the same arithmetic; native keeps the
    // full sweep (mirrors cas/src/overlay.rs).
    #![proptest_config(ProptestConfig {
        cases: if cfg!(miri) { 4 } else { 256 },
        ..ProptestConfig::default()
    })]

    /// Property 1 — `OpenChild` attenuation is monotone across arbitrary
    /// derivation chains: every child's rights are exactly `parent & mask`,
    /// and no chain ever gains a right.
    #[test]
    fn open_child_attenuation_is_monotone(
        ops in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..40),
    ) {
        let mut srv = new_server();
        let root = srv.root_grant(b"main").unwrap();
        let session = srv.open_session(vec![root]);
        // (handle id, expected rights) for every live ref handle so far.
        let mut handles: Vec<(HandleId, u8)> = vec![(0, R_ALL | R_STAT_STORE)];
        let mut now = 0u64;
        for (sel, mask) in ops {
            now += 1;
            let (parent_id, parent_rights) = handles[sel % handles.len()];
            // Empty path => same node (`validate_path([])` is `Ok`); `OpenChild`
            // needs no rights, so even a zero-rights handle keeps deriving and
            // the chain can grow arbitrarily long.
            let resp = srv.handle(
                session,
                Request::OpenChild { handle: parent_id, path: vec![], rights_mask: mask },
                now,
            );
            let child = match resp {
                Response::Handle(h) => h,
                other => {
                    prop_assert!(false, "OpenChild returned non-handle: {:?}", other);
                    unreachable!()
                }
            };
            // Oracle written as a literal `&`, independent of the lib's
            // `attenuate` — flipping that to `|` makes this fail.
            let expected = parent_rights & mask;
            prop_assert_eq!(srv.handle_rights(session, child), Some(expected));
            // No chain ever gains a right: child ⊆ parent.
            prop_assert_eq!(expected & !parent_rights, 0u8);
            // stat-store strip: absent on the parent, or omitted from the mask,
            // means absent on the child.
            if parent_rights & R_STAT_STORE == 0 {
                prop_assert_eq!(expected & R_STAT_STORE, 0u8);
            }
            if mask & R_STAT_STORE == 0 {
                prop_assert_eq!(expected & R_STAT_STORE, 0u8);
            }
            handles.push((child, expected));
        }
    }

    /// Property 2 — each gate matches its bit behaviorally. Probe a handle
    /// carrying exactly the generated rights and check the iff for the three
    /// representative gates; the mutating `gc` probe runs last so it can't
    /// disturb the read-only ones.
    #[test]
    fn gate_matches_bit(rights in any::<u8>()) {
        let mut srv = new_server();
        let h = HandleEntry { rights, ..srv.root_grant(b"main").unwrap() };
        let session = srv.open_session(vec![h]);

        // statfs ⇔ stat-store (scope ignores the subtree).
        let statfs = srv.handle(session, Request::Statfs { handle: 0 }, 1);
        prop_assert_eq!(
            matches!(statfs, Response::Space { .. }),
            rights & R_STAT_STORE != 0
        );

        // read ⇔ read. Rights are checked before path resolution, so a hit
        // (`Data`) and a `NotFound` both count as "allowed"; only a missing
        // right yields `Denied`. `top.txt` exists, so allowed => `Data`.
        let read = srv.handle(
            session,
            Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: u32::MAX },
            2,
        );
        prop_assert_eq!(read != Response::Err(ErrorCode::Denied), rights & R_READ != 0);

        // gc ⇔ rewrite-history (mutates the store; run last).
        let gc = srv.handle(session, Request::Gc { handle: 0 }, 3);
        prop_assert_eq!(
            gc != Response::Err(ErrorCode::Denied),
            rights & R_REWRITE_HISTORY != 0
        );
    }

    /// Property 4 — `OpenSnapshot` collapses any handle to read/enumerate. The
    /// resulting rights are `rights & mask & (R_READ | R_ENUMERATE)`, so a
    /// snapshot handle never carries store-global or mutating rights.
    #[test]
    fn snapshot_handle_drops_to_read_enumerate(rights in any::<u8>(), mask in any::<u8>()) {
        let mut srv = new_server();
        let root = srv.root_grant(b"main").unwrap();
        let under_test = HandleEntry { rights, ..srv.root_grant(b"main").unwrap() };
        // handle 0 = full root (mints the snapshot); handle 1 = parent under test.
        let session = srv.open_session(vec![root, under_test]);

        let id = match srv.handle(
            session,
            Request::Snapshot { handle: 0, message: b"p".to_vec(), class: cas::disk::CLASS_AUTO },
            1,
        ) {
            Response::SnapId(id) => id,
            other => {
                prop_assert!(false, "snapshot mint failed: {:?}", other);
                unreachable!()
            }
        };

        let resp = srv.handle(
            session,
            Request::OpenSnapshot { handle: 1, snap_id: id, path: vec![], rights_mask: mask },
            2,
        );
        if rights & R_READ == 0 {
            // `OpenSnapshot` needs read on the parent.
            prop_assert_eq!(resp, Response::Err(ErrorCode::Denied));
        } else {
            let snap = match resp {
                Response::Handle(h) => h,
                other => {
                    prop_assert!(false, "OpenSnapshot returned non-handle: {:?}", other);
                    unreachable!()
                }
            };
            let expected = rights & mask & (R_READ | R_ENUMERATE);
            prop_assert_eq!(srv.handle_rights(session, snap), Some(expected));
            // Never store-global, write, snapshot, or history-rewriting.
            prop_assert_eq!(expected & R_STAT_STORE, 0u8);
            prop_assert_eq!(expected & R_WRITE, 0u8);
            prop_assert_eq!(expected & R_SNAPSHOT, 0u8);
            prop_assert_eq!(expected & R_REWRITE_HISTORY, 0u8);
        }
    }
}

/// Property 3 — `stat-store`'s scope ignores the subtree. A deep subtree handle
/// that explicitly carries the bit observes the *same* whole-store space as the
/// root handle (rev2§2.3). Deterministic, so a plain `#[test]`.
#[test]
fn stat_store_scope_ignores_subtree() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let session = srv.open_session(vec![root]);

    // Whole-store space through the privileged root handle.
    let Response::Space { total, used, free } =
        srv.handle(session, Request::Statfs { handle: 0 }, 1)
    else {
        panic!("root statfs should succeed");
    };

    // A deep subtree handle that explicitly re-requests stat-store keeps it
    // (intersection retains it: the holder has it and the mask sets bit 5).
    let Response::Handle(deep) = srv.handle(
        session,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub", "deep"]),
            rights_mask: R_ALL | R_STAT_STORE,
        },
        2,
    ) else {
        panic!("opening the deep subtree child should succeed");
    };
    assert_eq!(srv.handle_rights(session, deep), Some(R_ALL | R_STAT_STORE));

    // statfs through the deep handle observes the whole store, not the subtree.
    let Response::Space {
        total: t2,
        used: u2,
        free: f2,
    } = srv.handle(session, Request::Statfs { handle: deep }, 3)
    else {
        panic!("deep statfs denied — stat-store should have survived the descent");
    };
    assert_eq!((total, used, free), (t2, u2, f2));
}
