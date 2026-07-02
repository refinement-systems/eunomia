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

//! Session/handle semantics tests (spec rev2§2.2-2.4): handle relativity,
//! subtree confinement, monotone attenuation, generation revocation,
//! one-shot tickets, audit.

use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use storage_server::*;

fn p(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|s| s.as_bytes().to_vec()).collect()
}

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

#[test]
fn handles_are_session_relative() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s1 = srv.open_session(vec![root.clone()]);
    let s2 = srv.open_session(vec![]);

    // Same integer, different sessions: handle 0 means something in s1,
    // nothing in s2 — the integers carry no authority (rev2§2.4).
    assert_eq!(
        srv.handle(
            s1,
            Request::Read {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            10
        ),
        Response::Data(b"top secret".to_vec())
    );
    assert_eq!(
        srv.handle(
            s2,
            Request::Read {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            10
        ),
        Response::Err(ErrorCode::BadHandle)
    );
}

#[test]
fn subtree_confinement_by_unreachability() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    let Response::Handle(sub) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_READ | R_WRITE,
        },
        10,
    ) else {
        panic!()
    };

    // Inside the subtree: fine.
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: sub,
                path: p(&["readme"]),
                offset: 0,
                len: u32::MAX
            },
            11
        ),
        Response::Data(b"public info".to_vec())
    );
    // The sibling simply has no name from here (rev2§2.3): the same path that
    // works on the root handle resolves under pub/ and finds nothing.
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: sub,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            11
        ),
        Response::NotFound
    );
    // ".." is path syntax, never stored — rejected outright (rev2§4.9).
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: sub,
                path: p(&["..", "top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            11
        ),
        Response::Err(ErrorCode::BadPath)
    );
    // Writes through the subtree handle land under the subtree.
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: sub,
                path: p(&["new"]),
                offset: 0,
                data: b"x".to_vec()
            },
            12
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["pub", "new"]),
                offset: 0,
                len: u32::MAX
            },
            13
        ),
        Response::Data(b"x".to_vec())
    );
}

#[test]
fn rename_moves_a_file_and_denies_what_it_should() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    // Happy path (rev2§4.9): write a, rename a -> b, b reads back, a is gone.
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: 0,
                path: p(&["a"]),
                offset: 0,
                data: b"x".to_vec()
            },
            10
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Rename {
                handle: 0,
                from: p(&["a"]),
                to: p(&["b"])
            },
            11
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["b"]),
                offset: 0,
                len: u32::MAX
            },
            12
        ),
        Response::Data(b"x".to_vec())
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["a"]),
                offset: 0,
                len: u32::MAX
            },
            13
        ),
        Response::NotFound
    );

    // A missing source fails cleanly as a path error (not a panic/Internal).
    assert_eq!(
        srv.handle(
            s,
            Request::Rename {
                handle: 0,
                from: p(&["a"]),
                to: p(&["c"])
            },
            14
        ),
        Response::Err(ErrorCode::BadPath)
    );

    // A subtree handle can never name a target outside its subtree: both ends
    // are resolved under the subtree, and ".." is rejected up front (rev2§4.9).
    let Response::Handle(sub) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_READ | R_WRITE,
        },
        15,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Rename {
                handle: sub,
                from: p(&["readme"]),
                to: p(&["..", "escaped"]),
            },
            16
        ),
        Response::Err(ErrorCode::BadPath)
    );
    // A legal move through the subtree handle stays confined under pub/.
    assert_eq!(
        srv.handle(
            s,
            Request::Rename {
                handle: sub,
                from: p(&["readme"]),
                to: p(&["moved"])
            },
            17
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["pub", "moved"]),
                offset: 0,
                len: u32::MAX
            },
            18
        ),
        Response::Data(b"public info".to_vec())
    );

    // A read-only handle cannot rename — the R_WRITE gate fires as Denied.
    let Response::Handle(ro) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_READ,
        },
        19,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Rename {
                handle: ro,
                from: p(&["moved"]),
                to: p(&["again"])
            },
            20
        ),
        Response::Err(ErrorCode::Denied)
    );
}

