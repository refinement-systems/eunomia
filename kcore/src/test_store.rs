//! Concrete array-backed `Store` + the executable contract checks for the
//! `external_body` cspace ops (`delete`, `cdt_unlink`, `slot_move`).
//!
//! Those three ops carry **assumed** Verus contracts (their bodies are in-place
//! linked-list-splice walks whose deductive proof is the scoped residue —
//! doc/results/22 §3). This module is the *executable counterpart* of that
//! deferred proof: a plain-array `Store` over which the **real** op bodies run,
//! with hand-built and randomly-generated CDT shapes, asserting every clause of
//! each op's `ensures` — including the **strengthened** `cspace_wf`
//! (`siblings_share_parent`/`parent_has_first_child`/`sib_acyclic`). If a body
//! ever violated its contract the assertion would fire; the contract is thus
//! continuously checked against the body in CI (`cargo test -p kcore`), the
//! discipline the §9 review recommended.
//!
//! The checkers (`*_exec`) mirror the `verus!{}` `spec fn`s — which are ghost
//! (erased in a normal build) and so uncallable from exec test code, hence the
//! plain-Rust re-expression. Shapes are built with the *verified* `derive`, so
//! the generator cannot manufacture a non-`cspace_wf` start state.

use crate::cspace::{cdt_unlink, delete, derive, revoke, slot_move, Cap, CapKind, CapSlot, Rights};
use crate::id::{ObjId, SlotId};
use crate::untyped::{reset, retype_check, ObjType, RetypeError};
use crate::store::{Binding, Store};
use crate::thread::{Report, ThreadState};
use std::collections::BTreeMap;

// ── The concrete store ────────────────────────────────────────────────────
//
// Slots are a `Vec<CapSlot>` (a `SlotId` is its index); object refcounts and
// cspace resident lists are keyed maps. Only the handful of accessors the CDT /
// teardown path touches for Frame/Untyped/CSpace/Aspace caps are real; the
// channel/notification/thread/timer seam is `unimplemented!()` — these tests use
// none of those cap kinds, so the teardown never reaches them (a stray call
// would panic loudly rather than silently model nothing).

struct ArrayStore {
    slots: Vec<CapSlot>,
    refs: BTreeMap<u64, u32>,
    cspaces: BTreeMap<u64, Vec<SlotId>>,
}

impl ArrayStore {
    fn new(n: usize) -> Self {
        ArrayStore { slots: vec![CapSlot::empty(); n], refs: BTreeMap::new(), cspaces: BTreeMap::new() }
    }
    fn n(&self) -> usize {
        self.slots.len()
    }
    fn at(&self, s: SlotId) -> CapSlot {
        self.slots[s.0 as usize]
    }
}

impl Store for ArrayStore {
    fn slot(&self, s: SlotId) -> CapSlot {
        self.slots[s.0 as usize]
    }
    fn set_slot(&mut self, s: SlotId, v: CapSlot) {
        self.slots[s.0 as usize] = v;
    }
    fn obj_refs(&self, o: ObjId) -> u32 {
        *self.refs.get(&o.0).expect("obj_refs: object not registered in this test store")
    }
    fn set_obj_refs(&mut self, o: ObjId, r: u32) {
        self.refs.insert(o.0, r);
    }
    fn cspace_num_slots(&self, cs: ObjId) -> u32 {
        self.cspaces.get(&cs.0).map(|v| v.len() as u32).unwrap_or(0)
    }
    fn cspace_slot(&self, cs: ObjId, i: u32) -> SlotId {
        self.cspaces[&cs.0][i as usize]
    }
    fn aspace_destroy(&mut self, _a: ObjId) {}
    fn aspace_unmap(&mut self, _a: ObjId, _va: u64, _pages: u64) {}

