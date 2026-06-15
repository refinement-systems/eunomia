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

use crate::channel::{
    bind, destroy_channel, endpoint_cap_dropped, recv, send, ChanError, EV_PEER_CLOSED,
    EV_READABLE, MSG_PAYLOAD,
};
use crate::cspace::{
    cdt_unlink, delete, derive, revoke, slot_move, Cap, CapKind, CapSlot, ChanEnd, Rights,
};
use crate::id::{ObjId, SlotId};
use crate::notification::signal;
use crate::untyped::{reset, retype_check, retype_install, ObjType, RetypeError};
use crate::store::{Binding, Store};
use crate::thread::{Report, ThreadState};
use std::collections::{BTreeMap, VecDeque};

// ── The concrete store ────────────────────────────────────────────────────
//
// Slots are a `Vec<CapSlot>` (a `SlotId` is its index); object refcounts and
// cspace resident lists are keyed maps. The CDT/teardown path needs only the
// Frame/Untyped/CSpace/Aspace accessors.
//
// Phase 3b adds **real channel state** (`chans`) — the `chan_*` accessors model
// the `ChanView` ghost view: per-end cap counts, per-ring FIFO cursors, event
// bindings, per-message lengths, and the ring cap-slot *handles* (the cap
// contents stay in `slots`, the single arena). It also adds the minimal
// notification + TCB state `notification::signal` touches, so the
// `external_body` `signal` contract (`slot_view`/`chan_view` unchanged) can be
// checked against the real body (`check_signal_frame`). The thread/timer seam
// `signal` never reaches stays `unimplemented!()` — a stray call panics loudly.

#[derive(Clone, PartialEq)]
struct ChanState {
    depth: u32,
    end_caps: [u32; 2],
    head: [u32; 2],
    count: [u32; 2],
    bindings: BTreeMap<(usize, usize), Binding>,      // (end, ev)
    msg_len: BTreeMap<(usize, u32), u16>,             // (ring, index)
    ring_cap: BTreeMap<(usize, u32, usize), SlotId>,  // (ring, index, cap) -> arena handle
}

#[derive(Clone, PartialEq)]
struct NotifState {
    word: u64,
    wait_head: Option<ObjId>,
    wait_tail: Option<ObjId>,
}

#[derive(Clone, PartialEq)]
struct TcbState {
    state: ThreadState,
    qnext: Option<ObjId>,
    wait_notif: Option<ObjId>,
    report: Report,
    retval: u64,
    cspace: Option<ObjId>,
    aspace: Option<ObjId>,
    bind_bits: [u64; 2],
    bind_slots: [SlotId; 2],
}

#[derive(Clone, PartialEq)]
struct TimerState {
    armed: bool,
    deadline: u64,
    notif: Option<ObjId>,
    bits: u64,
    next: Option<ObjId>,
}

struct ArrayStore {
    slots: Vec<CapSlot>,
    refs: BTreeMap<u64, u32>,
    cspaces: BTreeMap<u64, Vec<SlotId>>,
    chans: BTreeMap<u64, ChanState>,
    notifs: BTreeMap<u64, NotifState>,
    tcbs: BTreeMap<u64, TcbState>,
    timers: BTreeMap<u64, TimerState>,
    timer_armed_head: Option<ObjId>,
}