#[test]
fn attenuation_is_monotone() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    // Read-only child…
    let Response::Handle(ro) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_READ,
        },
        10,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: ro,
                path: p(&["z"]),
                offset: 0,
                data: b"n".to_vec()
            },
            11
        ),
        Response::Err(ErrorCode::Denied)
    );
    // …whose own children can never get write back (mask is ∩ only).
    let Response::Handle(child) = srv.handle(
        s,
        Request::OpenChild {
            handle: ro,
            path: p(&["deep"]),
            rights_mask: R_ALL,
        },
        12,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: child,
                path: p(&["w"]),
                offset: 0,
                data: b"n".to_vec()
            },
            13
        ),
        Response::Err(ErrorCode::Denied)
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: child,
                path: p(&["leaf"]),
                offset: 0,
                len: u32::MAX
            },
            14
        ),
        Response::Data(b"leaf data".to_vec())
    );
}

#[test]
fn generation_bump_revokes_all_handles_lazily() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s1 = srv.open_session(vec![root.clone()]);
    let s2 = srv.open_session(vec![root]);

    let Response::Handle(sub) = srv.handle(
        s2,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_ALL,
        },
        10,
    ) else {
        panic!()
    };

    // Mass revocation from s1: O(1), no enumeration of holders (rev2§2.2).
    assert_eq!(
        srv.handle(s1, Request::RevokeRef { handle: 0 }, 11),
        Response::Ok
    );

    // Every outstanding handle on the ref is stale on next use —
    // including the revoker's own and s2's derived subtree handle.
    for (sess, h) in [(s1, 0), (s2, 0), (s2, sub)] {
        assert_eq!(
            srv.handle(
                sess,
                Request::Read {
                    handle: h,
                    path: p(&["readme"]),
                    offset: 0,
                    len: u32::MAX
                },
                12
            ),
            Response::Err(ErrorCode::Stale),
            "session {sess} handle {h}"
        );
    }

    // Re-grant at the new generation works.
    let fresh = srv.root_grant(b"main").unwrap();
    let s3 = srv.open_session(vec![fresh]);
    assert_eq!(
        srv.handle(
            s3,
            Request::Read {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            13
        ),
        Response::Data(b"top secret".to_vec())
    );
}

#[test]
fn snapshots_are_immutable_and_survive_rollback() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"before".to_vec(),
            class: 0,
        },
        100,
    ) else {
        panic!()
    };

    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                data: b"CHANGEDNOW".to_vec()
            },
            101
        ),
        Response::Ok
    );

    // A snapshot handle scoped to the old state, read-only by nature.
    let Response::Handle(sh) = srv.handle(
        s,
        Request::OpenSnapshot {
            handle: 0,
            snap_id: snap,
            path: vec![],
            rights_mask: R_ALL,
        },
        102,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: sh,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            103
        ),
        Response::Data(b"top secret".to_vec())
    );
    // OpenSnapshot strips write rights unconditionally, so the rights
    // check (Denied) fires before the immutability backstop (ReadOnly).
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: sh,
                path: p(&["top.txt"]),
                offset: 0,
                data: vec![1]
            },
            104
        ),
        Response::Err(ErrorCode::Denied)
    );

    // Rollback needs may-rewrite-history and restores old content.
    assert_eq!(
        srv.handle(
            s,
            Request::Rollback {
                handle: 0,
                snap_id: snap
            },
            105
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            106
        ),
        Response::Data(b"top secret".to_vec())
    );

    // Provenance was server-assigned.
    let Response::Snapshots { snaps: rows, .. } =
        srv.handle(s, Request::ListSnapshots { handle: 0 }, 107)
    else {
        panic!()
    };
    assert!(rows[0].provenance.starts_with(b"session="));
}