    // ── unexercised by the CDT/teardown tests (no channel/notif/thread/timer
    //    caps are built), so left unimplemented; reaching one is a test bug. ──
    fn chan_depth(&self, _: ObjId) -> u32 {
        unimplemented!()
    }
    fn chan_end_caps(&self, _: ObjId, _: usize) -> u32 {
        unimplemented!()
    }
    fn set_chan_end_caps(&mut self, _: ObjId, _: usize, _: u32) {
        unimplemented!()
    }
    fn chan_head(&self, _: ObjId, _: usize) -> u32 {
        unimplemented!()
    }
    fn set_chan_head(&mut self, _: ObjId, _: usize, _: u32) {
        unimplemented!()
    }
    fn chan_count(&self, _: ObjId, _: usize) -> u32 {
        unimplemented!()
    }
    fn set_chan_count(&mut self, _: ObjId, _: usize, _: u32) {
        unimplemented!()
    }
    fn chan_binding(&self, _: ObjId, _: usize, _: usize) -> Binding {
        unimplemented!()
    }
    fn set_chan_binding(&mut self, _: ObjId, _: usize, _: usize, _: Binding) {
        unimplemented!()
    }
    fn chan_ring_cap(&self, _: ObjId, _: usize, _: u32, _: usize) -> SlotId {
        unimplemented!()
    }
    fn chan_msg_len(&self, _: ObjId, _: usize, _: u32) -> u16 {
        unimplemented!()
    }
    fn set_chan_msg_len(&mut self, _: ObjId, _: usize, _: u32, _: u16) {
        unimplemented!()
    }
    fn chan_msg_write(&mut self, _: ObjId, _: usize, _: u32, _: &[u8]) {
        unimplemented!()
    }
    fn chan_msg_read(&self, _: ObjId, _: usize, _: u32, _: usize, _: &mut [u8]) {
        unimplemented!()
    }
    fn notif_word(&self, _: ObjId) -> u64 {
        unimplemented!()
    }
    fn set_notif_word(&mut self, _: ObjId, _: u64) {
        unimplemented!()
    }
    fn notif_wait_head(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_notif_wait_head(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn notif_wait_tail(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_notif_wait_tail(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn tcb_state(&self, _: ObjId) -> ThreadState {
        unimplemented!()
    }
    fn set_tcb_state(&mut self, _: ObjId, _: ThreadState) {
        unimplemented!()
    }
    fn tcb_qnext(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_tcb_qnext(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn tcb_wait_notif(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_tcb_wait_notif(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn tcb_report(&self, _: ObjId) -> Report {
        unimplemented!()
    }
    fn set_tcb_report(&mut self, _: ObjId, _: Report) {
        unimplemented!()
    }
    fn tcb_bind_slot(&self, _: ObjId, _: usize) -> SlotId {
        unimplemented!()
    }
    fn tcb_bind_bits(&self, _: ObjId, _: usize) -> u64 {
        unimplemented!()
    }
    fn set_tcb_bind_bits(&mut self, _: ObjId, _: usize, _: u64) {
        unimplemented!()
    }
    fn tcb_cspace(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_tcb_cspace(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn tcb_aspace(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_tcb_aspace(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn set_tcb_retval(&mut self, _: ObjId, _: u64) {
        unimplemented!()
    }
    fn timer_armed(&self, _: ObjId) -> bool {
        unimplemented!()
    }
    fn set_timer_armed(&mut self, _: ObjId, _: bool) {
        unimplemented!()
    }
    fn timer_deadline(&self, _: ObjId) -> u64 {
        unimplemented!()
    }
    fn set_timer_deadline(&mut self, _: ObjId, _: u64) {
        unimplemented!()
    }
    fn timer_notif(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_timer_notif(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn timer_bits(&self, _: ObjId) -> u64 {
        unimplemented!()
    }
    fn set_timer_bits(&mut self, _: ObjId, _: u64) {
        unimplemented!()
    }
    fn timer_next(&self, _: ObjId) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_timer_next(&mut self, _: ObjId, _: Option<ObjId>) {
        unimplemented!()
    }
    fn make_runnable(&mut self, _: ObjId) {
        unimplemented!()
    }
    fn unqueue_ready(&mut self, _: ObjId) {
        unimplemented!()
    }
    fn tlb_invalidate_page(&mut self, _: u16, _: u64) {
        unimplemented!()
    }
    fn barrier_after_map(&mut self) {
        unimplemented!()
    }
    fn barrier_after_unmap(&mut self) {
        unimplemented!()
    }
    fn timer_armed_head(&self) -> Option<ObjId> {
        unimplemented!()
    }
    fn set_timer_armed_head(&mut self, _: Option<ObjId>) {
        unimplemented!()
    }
}

// ── Executable mirrors of the cspace.rs `spec fn`s (ghost, hence erased) ────

fn cdt_wf_exec(st: &ArrayStore) -> bool {
    let n = st.n();
    let in_dom = |o: Option<SlotId>| match o {
        None => true,
        Some(h) => (h.0 as usize) < n,
    };
    // links_in_domain — checked first so every later index is sound.
    for i in 0..n {
        let s = st.slots[i];
        if !(in_dom(s.parent) && in_dom(s.first_child) && in_dom(s.next_sib) && in_dom(s.prev_sib)) {
            return false;
        }
    }
    for i in 0..n {
        let a = SlotId(i as u64);
        let s = st.slots[i];
        let get = |h: SlotId| st.slots[h.0 as usize];
        // siblings_doubly_consistent
        if let Some(b) = s.next_sib {
            if get(b).prev_sib != Some(a) {
                return false;
            }
        }
        if let Some(b) = s.prev_sib {
            if get(b).next_sib != Some(a) {
                return false;
            }
        }
        // siblings_share_parent
        if let Some(b) = s.next_sib {
            if get(b).parent != s.parent {
                return false;
            }
        }
        // first_child_parent_agree
        if let Some(c) = s.first_child {
            if get(c).parent != Some(a) || get(c).prev_sib.is_some() {
                return false;
            }
        }
        // head_is_first_child
        if let Some(p) = s.parent {
            if s.prev_sib.is_none() && get(p).first_child != Some(a) {
                return false;
            }
        }
        // parent_has_first_child
        if let Some(p) = s.parent {
            if get(p).first_child.is_none() {
                return false;
            }
        }
        // empty_slots_detached
        if s.cap.is_empty()
            && (s.parent.is_some()
                || s.first_child.is_some()
                || s.next_sib.is_some()
                || s.prev_sib.is_some())
        {
            return false;
        }
    }
    true
}

// A `link`-following walk longer than `n` steps must have repeated a node — a
// cycle. (The ghost `acyclic`/`sib_acyclic` are existential rank witnesses; this
// is the equivalent executable check.)
fn no_cycle(st: &ArrayStore, link: impl Fn(CapSlot) -> Option<SlotId>) -> bool {
    let n = st.n();
    for i in 0..n {
        let mut cur = Some(SlotId(i as u64));
        let mut steps = 0;
        while let Some(h) = cur {
            steps += 1;
            if steps > n {
                return false;
            }
            cur = link(st.slots[h.0 as usize]);
        }
    }
    true
}

fn cspace_wf_exec(st: &ArrayStore) -> bool {
    cdt_wf_exec(st) && no_cycle(st, |s| s.parent) && no_cycle(st, |s| s.next_sib)
}

fn count_nonempty_exec(st: &ArrayStore) -> usize {
    (0..st.n()).filter(|&i| !st.slots[i].cap.is_empty()).count()
}

// ── Shape builders ─────────────────────────────────────────────────────────

fn detached(cap: Cap) -> CapSlot {
    CapSlot { cap, parent: None, first_child: None, next_sib: None, prev_sib: None }
}
fn frame_cap(base: u64) -> Cap {
    Cap { kind: CapKind::Frame { base, pages: 1, mapping: None }, rights: Rights(0xff) }
}
fn cspace_cap(o: u64) -> Cap {
    Cap { kind: CapKind::CSpace(ObjId(o)), rights: Rights(0xff) }
}
fn untyped_cap(base: u64, size: u64, watermark: u64) -> Cap {
    Cap { kind: CapKind::Untyped { base, size, watermark }, rights: Rights(0xff) }
}

// The exec mirror of the spec `CapKind::Untyped { base, size, watermark }`
// projection — `Some(geometry)` iff the cap is an untyped, used both to compute
// `retype_check`'s expected `Ok` triple and to read `reset`'s watermark edit.
fn untyped_geom(c: Cap) -> Option<(u64, u64, u64)> {
    match c.kind {
        CapKind::Untyped { base, size, watermark } => Some((base, size, watermark)),
        _ => None,
    }
}

// A Debug+PartialEq snapshot of everything the two ops can observably touch
// (emptiness, the four CDT links, and any untyped geometry). `SlotId`s are
// flattened to `u64` because `SlotId` is not `Debug` (so `assert_eq!` on the
// raw handle would not compile). Used to assert the read-only / single-slot
// frames against the real bodies.
type SlotFp = (bool, Option<u64>, Option<u64>, Option<u64>, Option<u64>, Option<(u64, u64, u64)>);
fn fingerprint(st: &ArrayStore) -> Vec<SlotFp> {
    (0..st.n())
        .map(|i| {
            let s = st.slots[i];
            (
                s.cap.is_empty(),
                s.parent.map(|x| x.0),
                s.first_child.map(|x| x.0),
                s.next_sib.map(|x| x.0),
                s.prev_sib.map(|x| x.0),
                untyped_geom(s.cap),
            )
        })
        .collect()
}

// A tiny deterministic LCG — Rust tests may not use a wall clock, and a fixed
// seed makes any failure reproducible (the fuzz-corpus discipline).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

// Grow a random CDT forest of bare Frame caps using the **verified** `derive`,
// so the start state is `cspace_wf` by construction (Frame caps carry no object,
// so no refcount bookkeeping is needed).
fn gen_forest(seed: u64, n: usize, edges: usize) -> ArrayStore {
    let mut st = ArrayStore::new(n);
    st.slots[0] = detached(frame_cap(0));
    let mut nonempty = vec![SlotId(0)];
    let mut empty: Vec<SlotId> = (1..n as u64).map(SlotId).collect();
    let mut rng = Lcg(seed);
    for _ in 0..edges {
        if empty.is_empty() {
            break;
        }
        let src = nonempty[rng.below(nonempty.len())];
        let dst = empty.swap_remove(rng.below(empty.len()));
        derive(&mut st, src, dst, 0xff).expect("derive Frame child");
        nonempty.push(dst);
    }
    assert!(cspace_wf_exec(&st), "generator produced a non-cspace_wf forest");
    st
}

// ── Contract checks (the op `ensures`, asserted against the real bodies) ────

fn check_delete(st: &mut ArrayStore, slot: SlotId) {
    assert!(cspace_wf_exec(st), "delete pre: cspace_wf");
    assert!(!st.at(slot).cap.is_empty(), "delete pre: slot non-empty");
    let (n0, c0) = (st.n(), count_nonempty_exec(st));
    delete(st, slot);
    assert!(cspace_wf_exec(st), "delete post: cspace_wf preserved");
    assert_eq!(st.n(), n0, "delete post: dom preserved");
    assert!(st.at(slot).cap.is_empty(), "delete post: target slot emptied");
    assert!(count_nonempty_exec(st) < c0, "delete post: count_nonempty strictly drops");
}

fn check_cdt_unlink(st: &mut ArrayStore, slot: SlotId) {
    assert!(cspace_wf_exec(st), "cdt_unlink pre: cspace_wf");
    let (n0, c0) = (st.n(), count_nonempty_exec(st));
    let cap_was_empty = st.at(slot).cap.is_empty();
    cdt_unlink(st, slot);
    let s = st.at(slot);
    assert!(cspace_wf_exec(st), "cdt_unlink post: cspace_wf preserved");
    assert_eq!(st.n(), n0, "cdt_unlink post: dom preserved");
    assert!(
        s.parent.is_none() && s.first_child.is_none() && s.next_sib.is_none() && s.prev_sib.is_none(),
        "cdt_unlink post: slot fully detached"
    );
    assert_eq!(s.cap.is_empty(), cap_was_empty, "cdt_unlink post: cap untouched");
    assert_eq!(count_nonempty_exec(st), c0, "cdt_unlink post: count_nonempty unchanged");
}

fn check_slot_move(st: &mut ArrayStore, src: SlotId, dst: SlotId) {
    assert!(cspace_wf_exec(st), "slot_move pre: cspace_wf");
    assert!(!st.at(src).cap.is_empty() && st.at(dst).cap.is_empty(), "slot_move pre: src live, dst empty");
    let (n0, c0) = (st.n(), count_nonempty_exec(st));
    let moved = st.at(src).cap;
    slot_move(st, src, dst);
    assert!(cspace_wf_exec(st), "slot_move post: cspace_wf preserved");
    assert_eq!(st.n(), n0, "slot_move post: dom preserved");
    assert!(st.at(src).cap.is_empty(), "slot_move post: src emptied");
    assert!(!st.at(dst).cap.is_empty(), "slot_move post: dst now holds the cap");
    assert!(matches!(st.at(dst).cap.kind, CapKind::Frame { base, .. } if matches!(moved.kind, CapKind::Frame { base: b, .. } if b == base)),
        "slot_move post: dst inherits src's cap");
    assert_eq!(count_nonempty_exec(st), c0, "slot_move post: count_nonempty unchanged (one owner relocates)");
}

// Re-derive `retype_check`'s spec result from the store state, then assert the
// real body returns exactly that AND left the arena untouched (the read-only
// frame, which holds on every path). Covers the geometry, the error precedence
// (NotUntyped before DestOccupied), and the channel `dst2` validity.
fn check_retype_check(st: &mut ArrayStore, ut: SlotId, ty: ObjType, dst: SlotId, dst2: Option<SlotId>) {
    let fp = fingerprint(st);
    let geom = untyped_geom(st.at(ut).cap);
    let dst_empty = st.at(dst).cap.is_empty();
    let chan_ok = if matches!(ty, ObjType::Channel) {
        match dst2 {
            Some(d2) => d2 != dst && st.at(d2).cap.is_empty(),
            None => false,
        }
    } else {
        true
    };
    let res = retype_check(st, ut, ty, dst, dst2);
    assert_eq!(fingerprint(st), fp, "retype_check post: read-only on every path");
    match (geom, dst_empty, chan_ok) {
        (None, _, _) => assert_eq!(res, Err(RetypeError::NotUntyped), "non-Untyped ut → NotUntyped (precedence)"),
        (Some(g), true, true) => assert_eq!(res, Ok(g), "Ok returns the untyped's geometry"),
        (Some(_), _, _) => assert_eq!(res, Err(RetypeError::DestOccupied), "occupied/aliased/missing dst(2) → DestOccupied"),
    }
}

// Assert `reset`'s per-arm contract against the real body: `Ok` zeroes only
// `ut`'s watermark (base/size/links/all other slots intact); both `Err` arms
// are read-only.
fn check_reset(st: &mut ArrayStore, ut: SlotId) {
    let fp = fingerprint(st);
    let geom = untyped_geom(st.at(ut).cap);
    let had_child = st.at(ut).first_child.is_some();
    let res = reset(st, ut);
    match (geom, had_child) {
        (None, _) => {
            assert_eq!(res, Err(RetypeError::NotUntyped), "non-Untyped → NotUntyped");
            assert_eq!(fingerprint(st), fp, "reset NotUntyped: read-only");
        }
        (Some(_), true) => {
            assert_eq!(res, Err(RetypeError::BadArg), "children present → BadArg");
            assert_eq!(fingerprint(st), fp, "reset BadArg: read-only");
        }
        (Some((base, size, _)), false) => {
            assert_eq!(res, Ok(()), "Untyped, no children → Ok");
            let mut expected = fp.clone();
            expected[ut.0 as usize].5 = Some((base, size, 0));
            assert_eq!(fingerprint(st), expected, "reset Ok: only ut's watermark zeroed, all else intact");
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn delete_leaf() {
    let mut st = ArrayStore::new(1);
    st.slots[0] = detached(frame_cap(7));
    check_delete(&mut st, SlotId(0));
}

#[test]
fn delete_non_leaf_reparents() {
    // A parent with three children; deleting the parent re-parents them up to
    // the grandparent (here: to roots). Built with the verified derive.
    let mut st = gen_forest(1, 6, 5);
    // find a non-leaf, non-root node to delete (covers the harder cdt_unlink
    // re-parenting case inside delete).
    let target = (0..st.n())
        .map(|i| SlotId(i as u64))
        .find(|s| !st.at(*s).cap.is_empty() && st.at(*s).first_child.is_some() && st.at(*s).parent.is_some())
        .or_else(|| (0..st.n()).map(|i| SlotId(i as u64)).find(|s| st.at(*s).first_child.is_some()))
        .expect("a non-leaf node");
    check_delete(&mut st, target);
}

#[test]
fn cdt_unlink_middle_sibling() {
    // Three siblings under one parent; unlink the middle one.
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(frame_cap(0)); // parent (root)
    // children c1=1, c2=2, c3=3 as 0's first_child chain
    derive(&mut st, SlotId(0), SlotId(3), 0xff).unwrap(); // 0.first_child = 3
    derive(&mut st, SlotId(0), SlotId(2), 0xff).unwrap(); // 0.first_child = 2, 2.next = 3
    derive(&mut st, SlotId(0), SlotId(1), 0xff).unwrap(); // 0.first_child = 1, 1.next = 2
    assert!(cspace_wf_exec(&st));
    check_cdt_unlink(&mut st, SlotId(2)); // the middle sibling
}

#[test]
fn slot_move_subtree_root() {
    // Move a node that has children — dst must inherit the children (their
    // parent fixed up to dst). Spare slot 5 is the move target.
    let mut st = gen_forest(2, 6, 4);
    let src = (0..st.n())
        .map(|i| SlotId(i as u64))
        .find(|s| !st.at(*s).cap.is_empty() && st.at(*s).first_child.is_some())
        .expect("a node with children");
    let dst = (0..st.n())
        .map(|i| SlotId(i as u64))
        .find(|s| st.at(*s).cap.is_empty())
        .expect("a free slot");
    check_slot_move(&mut st, src, dst);
}

#[test]
fn retype_check_arms() {
    // slot 0: untyped; 1,3: empty dsts; 2: an occupied (non-untyped) slot.
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(untyped_cap(0x1000, 0x4000, 0x100));
    st.slots[2] = detached(frame_cap(2));
    // Ok: non-channel, empty dst → the untyped's geometry, store untouched.
    check_retype_check(&mut st, SlotId(0), ObjType::Frame, SlotId(1), None);
    // NotUntyped: ut slot is a frame (precedence — even though dst would be ok).
    check_retype_check(&mut st, SlotId(2), ObjType::Frame, SlotId(1), None);
    // DestOccupied: dst slot is occupied.
    check_retype_check(&mut st, SlotId(0), ObjType::Frame, SlotId(2), None);
    // Channel Ok: two distinct empty dsts.
    check_retype_check(&mut st, SlotId(0), ObjType::Channel, SlotId(1), Some(SlotId(3)));
    // Channel DestOccupied: dst2 missing.
    check_retype_check(&mut st, SlotId(0), ObjType::Channel, SlotId(1), None);
    // Channel DestOccupied: dst2 aliases dst.
    check_retype_check(&mut st, SlotId(0), ObjType::Channel, SlotId(1), Some(SlotId(1)));
    // Channel DestOccupied: dst2 occupied.
    check_retype_check(&mut st, SlotId(0), ObjType::Channel, SlotId(1), Some(SlotId(2)));
}

#[test]
fn reset_arms() {
    // Ok: untyped with a nonzero watermark and no children → watermark zeroed.
    let mut st = ArrayStore::new(1);
    st.slots[0] = detached(untyped_cap(0x1000, 0x4000, 0x200));
    check_reset(&mut st, SlotId(0));

    // BadArg: untyped with a CDT child (caller has not revoked yet) → unchanged.
    let mut st = ArrayStore::new(2);
    st.slots[0] = CapSlot {
        cap: untyped_cap(0x1000, 0x4000, 0x200),
        parent: None,
        first_child: Some(SlotId(1)),
        next_sib: None,
        prev_sib: None,
    };
    st.slots[1] = CapSlot {
        cap: frame_cap(1),
        parent: Some(SlotId(0)),
        first_child: None,
        next_sib: None,
        prev_sib: None,
    };
    check_reset(&mut st, SlotId(0));

    // NotUntyped: a non-untyped slot → unchanged.
    let mut st = ArrayStore::new(1);
    st.slots[0] = detached(frame_cap(0));
    check_reset(&mut st, SlotId(0));
}

#[test]
fn delete_cspace_in_cspace_cross_object_teardown() {
    // The §9 cross-object case: deleting the last cap to a cspace tears down its
    // residents, one of which is itself a cspace — recursion across objects.
    // slot 0: root cap CSpace(10); obj 10 residents [1, 2]; slot 1: CSpace(11);
    // obj 11 residents [3]; slots 2,3: Frame. refs: each cspace has its one cap.
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(cspace_cap(10));
    st.slots[1] = detached(cspace_cap(11));
    st.slots[2] = detached(frame_cap(2));
    st.slots[3] = detached(frame_cap(3));
    st.refs.insert(10, 1);
    st.refs.insert(11, 1);
    st.cspaces.insert(10, vec![SlotId(1), SlotId(2)]);
    st.cspaces.insert(11, vec![SlotId(3)]);
    check_delete(&mut st, SlotId(0));
    // Every resident is reclaimed by the recursive teardown.
    assert_eq!(count_nonempty_exec(&st), 0, "all residents torn down");
    assert_eq!(st.refs[&10], 0);
    assert_eq!(st.refs[&11], 0);
}

#[test]
fn delete_refcount_above_one_does_not_destroy() {
    // Object 10 has two caps (slots 0 and 1). Deleting one decrements to 1 and
    // does NOT run the destructor; the object's residents stay live.
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(cspace_cap(10));
    st.slots[1] = detached(cspace_cap(10));
    st.slots[2] = detached(frame_cap(2)); // a resident of obj 10
    st.refs.insert(10, 2);
    st.cspaces.insert(10, vec![SlotId(2)]);
    check_delete(&mut st, SlotId(0));
    assert_eq!(st.refs[&10], 1, "refcount decremented, not destroyed");
    assert!(!st.at(SlotId(2)).cap.is_empty(), "resident survives (object still live)");
    // Now drop the second cap: refcount hits zero and the resident is reclaimed.
    check_delete(&mut st, SlotId(1));
    assert_eq!(st.refs[&10], 0);
    assert!(st.at(SlotId(2)).cap.is_empty(), "resident reclaimed at last ref");
}

#[test]
fn checker_has_teeth() {
    // The contract checks are only meaningful if `cspace_wf_exec` actually
    // rejects malformed CDTs — otherwise the green tests are vacuous. Each shape
    // below violates exactly one invariant (incl. the Stage-1 additions and the
    // sibling-acyclicity the looping ops decrease on).
    let mk = |f: &dyn Fn(&mut ArrayStore)| {
        let mut st = ArrayStore::new(2);
        st.slots[0] = detached(frame_cap(0));
        st.slots[1] = detached(frame_cap(1));
        f(&mut st);
        st
    };
    // parent cycle (acyclic)
    let st = mk(&|st| {
        st.slots[0].parent = Some(SlotId(1));
        st.slots[1].parent = Some(SlotId(0));
        st.slots[0].first_child = Some(SlotId(1));
        st.slots[1].first_child = Some(SlotId(0));
    });
    assert!(!cspace_wf_exec(&st), "parent cycle must be rejected");
    // broken doubly-linked sibling list (siblings_doubly_consistent)
    let st = mk(&|st| {
        st.slots[0].next_sib = Some(SlotId(1));
    });
    assert!(!cspace_wf_exec(&st), "half-linked siblings must be rejected");
    // phantom child: names a parent that has no first_child (parent_has_first_child)
    let st = mk(&|st| {
        st.slots[1].parent = Some(SlotId(0));
        st.slots[1].prev_sib = Some(SlotId(0)); // dodge head_is_first_child to isolate the clause
    });
    assert!(!cspace_wf_exec(&st), "phantom child must be rejected");
    // sibling cycle with a shared parent (sib_acyclic) — passes cdt_wf's local
    // checks but has no valid sibling rank.
    let st = {
        let mut st = ArrayStore::new(3);
        st.slots[0] = detached(frame_cap(0));
        st.slots[0].first_child = Some(SlotId(2)); // a real head, so the ring is "floating"
        st.slots[2] = CapSlot { cap: frame_cap(2), parent: Some(SlotId(0)), first_child: None, next_sib: None, prev_sib: None };
        st.slots[1] = detached(frame_cap(1));
        // ring: 1.next=1 self-loop, consistent prev, parented but not the head
        st.slots[1].parent = Some(SlotId(0));
        st.slots[1].next_sib = Some(SlotId(1));
        st.slots[1].prev_sib = Some(SlotId(1));
        st
    };
    assert!(!no_cycle(&st, |s| s.next_sib), "sibling self-loop must be a cycle");
    // sanity: a well-formed two-node parent/child passes
    let st = mk(&|st| {
        st.slots[0].first_child = Some(SlotId(1));
        st.slots[1].parent = Some(SlotId(0));
    });
    assert!(cspace_wf_exec(&st), "a valid CDT must be accepted");
}

#[test]
fn randomized_sweep() {
    // For many random forests, delete/unlink/move a random eligible slot and
    // assert the full contract each time — the executable counterpart of the
    // deferred body proof, at scale.
    let mut trials = 0usize;
    for seed in 0..200u64 {
        let n = 4 + (seed as usize % 9); // 4..=12 slots
        let edges = (seed as usize % n) + 1;

        // delete
        {
            let mut st = gen_forest(seed.wrapping_mul(3).wrapping_add(1), n, edges);
            let live: Vec<SlotId> =
                (0..st.n()).map(|i| SlotId(i as u64)).filter(|s| !st.at(*s).cap.is_empty()).collect();
            let pick = live[(seed as usize) % live.len()];
            check_delete(&mut st, pick);
            trials += 1;
        }
        // cdt_unlink
        {
            let mut st = gen_forest(seed.wrapping_mul(5).wrapping_add(2), n, edges);
            let live: Vec<SlotId> =
                (0..st.n()).map(|i| SlotId(i as u64)).filter(|s| !st.at(*s).cap.is_empty()).collect();
            let pick = live[(seed as usize * 7) % live.len()];
            check_cdt_unlink(&mut st, pick);
            trials += 1;
        }
        // slot_move
        {
            let mut st = gen_forest(seed.wrapping_mul(7).wrapping_add(3), n, edges);
            let live: Vec<SlotId> =
                (0..st.n()).map(|i| SlotId(i as u64)).filter(|s| !st.at(*s).cap.is_empty()).collect();
            let free: Vec<SlotId> =
                (0..st.n()).map(|i| SlotId(i as u64)).filter(|s| st.at(*s).cap.is_empty()).collect();
            if !free.is_empty() {
                let src = live[(seed as usize * 11) % live.len()];
                let dst = free[(seed as usize) % free.len()];
                check_slot_move(&mut st, src, dst);
                trials += 1;
            }
        }
    }
    assert!(trials > 500, "sweep should exercise hundreds of trials, ran {trials}");
}

// ── §C: evidence that doc/results/21 §9's proposed fix for revoke's "revoked cap
//    survives" is UNSOUND. §9 suggested framing `delete` to "empty only the
//    deleted slot's CDT subtree." These two tests run the real `delete`/`revoke`
//    and show that is false under cross-object teardown. (doc/results/23 §C) ──

#[test]
fn delete_empties_slots_outside_the_deleted_subtree() {
    // slot 0 = a CSpace(10) cap with NO CDT children (its subtree is just {0}).
    // cspace 10's residents are slots 1, 2 — independent CDT roots, NOT in slot
    // 0's subtree. Deleting slot 0 is the last ref to cspace 10, so destroy_cspace
    // empties slots 1 and 2 — *outside* the deleted subtree.
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(cspace_cap(10));
    st.slots[1] = detached(frame_cap(1));
    st.slots[2] = detached(frame_cap(2));
    st.refs.insert(10, 1);
    st.cspaces.insert(10, vec![SlotId(1), SlotId(2)]);
    // slot 0 has no CDT descendants — its "subtree" is itself alone.
    assert!(st.at(SlotId(0)).first_child.is_none());
    check_delete(&mut st, SlotId(0));
    // Yet slots 1 and 2 — outside slot 0's subtree — were emptied by the teardown.
    assert!(st.at(SlotId(1)).cap.is_empty(), "resident outside the subtree was emptied");
    assert!(st.at(SlotId(2)).cap.is_empty(), "resident outside the subtree was emptied");
    // => "delete empties only the deleted subtree" (doc 21 §9) is FALSE.
}

#[test]
fn revoke_can_empty_its_own_root_zombie() {
    // The seL4-zombie: the revoked root `slot 0` is itself a resident of a cspace
    // whose last surviving cap lies in slot 0's OWN subtree (its child slot 1).
    //   cspace 10 residents = [slot 0];  slot 0 (Frame) ── child ──▶ slot 1 = CSpace(10).
    // revoke(slot 0) descends to the leaf slot 1 and deletes it → last ref to
    // cspace 10 → destroy_cspace(10) → deletes its resident slot 0 → the *root* is
    // emptied. So "revoked cap survives" does NOT hold unconditionally, and §9's
    // subtree-frame cannot rescue it (the teardown crosses objects). cspace_wf is
    // still preserved — that part of revoke's contract holds.
    let mut st = ArrayStore::new(2);
    st.slots[0] = CapSlot {
        cap: frame_cap(0),
        parent: None,
        first_child: Some(SlotId(1)),
        next_sib: None,
        prev_sib: None,
    };
    st.slots[1] = CapSlot {
        cap: cspace_cap(10),
        parent: Some(SlotId(0)),
        first_child: None,
        next_sib: None,
        prev_sib: None,
    };
    st.refs.insert(10, 1); // slot 1 is the one (and last) cap to cspace 10
    st.cspaces.insert(10, vec![SlotId(0)]); // cspace 10 contains slot 0 as a resident
    assert!(cspace_wf_exec(&st), "the zombie shape is well-formed");

    revoke(&mut st, SlotId(0));

    assert!(cspace_wf_exec(&st), "revoke preserves cspace_wf (its real guarantee)");
    assert!(st.at(SlotId(0)).first_child.is_none(), "revoke: subtree empty");
    // The headline: the revoked root itself was emptied by the cross-object
    // teardown — the documented gap, here a concrete witness.
    assert!(st.at(SlotId(0)).cap.is_empty(), "revoke emptied its own root (zombie)");
}