impl ArrayStore {
    fn new(n: usize) -> Self {
        ArrayStore {
            slots: vec![CapSlot::empty(); n],
            refs: BTreeMap::new(),
            cspaces: BTreeMap::new(),
            chans: BTreeMap::new(),
            notifs: BTreeMap::new(),
            tcbs: BTreeMap::new(),
            timers: BTreeMap::new(),
            timer_armed_head: None,
        }
    }
    fn n(&self) -> usize {
        self.slots.len()
    }
    fn at(&self, s: SlotId) -> CapSlot {
        self.slots[s.0 as usize]
    }
    fn chan(&self, ch: ObjId) -> &ChanState {
        self.chans.get(&ch.0).expect("chan_*: channel not registered in this test store")
    }
    fn chan_mut(&mut self, ch: ObjId) -> &mut ChanState {
        self.chans.get_mut(&ch.0).expect("set_chan_*: channel not registered in this test store")
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

    // ── channel state (plan §3b): the `chan_*` accessors backed by `chans` ──
    fn chan_depth(&self, ch: ObjId) -> u32 {
        self.chan(ch).depth
    }
    fn chan_end_caps(&self, ch: ObjId, end: usize) -> u32 {
        self.chan(ch).end_caps[end]
    }
    fn set_chan_end_caps(&mut self, ch: ObjId, end: usize, v: u32) {
        self.chan_mut(ch).end_caps[end] = v;
    }
    fn chan_head(&self, ch: ObjId, ring: usize) -> u32 {
        self.chan(ch).head[ring]
    }
    fn set_chan_head(&mut self, ch: ObjId, ring: usize, v: u32) {
        self.chan_mut(ch).head[ring] = v;
    }
    fn chan_count(&self, ch: ObjId, ring: usize) -> u32 {
        self.chan(ch).count[ring]
    }
    fn set_chan_count(&mut self, ch: ObjId, ring: usize, v: u32) {
        self.chan_mut(ch).count[ring] = v;
    }
    fn chan_binding(&self, ch: ObjId, end: usize, ev: usize) -> Binding {
        self.chan(ch).bindings.get(&(end, ev)).copied().unwrap_or(Binding::UNBOUND)
    }
    fn set_chan_binding(&mut self, ch: ObjId, end: usize, ev: usize, b: Binding) {
        self.chan_mut(ch).bindings.insert((end, ev), b);
    }
    fn chan_ring_cap(&self, ch: ObjId, ring: usize, i: u32, c: usize) -> SlotId {
        self.chan(ch).ring_cap[&(ring, i, c)]
    }
    fn chan_msg_len(&self, ch: ObjId, ring: usize, i: u32) -> u16 {
        self.chan(ch).msg_len.get(&(ring, i)).copied().unwrap_or(0)
    }
    fn set_chan_msg_len(&mut self, ch: ObjId, ring: usize, i: u32, v: u16) {
        self.chan_mut(ch).msg_len.insert((ring, i), v);
    }
    // Payload bytes are abstracted out of the ghost view, so the write/read are
    // no-ops on the modelled state (the §3b frame: `chan_view` unchanged).
    fn chan_msg_write(&mut self, _: ObjId, _: usize, _: u32, _: &[u8]) {}
    fn chan_msg_read(&self, _: ObjId, _: usize, _: u32, _: usize, _: &mut [u8]) {}

    // ── notification + TCB state `signal` touches (plan §3b) ────────────────
    fn notif_word(&self, n: ObjId) -> u64 {
        self.notifs[&n.0].word
    }
    fn set_notif_word(&mut self, n: ObjId, v: u64) {
        self.notifs.get_mut(&n.0).unwrap().word = v;
    }
    fn notif_wait_head(&self, n: ObjId) -> Option<ObjId> {
        self.notifs[&n.0].wait_head
    }
    fn set_notif_wait_head(&mut self, n: ObjId, t: Option<ObjId>) {
        self.notifs.get_mut(&n.0).unwrap().wait_head = t;
    }
    fn notif_wait_tail(&self, n: ObjId) -> Option<ObjId> {
        self.notifs[&n.0].wait_tail
    }
    fn set_notif_wait_tail(&mut self, n: ObjId, t: Option<ObjId>) {
        self.notifs.get_mut(&n.0).unwrap().wait_tail = t;
    }
    fn tcb_state(&self, t: ObjId) -> ThreadState {
        self.tcbs[&t.0].state
    }
    fn set_tcb_state(&mut self, t: ObjId, s: ThreadState) {
        self.tcbs.get_mut(&t.0).unwrap().state = s;
    }
    fn tcb_qnext(&self, t: ObjId) -> Option<ObjId> {
        self.tcbs[&t.0].qnext
    }
    fn set_tcb_qnext(&mut self, t: ObjId, q: Option<ObjId>) {
        self.tcbs.get_mut(&t.0).unwrap().qnext = q;
    }
    fn tcb_wait_notif(&self, t: ObjId) -> Option<ObjId> {
        self.tcbs[&t.0].wait_notif
    }
    fn set_tcb_wait_notif(&mut self, t: ObjId, n: Option<ObjId>) {
        self.tcbs.get_mut(&t.0).unwrap().wait_notif = n;
    }
    fn tcb_report(&self, t: ObjId) -> Report {
        self.tcbs[&t.0].report
    }
    fn set_tcb_report(&mut self, t: ObjId, r: Report) {
        self.tcbs.get_mut(&t.0).unwrap().report = r;
    }
    fn tcb_bind_slot(&self, t: ObjId, which: usize) -> SlotId {
        self.tcbs[&t.0].bind_slots[which]
    }
    fn tcb_bind_bits(&self, t: ObjId, which: usize) -> u64 {
        self.tcbs[&t.0].bind_bits[which]
    }
    fn set_tcb_bind_bits(&mut self, t: ObjId, which: usize, b: u64) {
        self.tcbs.get_mut(&t.0).unwrap().bind_bits[which] = b;
    }
    fn tcb_cspace(&self, t: ObjId) -> Option<ObjId> {
        self.tcbs[&t.0].cspace
    }
    fn set_tcb_cspace(&mut self, t: ObjId, cs: Option<ObjId>) {
        self.tcbs.get_mut(&t.0).unwrap().cspace = cs;
    }
    fn tcb_aspace(&self, t: ObjId) -> Option<ObjId> {
        self.tcbs[&t.0].aspace
    }
    fn set_tcb_aspace(&mut self, t: ObjId, a: Option<ObjId>) {
        self.tcbs.get_mut(&t.0).unwrap().aspace = a;
    }
    fn set_tcb_retval(&mut self, t: ObjId, v: u64) {
        self.tcbs.get_mut(&t.0).unwrap().retval = v;
    }
    fn timer_armed(&self, t: ObjId) -> bool {
        self.timers[&t.0].armed
    }
    fn set_timer_armed(&mut self, t: ObjId, v: bool) {
        self.timers.get_mut(&t.0).unwrap().armed = v;
    }
    fn timer_deadline(&self, t: ObjId) -> u64 {
        self.timers[&t.0].deadline
    }
    fn set_timer_deadline(&mut self, t: ObjId, v: u64) {
        self.timers.get_mut(&t.0).unwrap().deadline = v;
    }
    fn timer_notif(&self, t: ObjId) -> Option<ObjId> {
        self.timers[&t.0].notif
    }
    fn set_timer_notif(&mut self, t: ObjId, n: Option<ObjId>) {
        self.timers.get_mut(&t.0).unwrap().notif = n;
    }
    fn timer_bits(&self, t: ObjId) -> u64 {
        self.timers[&t.0].bits
    }
    fn set_timer_bits(&mut self, t: ObjId, v: u64) {
        self.timers.get_mut(&t.0).unwrap().bits = v;
    }
    fn timer_next(&self, t: ObjId) -> Option<ObjId> {
        self.timers[&t.0].next
    }
    fn set_timer_next(&mut self, t: ObjId, n: Option<ObjId>) {
        self.timers.get_mut(&t.0).unwrap().next = n;
    }
    // `make_runnable` flips the woken thread to Runnable and touches nothing else —
    // the faithful counterpart of its §4a contract (the ready-queue linkage is
    // scheduler state below the abstract `tcb_view`; a thread is off every kcore
    // queue once Runnable, so a no-op on the rest models the frame). `unqueue_ready`
    // stays a no-op (its contract is phase 4e).
    fn make_runnable(&mut self, t: ObjId) {
        self.tcbs.get_mut(&t.0).unwrap().state = ThreadState::Runnable;
    }
    fn unqueue_ready(&mut self, _: ObjId) {}
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
        self.timer_armed_head
    }
    fn set_timer_armed_head(&mut self, h: Option<ObjId>) {
        self.timer_armed_head = h;
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

// The exec mirror of the spec `in_live_window` (cspace.rs): ring index `i` is one
// of the `count[ring]` positions starting at `head[ring]`, wrapping mod `depth`.
fn in_live_window_exec(cs: &ChanState, ring: usize, i: u32) -> bool {
    (0..cs.count[ring]).any(|j| i == (cs.head[ring] + j) % cs.depth)
}

// The exec mirror of the spec `chan_wf(cv, sv, ch)` (cspace.rs) — ghost, so erased
// and uncallable from test code, hence the plain-Rust re-expression (the
// `cspace_wf_exec` discipline). Checks every clause incl. the load-bearing ring
// coupling: each ring cap handle is a live arena slot, and a cap outside the live
// window is empty in `slots` (== `slot_view`).
fn chan_wf_exec(st: &ArrayStore, ch: ObjId) -> bool {
    let cs = match st.chans.get(&ch.0) {
        Some(c) => c,
        None => return false,
    };
    if cs.depth == 0 || cs.depth > 0x8000_0000 {
        return false;
    }
    for r in 0..2 {
        if cs.count[r] > cs.depth || cs.head[r] >= cs.depth {
            return false;
        }
    }
    for ring in 0..2usize {
        for i in 0..cs.depth {
            for c in 0..4usize {
                let sid = match cs.ring_cap.get(&(ring, i, c)) {
                    Some(s) => *s,
                    None => return false, // ring_cap domain incomplete
                };
                if (sid.0 as usize) >= st.n() {
                    return false; // handle escapes the arena (slot_view domain)
                }
                if !in_live_window_exec(cs, ring, i) && !st.slots[sid.0 as usize].cap.is_empty() {
                    return false; // windowing coupling: out-of-window slot must be empty
                }
            }
            if !cs.msg_len.contains_key(&(ring, i)) {
                return false; // msg_len domain incomplete
            }
        }
    }
    // ring-cap injectivity (the §3d clause): distinct positions, distinct handles.
    let mut seen: BTreeMap<u64, (usize, u32, usize)> = BTreeMap::new();
    for ring in 0..2usize {
        for i in 0..cs.depth {
            for c in 0..4usize {
                let sid = cs.ring_cap[&(ring, i, c)];
                if seen.insert(sid.0, (ring, i, c)).is_some() {
                    return false; // two ring positions alias one arena slot
                }
            }
        }
    }
    for e in 0..2usize {
        for v in 0..3usize {
            if !cs.bindings.contains_key(&(e, v)) {
                return false; // bindings domain incomplete
            }
        }
    }
    true
}

// The exec mirror of the spec `notif_wf(nv, tv, n)` (cspace.rs) — ghost, so erased
// and uncallable from test code, hence the plain-Rust re-expression (the
// `chan_wf_exec` discipline). Walks `wait_head` via `qnext`: empty-queue head/tail
// agreement; acyclicity/finiteness (a walk longer than the TCB count repeated a
// node — the `no_cycle` pattern); the walk ends exactly at `wait_tail` (its `qnext`
// is `None`); and every charted node is a live TCB naming `n` and `BlockedNotif`.
fn notif_wf_exec(st: &ArrayStore, n: ObjId) -> bool {
    let nv = match st.notifs.get(&n.0) {
        Some(v) => v,
        None => return false,
    };
    if nv.wait_head.is_none() != nv.wait_tail.is_none() {
        return false; // empty-queue head/tail agreement
    }
    let mut cur = nv.wait_head;
    let mut last: Option<ObjId> = None;
    let mut steps = 0usize;
    while let Some(c) = cur {
        steps += 1;
        if steps > st.tcbs.len() + 1 {
            return false; // a cycle (walk longer than the TCB count)
        }
        let tcb = match st.tcbs.get(&c.0) {
            Some(t) => t,
            None => return false, // a charted node is not a live TCB
        };
        if tcb.wait_notif != Some(n) || tcb.state != ThreadState::BlockedNotif {
            return false; // per-node: names n and is BlockedNotif
        }
        last = cur;
        cur = tcb.qnext;
    }
    // the walk ended at the chain's last node (its qnext == None); it must be wait_tail.
    last == nv.wait_tail
}

// ── Shape builders ─────────────────────────────────────────────────────────

fn detached(cap: Cap) -> CapSlot {
    CapSlot { cap, parent: None, first_child: None, next_sib: None, prev_sib: None }
}
// A blank TCB (the `Tcb::empty` analog) so fixtures set only the fields under test
// with `..tcb_state_default()`. `bind_slots` default to SlotId(0); they are unread
// unless a test wires up a real binding.
fn tcb_state_default() -> TcbState {
    TcbState {
        state: ThreadState::Inactive,
        qnext: None,
        wait_notif: None,
        report: Report::Running,
        retval: 0,
        cspace: None,
        aspace: None,
        bind_bits: [0, 0],
        bind_slots: [SlotId(0), SlotId(0)],
    }
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

// Structural CapKind equality (CapKind is Clone+Copy but not PartialEq, so the
// `dst.cap.kind == kind` postcondition is re-expressed here for the differential
// runner). Covers the kinds `retype_install` installs.
fn cap_kind_eq(a: CapKind, b: CapKind) -> bool {
    match (a, b) {
        (CapKind::Empty, CapKind::Empty) => true,
        (
            CapKind::Untyped { base: b1, size: s1, watermark: w1 },
            CapKind::Untyped { base: b2, size: s2, watermark: w2 },
        ) => b1 == b2 && s1 == s2 && w1 == w2,
        (
            CapKind::Frame { base: b1, pages: p1, mapping: m1 },
            CapKind::Frame { base: b2, pages: p2, mapping: m2 },
        ) => b1 == b2 && p1 == p2 && m1 == m2,
        (CapKind::Aspace(o1), CapKind::Aspace(o2)) => o1 == o2,
        (CapKind::CSpace(o1), CapKind::CSpace(o2)) => o1 == o2,
        (CapKind::Thread(o1), CapKind::Thread(o2)) => o1 == o2,
        (CapKind::Channel(o1, e1), CapKind::Channel(o2, e2)) => o1 == o2 && e1 == e2,
        (CapKind::Notification(o1), CapKind::Notification(o2)) => o1 == o2,
        (CapKind::Timer(o1), CapKind::Timer(o2)) => o1 == o2,
        _ => false,
    }
}

// Assert `retype_install`'s §3c contract against the real body: the watermark bump,
// the §2.5 rights-inheritance table (incl. PHYS cleared for a sub-Untyped), the new
// cap as a CDT child of `ut`, `cspace_wf` preserved, and the refcount/end_caps
// deltas (non-channel: refs/chan untouched — the object's `init` pre-counts `dst`;
// channel: refs 2, both ends accounted, `dst2` = endpoint B, other channels intact).
fn check_retype_install(
    st: &mut ArrayStore,
    ut: SlotId,
    ty: ObjType,
    kind: CapKind,
    end: u64,
    dst: SlotId,
    dst2: Option<SlotId>,
) {
    assert!(cspace_wf_exec(st), "retype_install pre: cspace_wf");
    let (base, size, _) = untyped_geom(st.at(ut).cap).expect("retype_install pre: ut is Untyped");
    assert!(base <= end, "retype_install pre: base <= end");
    let ut_rights = st.at(ut).cap.rights.0;
    let refs_before = st.refs.clone();
    let chans_before = st.chans.clone();

    retype_install(st, ut, ty, kind, end, dst, dst2);

    assert!(cspace_wf_exec(st), "retype_install post: cspace_wf preserved");
    // watermark advanced to `end - base`, base/size kept.
    assert_eq!(untyped_geom(st.at(ut).cap), Some((base, size, end - base)), "watermark advanced");
    // `dst` holds the new cap as a CDT child of `ut`.
    assert!(cap_kind_eq(st.at(dst).cap.kind, kind), "dst holds the carved kind");
    // SlotId is not Debug (see `fingerprint`), so compare with `assert!(==)`.
    assert!(st.at(dst).parent == Some(ut), "dst is a CDT child of ut");
    // §2.5 rights-inheritance table.
    let expect_rights = match ty {
        ObjType::Frame => ut_rights,
        ObjType::Thread => Rights::THREAD_ALL.0,
        ObjType::Untyped => ut_rights & (Rights::READ | Rights::WRITE),
        _ => Rights::ALL.0,
    };
    assert_eq!(st.at(dst).cap.rights.0, expect_rights, "rights-inheritance table");
    if matches!(ty, ObjType::Untyped) {
        assert_eq!(st.at(dst).cap.rights.0 & Rights::PHYS, 0, "sub-Untyped never carries PHYS");
    }
    // refcount / chan_view deltas.
    match kind {
        CapKind::Channel(ch, _) => {
            assert_eq!(st.refs[&ch.0], 2, "channel refs == 2");
            assert_eq!(st.chan(ch).end_caps, [1, 1], "both ends' caps accounted");
            let d2 = dst2.expect("channel: dst2 is Some");
            assert!(
                cap_kind_eq(st.at(d2).cap.kind, CapKind::Channel(ch, ChanEnd::B)),
                "dst2 holds endpoint B"
            );
            assert_eq!(st.at(d2).cap.rights.0, Rights::ALL.0, "dst2 rights == ALL");
            assert!(st.at(d2).parent == Some(ut), "dst2 is a CDT child of ut");
            for (k, v) in chans_before.iter() {
                if *k != ch.0 {
                    assert!(st.chans.get(k) == Some(v), "other channels untouched");
                }
            }
        }
        _ => {
            assert_eq!(st.refs, refs_before, "non-channel: refs untouched (init pre-counts dst)");
            assert!(st.chans == chans_before, "non-channel: chan_view untouched");
        }
    }
}

// A well-formed one-deep channel (ObjId 7) + a notification (ObjId 100), the
// fixture for the assumed-`signal` frame check. The arena holds the channel's 8
// ring cap slots (1..=8) plus a non-empty witness at slot 0; ring 0 has one
// in-window queued cap (slot 1 non-empty), ring 1 is empty — so `chan_wf_exec`
// holds. With `with_waiter`, the notification has one blocked waiter (TCB 200, a
// queued ref on the notification) so `signal` takes the full delivery path;
// without, it takes the accumulate-and-return path. Either way `signal` must
// leave the arena and the channel state untouched.
fn signal_fixture(with_waiter: bool) -> (ArrayStore, ObjId) {
    let mut st = ArrayStore::new(9);
    st.slots[0] = detached(frame_cap(0)); // a non-empty witness slot
    st.slots[1] = detached(frame_cap(1)); // ring 0 / idx 0 / cap 0 — in window, queued

    let mut ring_cap = BTreeMap::new();
    let mut slot = 1u64;
    for ring in 0..2usize {
        for c in 0..4usize {
            ring_cap.insert((ring, 0u32, c), SlotId(slot));
            slot += 1;
        }
    }
    let mut bindings = BTreeMap::new();
    for e in 0..2usize {
        for v in 0..3usize {
            bindings.insert((e, v), Binding::UNBOUND);
        }
    }
    let mut msg_len = BTreeMap::new();
    msg_len.insert((0usize, 0u32), 5u16);
    msg_len.insert((1usize, 0u32), 0u16);
    st.chans.insert(
        7,
        ChanState { depth: 1, end_caps: [1, 1], head: [0, 0], count: [1, 0], bindings, msg_len, ring_cap },
    );

    let n = ObjId(100);
    st.refs.insert(100, 1);
    if with_waiter {
        let t = ObjId(200);
        st.tcbs.insert(
            200,
            TcbState { state: ThreadState::BlockedNotif, wait_notif: Some(n), ..tcb_state_default() },
        );
        st.notifs.insert(100, NotifState { word: 0, wait_head: Some(t), wait_tail: Some(t) });
    } else {
        st.notifs.insert(100, NotifState { word: 0, wait_head: None, wait_tail: None });
    }
    (st, n)
}

// Run the real `notification::signal` and assert its assumed §3b frame holds:
// `slot_view` (the `fingerprint` observable) and `chan_view` (`chans`) are both
// unchanged. The executable counterpart of the `external_body` contract — the
// `delete`/`signal`-against-its-body discipline.
fn check_signal_frame(st: &mut ArrayStore, n: ObjId, bits: u64) {
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    signal(st, n, bits);
    assert!(fingerprint(st) == fp, "signal post: slot_view unchanged");
    assert!(st.chans == chans, "signal post: chan_view unchanged");
}

// A→0, B→1: the exec mirror of `end_idx`/`end_idx_spec` (both private/ghost in
// channel.rs, so unreachable from test code).
fn end_idx_exec(end: ChanEnd) -> usize {
    match end {
        ChanEnd::A => 0,
        ChanEnd::B => 1,
    }
}

// Run the real `endpoint_cap_dropped` and assert its §3e contract against the
// body: `slot_view` unchanged; `end_caps[end]` decremented with every other
// channel field untouched (the `..old` frame); and `refs_view` unchanged **iff**
// the count did not reach zero (the conditional frame — a zero drop fires the
// peer-closed event via `signal`, which is permitted to perturb `refs_view`).
fn check_endpoint_cap_dropped(st: &mut ArrayStore, ch: ObjId, end: ChanEnd) {
    let e = end_idx_exec(end);
    assert!(chan_wf_exec(st, ch), "endpoint_cap_dropped pre: chan_wf");
    let before = st.chan(ch).end_caps[e];
    assert!(before > 0, "endpoint_cap_dropped pre: end_caps[end] > 0");
    let fp = fingerprint(st);
    let refs_before = st.refs.clone();
    let mut expect_chan = st.chan(ch).clone();
    expect_chan.end_caps[e] = before - 1;

    endpoint_cap_dropped(st, ch, end);

    assert!(fingerprint(st) == fp, "endpoint_cap_dropped: slot_view unchanged");
    assert!(*st.chan(ch) == expect_chan, "endpoint_cap_dropped: only end_caps[end] decremented");
    if before != 1 {
        assert!(st.refs == refs_before, "endpoint_cap_dropped: refs_view unchanged (no fire)");
    }
}

// Run the real `bind` and assert its §3e contract against the body: `slot_view`
// unchanged; the `(end, event)` binding installed with every other channel field
// untouched; and the `refs_view` delta — old notif released, new acquired, in the
// decrement-then-increment order so a same-notif rebind is net-zero
// (`bind_refs_post`).
fn check_bind(st: &mut ArrayStore, ch: ObjId, end: ChanEnd, event: usize, notif: Option<ObjId>, bits: u64) {
    let e = end_idx_exec(end);
    let old_notif = st.chan(ch).bindings.get(&(e, event)).copied().unwrap_or(Binding::UNBOUND).notif;
    let fp = fingerprint(st);
    let refs_before = st.refs.clone();
    let mut expect_chan = st.chan(ch).clone();
    expect_chan.bindings.insert((e, event), Binding { notif, bits });
    let mut expect_refs = refs_before.clone();
    if let Some(no) = old_notif {
        *expect_refs.get_mut(&no.0).unwrap() -= 1;
    }
    if let Some(nn) = notif {
        *expect_refs.get_mut(&nn.0).unwrap() += 1;
    }

    bind(st, ch, end, event, notif, bits);

    assert!(fingerprint(st) == fp, "bind: slot_view unchanged");
    assert!(*st.chan(ch) == expect_chan, "bind: only the (end,event) binding changed");
    assert_eq!(st.refs, expect_refs, "bind: refs delta (old -1, new +1)");
}

// Run the real `destroy_channel` and assert its assumed §3e contract against the
// body — the `external_body`-vs-real-body discipline (`delete`/`signal`). The
// contract's checkable core: `cspace_wf` preserved, the arena unchanged in
// extent, and **every ring-cap slot emptied**. The host test also checks the part
// kept out of the formal contract: each bound binding's notif ref released once.
fn check_destroy_channel(st: &mut ArrayStore, ch: ObjId) {
    assert!(cspace_wf_exec(st), "destroy_channel pre: cspace_wf");
    assert!(chan_wf_exec(st, ch), "destroy_channel pre: chan_wf");
    let n = st.n();
    let depth = st.chan(ch).depth;
    let ring_caps: Vec<SlotId> = (0..2usize)
        .flat_map(|r| (0..depth).flat_map(move |i| (0..4usize).map(move |c| (r, i, c))))
        .map(|(r, i, c)| st.chan(ch).ring_cap[&(r, i, c)])
        .collect();
    let mut expect_refs = st.refs.clone();
    for ev in 0..2usize {
        for v in 0..3usize {
            if let Some(notif) = st.chan(ch).bindings[&(ev, v)].notif {
                *expect_refs.get_mut(&notif.0).unwrap() -= 1;
            }
        }
    }

    destroy_channel(st, ch);

    assert!(cspace_wf_exec(st), "destroy_channel post: cspace_wf preserved");
    assert_eq!(st.n(), n, "destroy_channel: arena extent unchanged");
    for cs in ring_caps {
        assert!(st.at(cs).cap.is_empty(), "destroy_channel: every ring cap slot emptied");
    }
    assert_eq!(st.refs, expect_refs, "destroy_channel: each binding's notif ref released once");
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
fn retype_install_arms() {
    // Frame inherits the untyped's rights (0xff here, PHYS included).
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(untyped_cap(0x1000, 0x10000, 0));
    check_retype_install(
        &mut st,
        SlotId(0),
        ObjType::Frame,
        CapKind::Frame { base: 0x2000, pages: 1, mapping: None },
        0x5000,
        SlotId(1),
        None,
    );

    // Thread → THREAD_ALL.
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(untyped_cap(0x1000, 0x10000, 0));
    st.refs.insert(50, 1);
    check_retype_install(
        &mut st,
        SlotId(0),
        ObjType::Thread,
        CapKind::Thread(ObjId(50)),
        0x4000,
        SlotId(1),
        None,
    );

    // Sub-Untyped masked to READ|WRITE — PHYS provably stripped (the untyped has it).
    // Rights = READ|PHYS so masked = READ (1) differs from ALL (3): teeth vs mutation.
    let mut st = ArrayStore::new(3);
    let mut uc = untyped_cap(0x1000, 0x10000, 0);
    uc.rights = Rights(Rights::READ | Rights::PHYS);
    st.slots[0] = detached(uc);
    check_retype_install(
        &mut st,
        SlotId(0),
        ObjType::Untyped,
        CapKind::Untyped { base: 0x2000, size: 0x1000, watermark: 0 },
        0x3000,
        SlotId(1),
        None,
    );

    // CSpace → ALL.
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(untyped_cap(0x1000, 0x10000, 0));
    st.refs.insert(60, 1);
    check_retype_install(
        &mut st,
        SlotId(0),
        ObjType::CSpace,
        CapKind::CSpace(ObjId(60)),
        0x4000,
        SlotId(1),
        None,
    );

    // Channel: endpoint A in dst, B in dst2, refs → 2, end_caps → [1, 1].
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(untyped_cap(0x1000, 0x10000, 0));
    let ch = ObjId(70);
    st.refs.insert(70, 1);
    st.chans.insert(
        70,
        ChanState {
            depth: 1,
            end_caps: [0, 0],
            head: [0, 0],
            count: [0, 0],
            bindings: BTreeMap::new(),
            msg_len: BTreeMap::new(),
            ring_cap: BTreeMap::new(),
        },
    );
    // A second, unrelated channel (ObjId 99) so the "other channels untouched"
    // frame-check loop in check_retype_install actually runs (it skips ch == 70) —
    // pinning the `forall|o| o != ch ==> chan_view[o] unchanged` postcondition.
    st.chans.insert(
        99,
        ChanState {
            depth: 2,
            end_caps: [1, 1],
            head: [0, 1],
            count: [1, 0],
            bindings: BTreeMap::new(),
            msg_len: BTreeMap::new(),
            ring_cap: BTreeMap::new(),
        },
    );
    st.refs.insert(99, 1);
    check_retype_install(
        &mut st,
        SlotId(0),
        ObjType::Channel,
        CapKind::Channel(ch, ChanEnd::A),
        0x4000,
        SlotId(1),
        Some(SlotId(2)),
    );
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

// ── Channel ghost view (plan §3b) ──────────────────────────────────────────

#[test]
fn signal_frame() {
    // The assumed `signal` contract: the real body leaves `slot_view`/`chan_view`
    // untouched on BOTH paths, while its intended effects (accumulate / deliver)
    // still happen — so the frame is real, not a no-op masquerading as one.

    // No-waiter: the bits accumulate in the word; nothing else moves.
    let (mut st, n) = signal_fixture(false);
    assert!(chan_wf_exec(&st, ObjId(7)), "fixture channel is well-formed");
    check_signal_frame(&mut st, n, 0b101);
    assert_eq!(st.notifs[&100].word, 0b101, "no-waiter signal accumulated the bits");

    // One waiter: the whole word is delivered, cleared, and the queued ref freed
    // — all OUTSIDE the slot/chan frame the contract pins.
    let (mut st, n) = signal_fixture(true);
    check_signal_frame(&mut st, n, 0b110);
    assert_eq!(st.notifs[&100].word, 0, "delivered word cleared");
    assert_eq!(st.tcbs[&200].retval, 0b110, "waiter received the whole word");
    assert!(st.notifs[&100].wait_head.is_none(), "waiter dequeued");
    assert_eq!(st.refs[&100], 0, "waiter's queued ref released");
    // The §4a `make_runnable` contract, host-checked: the woken thread is Runnable.
    assert_eq!(st.tcbs[&200].state, ThreadState::Runnable, "woken waiter made Runnable");
}

#[test]
fn chan_wf_exec_has_teeth() {
    // `chan_wf_exec` (and so `check_signal_frame`'s precondition) is only
    // meaningful if it rejects malformed channels. Each shape violates exactly
    // one clause; the windowing coupling (out-of-window slot non-empty) is the
    // load-bearing §3b clause.
    let ch = ObjId(7);
    assert!(chan_wf_exec(&signal_fixture(false).0, ch), "a well-formed channel must be accepted");

    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().count[1] = 2;
    assert!(!chan_wf_exec(&st, ch), "count > depth must be rejected");

    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().head[0] = 1;
    assert!(!chan_wf_exec(&st, ch), "head >= depth must be rejected");

    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().depth = 0;
    assert!(!chan_wf_exec(&st, ch), "depth 0 must be rejected");

    // ring 1 (count 0) idx 0 cap 0 is slot 5 — out of every live window, so it
    // must be empty; make it non-empty.
    let mut st = signal_fixture(false).0;
    st.slots[5] = detached(frame_cap(5));
    assert!(!chan_wf_exec(&st, ch), "out-of-window non-empty ring cap must be rejected");

    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().ring_cap.insert((1, 0, 0), SlotId(999));
    assert!(!chan_wf_exec(&st, ch), "ring cap handle outside the arena must be rejected");

    // Injectivity (§3d): (1,0,1) aliases (1,0,0)'s slot 5. Both ring-1 caps are
    // out-of-window and slot 5 is empty, so the windowing clause is satisfied —
    // only the injectivity clause rejects this.
    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().ring_cap.insert((1, 0, 1), SlotId(5));
    assert!(!chan_wf_exec(&st, ch), "two ring positions aliasing one slot must be rejected");

    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().bindings.remove(&(1, 2));
    assert!(!chan_wf_exec(&st, ch), "incomplete bindings domain must be rejected");

    assert!(!chan_wf_exec(&signal_fixture(false).0, ObjId(999)), "unknown channel must be rejected");
}

// ── Notification waiter-queue well-formedness (plan §4a) ────────────────────

// A notification (ObjId 100) with a two-deep FIFO waiter chain 200 → 201: the
// `notif_wf` fixture for the teeth test. `notif_wf_exec` must accept it.
fn notif_fixture() -> ArrayStore {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.notifs.insert(100, NotifState { word: 0, wait_head: Some(ObjId(200)), wait_tail: Some(ObjId(201)) });
    st.tcbs.insert(
        200,
        TcbState { state: ThreadState::BlockedNotif, wait_notif: Some(n), qnext: Some(ObjId(201)), ..tcb_state_default() },
    );
    st.tcbs.insert(
        201,
        TcbState { state: ThreadState::BlockedNotif, wait_notif: Some(n), qnext: None, ..tcb_state_default() },
    );
    st
}

#[test]
fn notif_wf_exec_has_teeth() {
    // `notif_wf_exec` (the precondition 4b/4c's `signal`/`wait`/`remove_waiter`
    // proofs rest on) is only meaningful if it rejects malformed queues. Each shape
    // violates exactly one clause; a well-formed queue is accepted.
    let n = ObjId(100);
    assert!(notif_wf_exec(&notif_fixture(), n), "a well-formed waiter queue must be accepted");

    // empty-queue head/tail disagreement: head Some, tail None.
    let mut st = notif_fixture();
    st.notifs.get_mut(&100).unwrap().wait_tail = None;
    assert!(!notif_wf_exec(&st, n), "head/tail disagreement must be rejected");

    // a qnext cycle (201 → 200 → 201 …): the walk never terminates.
    let mut st = notif_fixture();
    st.tcbs.get_mut(&201).unwrap().qnext = Some(ObjId(200));
    assert!(!notif_wf_exec(&st, n), "a qnext cycle must be rejected");

    // a charted node naming the wrong notification.
    let mut st = notif_fixture();
    st.tcbs.get_mut(&201).unwrap().wait_notif = Some(ObjId(999));
    assert!(!notif_wf_exec(&st, n), "a waiter naming another notification must be rejected");

    // a charted node not in BlockedNotif.
    let mut st = notif_fixture();
    st.tcbs.get_mut(&201).unwrap().state = ThreadState::Runnable;
    assert!(!notif_wf_exec(&st, n), "a non-BlockedNotif waiter must be rejected");

    // wait_tail names a node that is not the chain's end.
    let mut st = notif_fixture();
    st.notifs.get_mut(&100).unwrap().wait_tail = Some(ObjId(200));
    assert!(!notif_wf_exec(&st, n), "wait_tail off the chain end must be rejected");

    // a charted node that is not a live TCB (201 removed, 200 still points at it).
    let mut st = notif_fixture();
    st.tcbs.remove(&201);
    assert!(!notif_wf_exec(&st, n), "a charted node with no live TCB must be rejected");

    assert!(!notif_wf_exec(&notif_fixture(), ObjId(999)), "unknown notification must be rejected");
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

// ── Channel send/recv (plan §3d): the FIFO core, host-differential ──────────
//
// `send`/`recv` carry full Verus contracts (FIFO `Seq` push/pop, move totality,
// two-pass atomicity, null-slot tolerance) — proven, not assumed. These run the
// real bodies on `ArrayStore` and assert the observable effects, keeping the
// `test_store` cadence and guarding against spec/body drift.

// A well-formed empty channel (ObjId 7) of the given depth: the `2*depth*4` ring
// cap slots occupy arena indices `[0, 2*depth*4)`, with `scratch` spare slots
// after them for sender/dest caps. Both ends live (`end_caps [1,1]`) so `send`
// never PeerCloses. Returns the store, the channel id, and the first scratch idx.
fn chan_fixture(depth: u32, scratch: usize) -> (ArrayStore, ObjId, usize) {
    let ring_slots = (2 * depth as usize) * 4;
    let mut st = ArrayStore::new(ring_slots + scratch);
    let mut ring_cap = BTreeMap::new();
    let mut slot = 0u64;
    for ring in 0..2usize {
        for i in 0..depth {
            for c in 0..4usize {
                ring_cap.insert((ring, i, c), SlotId(slot));
                slot += 1;
            }
        }
    }
    let mut bindings = BTreeMap::new();
    for e in 0..2usize {
        for v in 0..3usize {
            bindings.insert((e, v), Binding::UNBOUND);
        }
    }
    let mut msg_len = BTreeMap::new();
    for ring in 0..2usize {
        for i in 0..depth {
            msg_len.insert((ring, i), 0u16);
        }
    }
    st.chans.insert(
        7,
        ChanState { depth, end_caps: [1, 1], head: [0, 0], count: [0, 0], bindings, msg_len, ring_cap },
    );
    (st, ObjId(7), ring_slots)
}

#[test]
fn send_recv_roundtrip() {
    // depth 2; A sends two messages (the first carrying a cap), B receives both
    // FIFO — the cap is moved out of the sender and into the receiver's dest.
    let (mut st, ch, scratch0) = chan_fixture(2, 4);
    assert!(chan_wf_exec(&st, ch));
    let send_cap = SlotId(scratch0 as u64);
    st.slots[scratch0] = detached(frame_cap(99));
    // msg 1: len 3 + a cap in slot 0.
    assert_eq!(send(&mut st, ch, ChanEnd::A, &[1u8, 2, 3], &[Some(send_cap), None, None, None]), Ok(()));
    assert!(chan_wf_exec(&st, ch));
    assert_eq!(st.chan(ch).count[0], 1, "A sends on ring 0");
    assert!(st.at(send_cap).cap.is_empty(), "sender slot emptied (move totality)");
    // msg 2: len 5, no caps.
    assert_eq!(send(&mut st, ch, ChanEnd::A, &[0u8; 5], &[None; 4]), Ok(()));
    assert_eq!(st.chan(ch).count[0], 2);

    // B receives on ring 0 (1 - end_idx(B)); the head (msg 1) comes out first.
    let dest = SlotId((scratch0 + 1) as u64);
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(&mut st, ch, ChanEnd::B, &mut buf, &[Some(dest), None, None, None]),
        Ok((3, 0b1)),
        "FIFO head delivered first, carrying its cap (mask bit 0)"
    );
    assert!(!st.at(dest).cap.is_empty(), "cap delivered to the dest slot");
    assert_eq!(st.chan(ch).count[0], 1);
    assert!(chan_wf_exec(&st, ch));
    // msg 2 next, in order.
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]), Ok((5, 0)), "second message in order");
    assert_eq!(st.chan(ch).count[0], 0);
}

#[test]
fn send_full_and_recv_empty() {
    // recv on an empty ring → Empty (unchanged); fill the depth-1 ring; the next
    // send → Full (unchanged) — the read-only guard frames.
    let (mut st, ch, _) = chan_fixture(1, 0);
    let chans0 = st.chans.clone();
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]), Err(ChanError::Empty));
    assert!(st.chans == chans0, "recv Empty: channel unchanged");

    assert_eq!(send(&mut st, ch, ChanEnd::A, &[7u8], &[None; 4]), Ok(()));
    assert_eq!(st.chan(ch).count[0], 1);
    let fp = fingerprint(&st);
    let chans1 = st.chans.clone();
    assert_eq!(send(&mut st, ch, ChanEnd::A, &[8u8], &[None; 4]), Err(ChanError::Full));
    assert_eq!(fingerprint(&st), fp, "send Full: arena unchanged");
    assert!(st.chans == chans1, "send Full: channel unchanged");
}