/// rev2§4.7: `ListSnapshots` returns the ref's current edit version
/// alongside the rows, read in one call — so a retention daemon's enumerate and
/// its later guarded-batch `expected_version` come from one atomic snapshot of
/// the ref, and a concurrent mutation is detectable.
#[test]
fn list_snapshots_carries_the_ref_edit_version() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    // The reply's edit_version equals the ref's current version.
    let Response::Snapshots {
        edit_version: v0, ..
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 10)
    else {
        panic!()
    };
    assert_eq!(Some(v0), srv.store().edit_version(b"main"));

    // Taking a snapshot is an entry-set mutation: the next enumerate sees a
    // strictly higher version.
    let Response::SnapId(_) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"m".to_vec(),
            class: 0,
        },
        11,
    ) else {
        panic!()
    };
    let Response::Snapshots {
        snaps,
        edit_version: v1,
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 12)
    else {
        panic!()
    };
    assert!(
        v1 > v0,
        "snapshot must advance the edit version: {v0} -> {v1}"
    );
    assert_eq!(Some(v1), srv.store().edit_version(b"main"));
    assert_eq!(snaps.len(), 1);
}

#[test]
fn tickets_are_one_shot_with_ttl() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let alice = srv.open_session(vec![root]);
    let bob = srv.open_session(vec![]);

    let Response::Handle(sub) = srv.handle(
        alice,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_READ,
        },
        10,
    ) else {
        panic!()
    };
    let Response::Ticket(t1) = srv.handle(
        alice,
        Request::MintTicket {
            handle: sub,
            ttl_nanos: 1_000,
        },
        20,
    ) else {
        panic!()
    };

    // Bob redeems on his own session; attenuation traveled with it.
    let Response::Handle(bh) = srv.handle(bob, Request::RedeemTicket { ticket: t1 }, 25) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            bob,
            Request::Read {
                handle: bh,
                path: p(&["readme"]),
                offset: 0,
                len: u32::MAX
            },
            26
        ),
        Response::Data(b"public info".to_vec())
    );

    // One-shot: a second redemption fails.
    assert_eq!(
        srv.handle(bob, Request::RedeemTicket { ticket: t1 }, 27),
        Response::Err(ErrorCode::BadTicket)
    );

    // Expiry bounds the exposure window.
    let Response::Ticket(t2) = srv.handle(
        alice,
        Request::MintTicket {
            handle: sub,
            ttl_nanos: 5,
        },
        30,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(bob, Request::RedeemTicket { ticket: t2 }, 99),
        Response::Err(ErrorCode::BadTicket)
    );

    // rev2§2.4: the caller requests the TTL but the server clamps it to
    // MAX_TICKET_TTL_NANOS, so an unbounded request does not yield an unbounded
    // ticket. A failed (expired) redeem still consumes the one-shot ticket
    // (RedeemTicket removes before checking expiry), so each boundary point
    // below needs its own freshly-minted ticket.
    let mint = 100;
    let Response::Ticket(t3) = srv.handle(
        alice,
        Request::MintTicket {
            handle: sub,
            ttl_nanos: u64::MAX,
        },
        mint,
    ) else {
        panic!()
    };
    // Just past the clamp → refused, despite the u64::MAX request.
    assert_eq!(
        srv.handle(
            bob,
            Request::RedeemTicket { ticket: t3 },
            mint + MAX_TICKET_TTL_NANOS + 1
        ),
        Response::Err(ErrorCode::BadTicket)
    );

    let Response::Ticket(t4) = srv.handle(
        alice,
        Request::MintTicket {
            handle: sub,
            ttl_nanos: u64::MAX,
        },
        mint,
    ) else {
        panic!()
    };
    // Exactly at the clamp boundary → still valid (redeem fails only when now > expires).
    assert!(matches!(
        srv.handle(
            bob,
            Request::RedeemTicket { ticket: t4 },
            mint + MAX_TICKET_TTL_NANOS
        ),
        Response::Handle(_)
    ));
}

