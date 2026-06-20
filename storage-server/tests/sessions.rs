//! Session/handle semantics tests (spec rev1§2.2-2.4): handle relativity,
//! subtree confinement, monotone attenuation, generation revocation,
//! one-shot tickets, audit.

use cas::dev::MemDev;
use cas::store::{Store, StoreOptions};
use storage_server::*;

fn p(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|s| s.as_bytes().to_vec()).collect()
}

fn new_server() -> Server<MemDev> {
    let opts = StoreOptions { wal_len: 64 * 1024, ..StoreOptions::default() };
    let mut store = Store::format(MemDev::new(1 << 20), opts).unwrap();
    store.create_ref(b"main").unwrap();
    store.write(b"main", &p(&["top.txt"]), 0, b"top secret", 1).unwrap();
    store.write(b"main", &p(&["pub", "readme"]), 0, b"public info", 2).unwrap();
    store.write(b"main", &p(&["pub", "deep", "leaf"]), 0, b"leaf data", 3).unwrap();
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
    // nothing in s2 — the integers carry no authority (rev1§2.4).
    assert_eq!(
        srv.handle(s1, Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 10),
        Response::Data(b"top secret".to_vec())
    );
    assert_eq!(
        srv.handle(s2, Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 10),
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
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: R_READ | R_WRITE },
        10,
    ) else {
        panic!()
    };

    // Inside the subtree: fine.
    assert_eq!(
        srv.handle(s, Request::Read { handle: sub, path: p(&["readme"]), offset: 0, len: u32::MAX }, 11),
        Response::Data(b"public info".to_vec())
    );
    // The sibling simply has no name from here (rev1§2.3): the same path that
    // works on the root handle resolves under pub/ and finds nothing.
    assert_eq!(
        srv.handle(s, Request::Read { handle: sub, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 11),
        Response::NotFound
    );
    // ".." is path syntax, never stored — rejected outright (rev1§4.9).
    assert_eq!(
        srv.handle(s, Request::Read { handle: sub, path: p(&["..", "top.txt"]), offset: 0, len: u32::MAX }, 11),
        Response::Err(ErrorCode::BadPath)
    );
    // Writes through the subtree handle land under the subtree.
    assert_eq!(
        srv.handle(
            s,
            Request::Write { handle: sub, path: p(&["new"]), offset: 0, data: b"x".to_vec() },
            12
        ),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::Read { handle: 0, path: p(&["pub", "new"]), offset: 0, len: u32::MAX }, 13),
        Response::Data(b"x".to_vec())
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
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: R_READ },
        10,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write { handle: ro, path: p(&["z"]), offset: 0, data: b"n".to_vec() },
            11
        ),
        Response::Err(ErrorCode::Denied)
    );
    // …whose own children can never get write back (mask is ∩ only).
    let Response::Handle(child) = srv.handle(
        s,
        Request::OpenChild { handle: ro, path: p(&["deep"]), rights_mask: R_ALL },
        12,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write { handle: child, path: p(&["w"]), offset: 0, data: b"n".to_vec() },
            13
        ),
        Response::Err(ErrorCode::Denied)
    );
    assert_eq!(
        srv.handle(s, Request::Read { handle: child, path: p(&["leaf"]), offset: 0, len: u32::MAX }, 14),
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
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: R_ALL },
        10,
    ) else {
        panic!()
    };

    // Mass revocation from s1: O(1), no enumeration of holders (rev1§2.2).
    assert_eq!(srv.handle(s1, Request::RevokeRef { handle: 0 }, 11), Response::Ok);

    // Every outstanding handle on the ref is stale on next use —
    // including the revoker's own and s2's derived subtree handle.
    for (sess, h) in [(s1, 0), (s2, 0), (s2, sub)] {
        assert_eq!(
            srv.handle(sess, Request::Read { handle: h, path: p(&["readme"]), offset: 0, len: u32::MAX }, 12),
            Response::Err(ErrorCode::Stale),
            "session {sess} handle {h}"
        );
    }

    // Re-grant at the new generation works.
    let fresh = srv.root_grant(b"main").unwrap();
    let s3 = srv.open_session(vec![fresh]);
    assert_eq!(
        srv.handle(s3, Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 13),
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
        Request::Snapshot { handle: 0, message: b"before".to_vec(), class: 0 },
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
        Request::OpenSnapshot { handle: 0, snap_id: snap, path: vec![], rights_mask: R_ALL },
        102,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(s, Request::Read { handle: sh, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 103),
        Response::Data(b"top secret".to_vec())
    );
    // OpenSnapshot strips write rights unconditionally, so the rights
    // check (Denied) fires before the immutability backstop (ReadOnly).
    assert_eq!(
        srv.handle(
            s,
            Request::Write { handle: sh, path: p(&["top.txt"]), offset: 0, data: vec![1] },
            104
        ),
        Response::Err(ErrorCode::Denied)
    );

    // Rollback needs may-rewrite-history and restores old content.
    assert_eq!(
        srv.handle(s, Request::Rollback { handle: 0, snap_id: snap }, 105),
        Response::Ok
    );
    assert_eq!(
        srv.handle(s, Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 106),
        Response::Data(b"top secret".to_vec())
    );

    // Provenance was server-assigned.
    let Response::Snapshots(rows) = srv.handle(s, Request::ListSnapshots { handle: 0 }, 107)
    else {
        panic!()
    };
    assert!(rows[0].provenance.starts_with(b"session="));
}