#[test]
fn recv_nocapslot_atomic() {
    // A sends a cap; B recvs with no dest for it → NoCapSlot, and the message
    // stays fully queued (two-pass atomicity: pass 1 is read-only).
    let (mut st, ch, scratch0) = chan_fixture(1, 2);
    let send_cap = SlotId(scratch0 as u64);
    st.slots[scratch0] = detached(frame_cap(42));
    assert_eq!(send(&mut st, ch, ChanEnd::A, &[1u8], &[Some(send_cap), None, None, None]), Ok(()));
    let fp = fingerprint(&st);
    let chans = st.chans.clone();
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]), Err(ChanError::NoCapSlot));
    assert_eq!(fingerprint(&st), fp, "NoCapSlot: arena unchanged");
    assert!(st.chans == chans, "NoCapSlot: message fully queued");
    assert_eq!(st.chan(ch).count[0], 1);
}

#[test]
fn recv_null_slot_tolerance() {
    // A sends a cap; revocation empties the queued ring cap in flight; B's recv
    // delivers it as absent (mask bit clear) — never a panic (§3.4 null slots).
    let (mut st, ch, scratch0) = chan_fixture(1, 2);
    let send_cap = SlotId(scratch0 as u64);
    st.slots[scratch0] = detached(frame_cap(7));
    assert_eq!(send(&mut st, ch, ChanEnd::A, &[0u8; 3], &[Some(send_cap), None, None, None]), Ok(()));
    // simulate a revoke emptying the queued ring cap (an in-window slot may be empty).
    let rc = st.chan(ch).ring_cap[&(0, 0, 0)];
    st.slots[rc.0 as usize] = CapSlot::empty();
    assert!(chan_wf_exec(&st, ch));
    let dest = SlotId((scratch0 + 1) as u64);
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(&mut st, ch, ChanEnd::B, &mut buf, &[Some(dest), None, None, None]),
        Ok((3, 0)),
        "null cap delivered as absent (mask 0), no panic"
    );
    assert!(st.at(dest).cap.is_empty(), "dest stays empty (nothing moved)");
    assert_eq!(st.chan(ch).count[0], 0, "still dequeued");
}