#[test]
fn history_rewriting_needs_the_right_and_triggers_gc() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"old".to_vec(),
            class: 1,
        },
        100,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                data: vec![9; 64]
            },
            101
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::Sync { handle: 0 }, 102),
        Response::Ok
    );

    // A handle without may-rewrite-history can't delete snapshots; one
    // scoped to a subtree can't either (no ref surgery from a chroot).
    let limited = HandleEntry {
        rights: R_READ | R_WRITE,
        ..srv.root_grant(b"main").unwrap()
    };
    let s2 = srv.open_session(vec![limited]);
    assert_eq!(
        srv.handle(
            s2,
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: snap
            },
            103
        ),
        Response::Err(ErrorCode::Denied)
    );
    let Response::Handle(sub) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: 0xFF,
        },
        104,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::DeleteSnapshot {
                handle: sub,
                snap_id: snap
            },
            105
        ),
        Response::Err(ErrorCode::Denied)
    );

    // Deletion is a small ref-table edit that arms the GC trigger
    // (rev2§4.6); the reclamation itself happens in the drained cycle.
    assert!(!srv.gc_requested());
    assert_eq!(
        srv.handle(
            s,
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: snap
            },
            106
        ),
        Response::Ok
    );
    assert!(srv.gc_requested());
    let stats = srv.run_gc().unwrap();
    assert!(stats.freed_objects > 0);
    assert!(!srv.gc_requested());

    // The deleted snapshot is gone; current state is untouched.
    assert_eq!(
        srv.handle(
            s,
            Request::Rollback {
                handle: 0,
                snap_id: snap
            },
            107
        ),
        Response::Err(ErrorCode::NoSuchSnapshot)
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                len: 4
            },
            108
        ),
        Response::Data(vec![9; 4])
    );
}

#[test]
fn manual_gc_and_statfs() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    let Response::Space { total, used, free } = srv.handle(s, Request::Statfs { handle: 0 }, 10)
    else {
        panic!()
    };
    assert_eq!(used + free, total);

    // Manual gc needs may-rewrite-history too.
    let ro = HandleEntry {
        rights: R_READ,
        ..srv.root_grant(b"main").unwrap()
    };
    let s2 = srv.open_session(vec![ro]);
    assert_eq!(
        srv.handle(s2, Request::Gc { handle: 0 }, 11),
        Response::Err(ErrorCode::Denied)
    );

    let Response::GcReport { live_objects, .. } = srv.handle(s, Request::Gc { handle: 0 }, 12)
    else {
        panic!()
    };
    assert!(live_objects > 0);
}

#[test]
fn statfs_gated_by_stat_store() {
    // statfs observes store-global space, so it needs `stat-store` (rev2§2.3),
    // deny-by-default. Only the privileged root_grant originates the bit.
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let zero = HandleEntry {
        rights: 0,
        ..srv.root_grant(b"main").unwrap()
    };
    let ro = HandleEntry {
        rights: R_READ,
        ..srv.root_grant(b"main").unwrap()
    };
    // handle 0: privileged (carries stat-store). handle 1: zero rights.
    // handle 2: R_READ only. Neither 1 nor 2 has the bit.
    let s = srv.open_session(vec![root, zero, ro]);

    // Deny-by-default: a handle lacking the bit cannot observe the store.
    assert_eq!(
        srv.handle(s, Request::Statfs { handle: 1 }, 10),
        Response::Err(ErrorCode::Denied)
    );
    assert_eq!(
        srv.handle(s, Request::Statfs { handle: 2 }, 10),
        Response::Err(ErrorCode::Denied)
    );

    // The privileged handle observes the whole store.
    let Response::Space { total, used, free } = srv.handle(s, Request::Statfs { handle: 0 }, 10)
    else {
        panic!("privileged statfs was refused")
    };
    assert_eq!(used + free, total);

    // Ordinary delegation strips the bit: a subtree handle masked by R_ALL
    // (which omits bit 5) is refused — the intersection clears it for free.
    let Response::Handle(plain) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_ALL,
        },
        11,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(s, Request::Statfs { handle: plain }, 12),
        Response::Err(ErrorCode::Denied)
    );

    // Explicitly carrying the bit onto a deep subtree handle keeps it; its
    // statfs observes the *whole* store — the right's scope ignores the
    // subtree its handle denotes (rev2§2.3).
    let Response::Handle(carried) = srv.handle(
        s,
        Request::OpenChild {
            handle: 0,
            path: p(&["pub"]),
            rights_mask: R_ALL | R_STAT_STORE,
        },
        13,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(s, Request::Statfs { handle: carried }, 14),
        Response::Space { total, used, free }
    );

    // A generation bump kills stat-store like any other right: lookup's
    // staleness check precedes the rights check, so statfs returns Stale.
    assert_eq!(
        srv.handle(s, Request::RevokeRef { handle: 0 }, 15),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::Statfs { handle: 0 }, 16),
        Response::Err(ErrorCode::Stale)
    );
}