#[test]
fn tickets_are_one_shot_with_ttl() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let alice = srv.open_session(vec![root]);
    let bob = srv.open_session(vec![]);

    let Response::Handle(sub) = srv.handle(
        alice,
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: R_READ },
        10,
    ) else {
        panic!()
    };
    let Response::Ticket(t1) = srv.handle(
        alice,
        Request::MintTicket { handle: sub, ttl_nanos: 1_000 },
        20,
    ) else {
        panic!()
    };

    // Bob redeems on his own session; attenuation traveled with it.
    let Response::Handle(bh) = srv.handle(bob, Request::RedeemTicket { ticket: t1 }, 25)
    else {
        panic!()
    };
    assert_eq!(
        srv.handle(bob, Request::Read { handle: bh, path: p(&["readme"]), offset: 0, len: u32::MAX }, 26),
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
        Request::MintTicket { handle: sub, ttl_nanos: 5 },
        30,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(bob, Request::RedeemTicket { ticket: t2 }, 99),
        Response::Err(ErrorCode::BadTicket)
    );
}

#[test]
fn history_rewriting_needs_the_right_and_triggers_gc() {
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let s = srv.open_session(vec![root]);

    let Response::SnapId(snap) = srv.handle(
        s,
        Request::Snapshot { handle: 0, message: b"old".to_vec(), class: 1 },
        100,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(
            s,
            Request::Write { handle: 0, path: p(&["top.txt"]), offset: 0, data: vec![9; 64] },
            101
        ),
        Response::Ok
    );
    assert_eq!(srv.handle(s, Request::Sync { handle: 0 }, 102), Response::Ok);

    // A handle without may-rewrite-history can't delete snapshots; one
    // scoped to a subtree can't either (no ref surgery from a chroot).
    let limited = HandleEntry { rights: R_READ | R_WRITE, ..srv.root_grant(b"main").unwrap() };
    let s2 = srv.open_session(vec![limited]);
    assert_eq!(
        srv.handle(s2, Request::DeleteSnapshot { handle: 0, snap_id: snap }, 103),
        Response::Err(ErrorCode::Denied)
    );
    let Response::Handle(sub) = srv.handle(
        s,
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: 0xFF },
        104,
    ) else {
        panic!()
    };
    assert_eq!(
        srv.handle(s, Request::DeleteSnapshot { handle: sub, snap_id: snap }, 105),
        Response::Err(ErrorCode::Denied)
    );

    // Deletion is a small ref-table edit that arms the GC trigger
    // (rev1§4.6); the reclamation itself happens in the drained cycle.
    assert!(!srv.gc_requested());
    assert_eq!(
        srv.handle(s, Request::DeleteSnapshot { handle: 0, snap_id: snap }, 106),
        Response::Ok
    );
    assert!(srv.gc_requested());
    let stats = srv.run_gc().unwrap();
    assert!(stats.freed_objects > 0);
    assert!(!srv.gc_requested());

    // The deleted snapshot is gone; current state is untouched.
    assert_eq!(
        srv.handle(s, Request::Rollback { handle: 0, snap_id: snap }, 107),
        Response::Err(ErrorCode::NoSuchSnapshot)
    );
    assert_eq!(
        srv.handle(s, Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: 4 }, 108),
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
    let ro = HandleEntry { rights: R_READ, ..srv.root_grant(b"main").unwrap() };
    let s2 = srv.open_session(vec![ro]);
    assert_eq!(srv.handle(s2, Request::Gc { handle: 0 }, 11), Response::Err(ErrorCode::Denied));

    let Response::GcReport { live_objects, .. } = srv.handle(s, Request::Gc { handle: 0 }, 12)
    else {
        panic!()
    };
    assert!(live_objects > 0);
}