#[test]
fn randomized_fifo_sweep() {
    // Random send/recv on the A→B ring against a reference deque of message
    // lengths; assert FIFO order, count tracking, and chan_wf_exec throughout —
    // the executable counterpart of the ring_fifo Seq proof, across wraparound.
    let mut trials = 0usize;
    for seed in 0..120u64 {
        let depth = 1 + (seed % 4) as u32; // 1..=4
        let (mut st, ch, _) = chan_fixture(depth, 0);
        let mut model: VecDeque<u16> = VecDeque::new();
        let mut rng = seed.wrapping_mul(2654435761).wrapping_add(1);
        for _ in 0..30 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let do_send = (rng >> 33) & 1 == 0;
            if do_send && model.len() < depth as usize {
                let len = ((rng >> 3) % 200) as u16;
                assert_eq!(send(&mut st, ch, ChanEnd::A, &vec![0u8; len as usize], &[None; 4]), Ok(()));
                model.push_back(len);
                trials += 1;
            } else if !model.is_empty() {
                let mut buf = [0u8; MSG_PAYLOAD];
                let r = recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]);
                assert_eq!(r, Ok((model.pop_front().unwrap() as usize, 0)), "FIFO head len matches model");
                trials += 1;
            }
            assert!(chan_wf_exec(&st, ch), "chan_wf preserved through the sweep");
            assert_eq!(st.chan(ch).count[0], model.len() as u32, "count tracks the model");
        }
    }
    assert!(trials > 300, "sweep should exercise hundreds of ops, ran {trials}");
}