#[test]
fn watermark_arms_gc_and_reclaim_recovers_space() {
    // Small store: ~112 KiB chunk region, so a few generations of churn
    // cross the 20%-free watermark.
    let opts = StoreOptions {
        wal_len: 8 * 1024,
        ..StoreOptions::default()
    };
    let mut store = Store::format(MemDev::new(128 * 1024), opts).unwrap();
    store.create_ref(b"main").unwrap();
    let mut srv = Server::new(store, 1);
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    // Drive churn the way the transport does: drain the trigger after
    // each reply. The watermark must fire before space runs out, and the
    // store must keep absorbing the same churn forever afterwards.
    let mut armed = 0u32;
    for i in 0..40u32 {
        let data: Vec<u8> = (0..10_000)
            .map(|j| (j as u8).wrapping_mul(i as u8 + 1))
            .collect();
        let w = srv.handle(
            s,
            Request::Write {
                handle: 0,
                path: p(&["churn"]),
                offset: 0,
                data,
            },
            i as u64,
        );
        assert_eq!(w, Response::Ok, "iteration {i}");
        if srv.gc_requested() {
            armed += 1;
            let stats = srv.run_gc().unwrap();
            assert!(stats.freed_bytes > 0, "iteration {i} reclaimed nothing");
        }
        assert_eq!(
            srv.handle(s, Request::Sync { handle: 0 }, i as u64),
            Response::Ok
        );
        if srv.gc_requested() {
            armed += 1;
            srv.run_gc().unwrap();
        }
    }
    assert!(
        armed >= 3,
        "watermark armed only {armed} times over 40 generations of churn"
    );
    let Response::Space { total, free, .. } = srv.handle(s, Request::Statfs { handle: 0 }, 99)
    else {
        panic!()
    };
    assert!(free * 5 >= total, "GC did not get back above the watermark");

    // Tag pins surface as Pinned.
    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: vec![],
            class: 0,
        },
        100,
    ) else {
        panic!()
    };
    srv.store().tag(b"pin", b"main", snap).unwrap();
    assert_eq!(
        srv.handle(
            s,
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: snap
            },
            101
        ),
        Response::Err(ErrorCode::Pinned)
    );
}

#[test]
fn session_cleanup_and_audit() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    let Response::SessionDump(dump) = srv.handle(s, Request::EnumerateSession, 10) else {
        panic!()
    };
    assert_eq!(dump.len(), 1);

    // A session without the enumerate right can't audit itself.
    let limited = HandleEntry {
        rights: R_READ,
        ..srv.root_grant(b"main").unwrap()
    };
    let s2 = srv.open_session(vec![limited]);
    assert_eq!(
        srv.handle(s2, Request::EnumerateSession, 11),
        Response::Err(ErrorCode::Denied)
    );

    // Peer-closed → whole table dropped (rev2§2.4 cleanup).
    srv.close_session(s);
    assert_eq!(
        srv.handle(
            s,
            Request::Read {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                len: u32::MAX
            },
            12
        ),
        Response::Err(ErrorCode::BadHandle)
    );
}