#[test]
fn statfs_gated_by_stat_store() {
    // statfs observes store-global space, so it needs `stat-store` (rev1§2.3),
    // deny-by-default. Only the privileged root_grant originates the bit.
    let mut srv = new_server();
    let root = srv.root_grant(b"main").unwrap();
    let zero = HandleEntry { rights: 0, ..srv.root_grant(b"main").unwrap() };
    let ro = HandleEntry { rights: R_READ, ..srv.root_grant(b"main").unwrap() };
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
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: R_ALL },
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
    // subtree its handle denotes (rev1§2.3).
    let Response::Handle(carried) = srv.handle(
        s,
        Request::OpenChild { handle: 0, path: p(&["pub"]), rights_mask: R_ALL | R_STAT_STORE },
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
    assert_eq!(srv.handle(s, Request::RevokeRef { handle: 0 }, 15), Response::Ok);
    assert_eq!(
        srv.handle(s, Request::Statfs { handle: 0 }, 16),
        Response::Err(ErrorCode::Stale)
    );
}

#[test]
fn watermark_arms_gc_and_reclaim_recovers_space() {
    // Small store: ~112 KiB chunk region, so a few generations of churn
    // cross the 20%-free watermark.
    let opts = StoreOptions { wal_len: 8 * 1024, ..StoreOptions::default() };
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
        let data: Vec<u8> = (0..10_000).map(|j| (j as u8).wrapping_mul(i as u8 + 1)).collect();
        let w = srv.handle(
            s,
            Request::Write { handle: 0, path: p(&["churn"]), offset: 0, data },
            i as u64,
        );
        assert_eq!(w, Response::Ok, "iteration {i}");
        if srv.gc_requested() {
            armed += 1;
            let stats = srv.run_gc().unwrap();
            assert!(stats.freed_bytes > 0, "iteration {i} reclaimed nothing");
        }
        assert_eq!(srv.handle(s, Request::Sync { handle: 0 }, i as u64), Response::Ok);
        if srv.gc_requested() {
            armed += 1;
            srv.run_gc().unwrap();
        }
    }
    assert!(armed >= 3, "watermark armed only {armed} times over 40 generations of churn");
    let Response::Space { total, free, .. } = srv.handle(s, Request::Statfs { handle: 0 }, 99)
    else {
        panic!()
    };
    assert!(free * 5 >= total, "GC did not get back above the watermark");

    // Tag pins surface as Pinned.
    let Response::SnapId(snap) =
        srv.handle(s, Request::Snapshot { handle: 0, message: vec![], class: 0 }, 100)
    else {
        panic!()
    };
    srv.store().tag(b"pin", b"main", snap).unwrap();
    assert_eq!(
        srv.handle(s, Request::DeleteSnapshot { handle: 0, snap_id: snap }, 101),
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

    // Peer-closed → whole table dropped (rev1§2.4 cleanup).
    srv.close_session(s);
    assert_eq!(
        srv.handle(s, Request::Read { handle: 0, path: p(&["top.txt"]), offset: 0, len: u32::MAX }, 12),
        Response::Err(ErrorCode::BadHandle)
    );
}