#[test]
fn endpoint_cap_dropped_decrement_and_fire() {
    // Non-firing: end_caps[A] = 2 → 1, no peer-closed fire — `refs_view` and the
    // rest of the channel untouched.
    let (mut st, ch, _) = chan_fixture(1, 0);
    st.chan_mut(ch).end_caps = [2, 1];
    check_endpoint_cap_dropped(&mut st, ch, ChanEnd::A);
    assert_eq!(st.chan(ch).end_caps, [1, 1]);

    // Firing: end_caps[A] = 1 → 0 fires the *other* end's (1 - e = 1) peer-closed
    // binding into a live notif; the bits land in the notif word (signal
    // delivered) while the slot/chan frame still holds.
    let (mut st, ch, _) = chan_fixture(1, 0);
    let n = ObjId(100);
    st.refs.insert(100, 1);
    st.notifs.insert(100, NotifState { word: 0, wait_head: None, wait_tail: None });
    st.chan_mut(ch).bindings.insert((1, EV_PEER_CLOSED), Binding { notif: Some(n), bits: 0b100 });
    check_endpoint_cap_dropped(&mut st, ch, ChanEnd::A);
    assert_eq!(st.chan(ch).end_caps, [0, 1]);
    assert_eq!(st.notifs[&100].word, 0b100, "peer-closed fired into the bound notif");
}