/// rev2§4.7: the guarded batch closes the retention read-then-act
/// race over a session. X enumerates (getting the edit version); Y snapshots,
/// advancing it; X's `Apply` at the stale version is refused carrying the
/// current version (no edit applied); X re-reads and re-applies at the current
/// version, which then lands. This demonstrates the remedy closing the
/// read-then-act window.
#[test]
fn apply_batch_closes_read_then_act_race() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let x = srv.open_session(vec![root.clone()]);
    let y = srv.open_session(vec![root]);

    // X creates a base snapshot (class KEEP=0) and enumerates to get v.
    let Response::SnapId(snap) = srv.handle(
        x,
        Request::Snapshot {
            handle: 0,
            message: b"base".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };
    let Response::Snapshots {
        edit_version: v, ..
    } = srv.handle(x, Request::ListSnapshots { handle: 0 }, 11)
    else {
        panic!()
    };

    // Y snapshots concurrently — the ref's edit version advances under X.
    assert!(matches!(
        srv.handle(
            y,
            Request::Snapshot {
                handle: 0,
                message: b"concurrent".to_vec(),
                class: 0,
            },
            12,
        ),
        Response::SnapId(_)
    ));

    // X acts on the now-stale version: refused, carrying the current version,
    // and nothing is applied (the snapshot's class stays KEEP).
    let edits = vec![RefEdit::SetClass { id: snap, class: 2 }]; // -> EPHEMERAL
    assert_eq!(
        srv.handle(
            x,
            Request::Apply {
                handle: 0,
                expected_version: v,
                edits: edits.clone(),
            },
            13,
        ),
        Response::VersionMismatch {
            edit_version: v + 1
        }
    );
    let Response::Snapshots {
        snaps,
        edit_version: v2,
    } = srv.handle(x, Request::ListSnapshots { handle: 0 }, 14)
    else {
        panic!()
    };
    assert_eq!(v2, v + 1, "Y's snapshot advanced the version");
    assert_eq!(
        snaps.iter().find(|r| r.id == snap).unwrap().class,
        0,
        "the stale batch applied nothing"
    );

    // X re-reads and retries at the current version → applied.
    assert_eq!(
        srv.handle(
            x,
            Request::Apply {
                handle: 0,
                expected_version: v2,
                edits,
            },
            15,
        ),
        Response::Applied {
            edit_version: v2 + 1
        }
    );
    let Response::Snapshots { snaps, .. } = srv.handle(x, Request::ListSnapshots { handle: 0 }, 16)
    else {
        panic!()
    };
    assert_eq!(
        snaps.iter().find(|r| r.id == snap).unwrap().class,
        2,
        "the retried batch applied the edit"
    );
}

/// rev2§4.7: a guarded batch is history rewriting, gated by
/// `may-rewrite-history` (the `DeleteSnapshot`/`Gc` right), deny-by-default.
#[test]
fn apply_batch_requires_rewrite_history_right() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);
    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"m".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };
    let Response::Snapshots {
        edit_version: v, ..
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 11)
    else {
        panic!()
    };
    let edits = vec![RefEdit::SetClass { id: snap, class: 2 }];

    // A handle without may-rewrite-history is denied (before any version
    // check), even with a correct expected_version.
    let limited = HandleEntry {
        rights: R_READ | R_WRITE | R_SNAPSHOT,
        ..srv.root_grant(b"main").unwrap()
    };
    let s2 = srv.open_session(vec![limited]);
    assert_eq!(
        srv.handle(
            s2,
            Request::Apply {
                handle: 0,
                expected_version: v,
                edits: edits.clone(),
            },
            12,
        ),
        Response::Err(ErrorCode::Denied)
    );

    // With the right, it applies.
    assert_eq!(
        srv.handle(
            s,
            Request::Apply {
                handle: 0,
                expected_version: v,
                edits,
            },
            13,
        ),
        Response::Applied {
            edit_version: v + 1
        }
    );
}