#[test]
fn bind_install_rebind_unbind() {
    // The four refcount cases on one binding, in sequence (each `check_bind`
    // snapshots and asserts its own delta): install onto unbound, rebind to a
    // different notif, rebind to the *same* notif (net-zero), unbind.
    let (mut st, ch, _) = chan_fixture(1, 0);
    let n1 = ObjId(100);
    let n2 = ObjId(101);
    st.refs.insert(100, 5);
    st.refs.insert(101, 5);

    // install (old None → +1 on n1).
    check_bind(&mut st, ch, ChanEnd::A, EV_READABLE, Some(n1), 0b1);
    assert_eq!(st.refs[&100], 6);

    // rebind to a different notif (−1 n1, +1 n2).
    check_bind(&mut st, ch, ChanEnd::A, EV_READABLE, Some(n2), 0b10);
    assert_eq!(st.refs[&100], 5, "old notif released");
    assert_eq!(st.refs[&101], 6, "new notif acquired");

    // rebind to the same notif (−1 then +1 on n2 == net zero).
    check_bind(&mut st, ch, ChanEnd::A, EV_READABLE, Some(n2), 0b11);
    assert_eq!(st.refs[&101], 6, "same-notif rebind is net-zero");

    // unbind (old n2 → −1, no new).
    check_bind(&mut st, ch, ChanEnd::A, EV_READABLE, None, 0);
    assert_eq!(st.refs[&101], 5, "unbind released the notif");
}

#[test]
fn destroy_channel_deletes_caps_and_releases_bindings() {
    // depth-1 channel with a queued cap in each ring (count = 1 keeps index 0
    // in-window, so the non-empty caps satisfy chan_wf) and two event bindings to
    // live notifs. Teardown deletes every ring cap and releases each binding ref.
    let (mut st, ch, _) = chan_fixture(1, 0);
    st.chan_mut(ch).count = [1, 1];
    let c0 = st.chan(ch).ring_cap[&(0, 0, 0)];
    let c1 = st.chan(ch).ring_cap[&(1, 0, 0)];
    st.slots[c0.0 as usize] = detached(frame_cap(1));
    st.slots[c1.0 as usize] = detached(frame_cap(2));
    let n1 = ObjId(100);
    let n2 = ObjId(101);
    st.refs.insert(100, 3);
    st.refs.insert(101, 3);
    st.chan_mut(ch).bindings.insert((0, EV_PEER_CLOSED), Binding { notif: Some(n1), bits: 0b1 });
    st.chan_mut(ch).bindings.insert((1, EV_READABLE), Binding { notif: Some(n2), bits: 0b1 });

    check_destroy_channel(&mut st, ch);

    assert_eq!(st.refs[&100], 2, "peer-closed binding's notif released");
    assert_eq!(st.refs[&101], 2, "readable binding's notif released");
}