/// rev2§4.7: a guarded batch is all-or-nothing over the wire. A batch that
/// creates a tag and then deletes the snapshot it just pinned is rejected with
/// `Pinned` (the staged tag pins the staged snapshot) — and *nothing* persists:
/// no class edit, no version bump, and not even the tag (so a later
/// `DeleteSnapshot` of that snapshot succeeds).
#[test]
fn apply_batch_all_or_nothing_over_wire() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);
    let Response::SnapId(s1) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"a".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write {
                handle: 0,
                path: p(&["top.txt"]),
                offset: 0,
                data: b"x".to_vec(),
            },
            11,
        ),
        Response::Ok
    );
    let Response::SnapId(s2) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"b".to_vec(),
            class: 0,
        },
        12,
    ) else {
        panic!()
    };
    let Response::Snapshots {
        edit_version: v, ..
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 13)
    else {
        panic!()
    };

    let edits = vec![
        RefEdit::CreateTag {
            name: b"rel".to_vec(),
            snap_id: s1,
        },
        RefEdit::SetClass { id: s2, class: 2 },
        RefEdit::DeleteSnapshot { id: s1 }, // pinned by the staged tag
    ];
    assert_eq!(
        srv.handle(
            s,
            Request::Apply {
                handle: 0,
                expected_version: v,
                edits,
            },
            14,
        ),
        Response::Err(ErrorCode::Pinned)
    );

    // Nothing persisted: version unchanged, s2's class unchanged.
    let Response::Snapshots {
        snaps,
        edit_version: v2,
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 15)
    else {
        panic!()
    };
    assert_eq!(v2, v, "rejected batch did not commit");
    assert_eq!(
        snaps.iter().find(|r| r.id == s2).unwrap().class,
        0,
        "rejected batch applied no edit"
    );
    // The tag never persisted either, so s1 is not pinned and deletes cleanly.
    assert_eq!(
        srv.handle(
            s,
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: s1,
            },
            16,
        ),
        Response::Ok
    );
}

/// rev2§4.7 "Tags" over the wire: create, list, and delete a tag across
/// a session. `ListTags` is scoped to the handle's ref and reports each tag as
/// `(name, ref_name, snap_id)`.
#[test]
fn tags_round_trip_over_a_session() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);
    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"m".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };

    // No tags yet.
    assert_eq!(
        srv.handle(s, Request::ListTags { handle: 0 }, 11),
        Response::Tags(vec![])
    );

    // Create one: it lists, scoped to the ref, with the id it pins.
    assert_eq!(
        srv.handle(
            s,
            Request::Tag {
                handle: 0,
                name: b"release".to_vec(),
                snap_id: snap,
            },
            12,
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::ListTags { handle: 0 }, 13),
        Response::Tags(vec![(b"release".to_vec(), b"main".to_vec(), snap)])
    );

    // Delete it: gone.
    assert_eq!(
        srv.handle(
            s,
            Request::Untag {
                handle: 0,
                name: b"release".to_vec(),
            },
            14,
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::ListTags { handle: 0 }, 15),
        Response::Tags(vec![])
    );
}

/// rev2§4.7: a tag is a `keep`-strength pin over the wire — a tagged snapshot
/// can't be deleted out from under its tag, and because the tag names the
/// snapshot *id* (not a hash) a metadata edit leaves it in place. The remedy
/// is to delete the tag first.
#[test]
fn tag_pins_snapshot_over_the_wire_and_survives_metadata_edits() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);
    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"m".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };

    assert_eq!(
        srv.handle(
            s,
            Request::Tag {
                handle: 0,
                name: b"keep".to_vec(),
                snap_id: snap,
            },
            11,
        ),
        Response::Ok
    );
    // A metadata edit must not unpin it (the tag points at the id, rev2§4.7).
    assert_eq!(
        srv.handle(
            s,
            Request::SetClass {
                handle: 0,
                snap_id: snap,
                class: 2,
            },
            12,
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::ListTags { handle: 0 }, 13),
        Response::Tags(vec![(b"keep".to_vec(), b"main".to_vec(), snap)])
    );

    // Pinned: the snapshot can't be deleted while the tag holds it.
    assert_eq!(
        srv.handle(
            s,
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: snap,
            },
            14,
        ),
        Response::Err(ErrorCode::Pinned)
    );

    // Delete the tag first, then the snapshot deletes cleanly.
    assert_eq!(
        srv.handle(
            s,
            Request::Untag {
                handle: 0,
                name: b"keep".to_vec(),
            },
            15,
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::DeleteSnapshot {
                handle: 0,
                snap_id: snap,
            },
            16,
        ),
        Response::Ok
    );
}

/// rev2§4.7: tag/untag are row surgery — they need `may-rewrite-history` (and
/// validate the snapshot), while `ListTags` is a read needing `R_READ`.
#[test]
fn tag_ops_need_rewrite_history_and_validate_the_snapshot() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);
    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"m".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };

    // No may-rewrite-history → Tag/Untag denied (deny-by-default, before any
    // store work). Without even R_READ, ListTags is denied too.
    let limited = HandleEntry {
        rights: R_WRITE | R_SNAPSHOT,
        ..srv.root_grant(b"main").unwrap()
    };
    let s2 = srv.open_session(vec![limited]);
    assert_eq!(
        srv.handle(
            s2,
            Request::Tag {
                handle: 0,
                name: b"r".to_vec(),
                snap_id: snap,
            },
            11,
        ),
        Response::Err(ErrorCode::Denied)
    );
    assert_eq!(
        srv.handle(
            s2,
            Request::Untag {
                handle: 0,
                name: b"r".to_vec(),
            },
            12,
        ),
        Response::Err(ErrorCode::Denied)
    );
    assert_eq!(
        srv.handle(s2, Request::ListTags { handle: 0 }, 13),
        Response::Err(ErrorCode::Denied)
    );

    // With the right: a real snapshot tags; a nonexistent one is NoSuchSnapshot.
    assert_eq!(
        srv.handle(
            s,
            Request::Tag {
                handle: 0,
                name: b"r".to_vec(),
                snap_id: snap,
            },
            14,
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(
            s,
            Request::Tag {
                handle: 0,
                name: b"bad".to_vec(),
                snap_id: snap + 999,
            },
            15,
        ),
        Response::Err(ErrorCode::NoSuchSnapshot)
    );
}

/// rev2§4.7: tags are entry-set mutations, so creating or removing one advances
/// the ref's edit version — exactly what a concurrent guarded batch checks.
#[test]
fn tag_and_untag_advance_the_ref_edit_version() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);
    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot {
            handle: 0,
            message: b"m".to_vec(),
            class: 0,
        },
        10,
    ) else {
        panic!()
    };
    let Response::Snapshots {
        edit_version: v0, ..
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 11)
    else {
        panic!()
    };

    assert_eq!(
        srv.handle(
            s,
            Request::Tag {
                handle: 0,
                name: b"r".to_vec(),
                snap_id: snap,
            },
            12,
        ),
        Response::Ok
    );
    let Response::Snapshots {
        edit_version: v1, ..
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 13)
    else {
        panic!()
    };
    assert_eq!(v1, v0 + 1, "tag must advance the edit version");

    assert_eq!(
        srv.handle(
            s,
            Request::Untag {
                handle: 0,
                name: b"r".to_vec(),
            },
            14,
        ),
        Response::Ok
    );
    let Response::Snapshots {
        edit_version: v2, ..
    } = srv.handle(s, Request::ListSnapshots { handle: 0 }, 15)
    else {
        panic!()
    };
    assert_eq!(v2, v1 + 1, "untag must advance the edit version");
}
