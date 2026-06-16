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
use crate::notification::{destroy_notif, remove_waiter, signal, wait};
use crate::timer::{arm, check_expired, destroy_timer, disarm};
use crate::untyped::{reset, retype_check, retype_install, ObjType, RetypeError};
use crate::store::{Binding, Store};
use crate::thread::{
    bind as thread_bind, destroy_tcb, report_terminal, Report, ThreadState, BIND_EXIT, BIND_FAULT,
};
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
    // No-op so `map_in` host tests (`aspace::map_in`) can use `ArrayStore` purely
    // as the barrier supplier; the real `dsb`/`isb` is the kernel shell's job.
    // `tlb_invalidate_page`/`barrier_after_unmap` stay `unimplemented!()` (5e).
    fn barrier_after_map(&mut self) {}
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

// The exec mirror of `timer_wf` (plan §4e): the armed list from `timer_armed_head`,
// threaded through `next`, is a finite duplicate-free chain whose every node is a live,
// armed timer with a bound notification (`timer_chain`), and it captures EVERY armed
// timer (`timer_complete`). The completeness sweep is what makes `disarm`'s walk sound.
fn timer_wf_exec(st: &ArrayStore) -> bool {
    let mut cur = st.timer_armed_head;
    let mut seen: Vec<u64> = Vec::new();
    let mut steps = 0usize;
    while let Some(c) = cur {
        steps += 1;
        if steps > st.timers.len() + 1 {
            return false; // a cycle (walk longer than the timer count)
        }
        let tm = match st.timers.get(&c.0) {
            Some(v) => v,
            None => return false, // a charted node is not a live timer
        };
        if !tm.armed || tm.notif.is_none() {
            return false; // a charted node must be armed with a bound notification
        }
        if seen.contains(&c.0) {
            return false; // a duplicate (defensive; the steps cap also bounds cycles)
        }
        seen.push(c.0);
        cur = tm.next;
    }
    // completeness: every armed timer is on the chain.
    for (id, tm) in st.timers.iter() {
        if tm.armed && !seen.contains(id) {
            return false;
        }
    }
    true
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
fn notif_cap(o: u64) -> Cap {
    Cap { kind: CapKind::Notification(ObjId(o)), rights: Rights(0xff) }
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
    assert!(notif_wf_exec(st, n), "signal pre: notif_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    signal(st, n, bits);
    assert!(fingerprint(st) == fp, "signal post: slot_view unchanged");
    assert!(st.chans == chans, "signal post: chan_view unchanged");
    assert!(notif_wf_exec(st, n), "signal post: notif_wf preserved");
}

// Run the real `notification::remove_waiter` and assert its proven (§4c) frame: the
// `slot_view`/`chan_view` are untouched, `notif_wf` is preserved, and the queued-ref
// release happens iff `t` was on the queue (the host check of the per-op refcount
// delta + the splice). The executable counterpart of the proven contract.
fn check_remove_waiter(st: &mut ArrayStore, n: ObjId, t: ObjId, queued: bool) {
    assert!(notif_wf_exec(st, n), "remove_waiter pre: notif_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    let refs0 = st.refs[&n.0];
    remove_waiter(st, n, t);
    assert!(fingerprint(st) == fp, "remove_waiter post: slot_view unchanged");
    assert!(st.chans == chans, "remove_waiter post: chan_view unchanged");
    assert!(notif_wf_exec(st, n), "remove_waiter post: notif_wf preserved");
    if queued {
        assert!(st.tcbs[&t.0].qnext.is_none(), "removed waiter's qnext cleared");
        assert!(st.tcbs[&t.0].wait_notif.is_none(), "removed waiter's wait_notif cleared");
        assert_eq!(st.refs[&n.0], refs0 - 1, "queued ref released");
    } else {
        assert_eq!(st.refs[&n.0], refs0, "absent removal touches no ref");
    }
}

// The exec mirror of `cspace::binding_notif_wf`: every bound endpoint event of `ch`
// names a notification that is resident and `notif_wf`. The plain-Rust re-expression
// of the §4b named binding invariant (the `notif_wf_exec` discipline).
fn binding_notif_wf_exec(st: &ArrayStore, ch: ObjId) -> bool {
    let cv = match st.chans.get(&ch.0) {
        Some(c) => c,
        None => return false,
    };
    for e in 0..2usize {
        for v in 0..3usize {
            if let Some(b) = cv.bindings.get(&(e, v)) {
                if let Some(m) = b.notif {
                    if !st.notifs.contains_key(&m.0) {
                        return false; // a binding names a non-resident notification
                    }
                    if !notif_wf_exec(st, m) {
                        return false; // a binding names a malformed-queue notification
                    }
                }
            }
        }
    }
    true
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

// Run the real `delete` on a **notification** cap and assert the §4d conditional
// frame against the body — the mandatory executable check of `delete`'s new assumed
// `ensures` (the `external_body`-vs-real-body discipline): the TCB/channel/timer/notif
// views and every *other* slot's cap are untouched, and the designated `refs[n]` drops
// by one (the part the formal contract leaves to the host test). Refs start > 1 so the
// delete just decrements (no `destroy_notif`), isolating the frame.
fn check_delete_notif(st: &mut ArrayStore, slot: SlotId, n: ObjId) {
    assert!(cspace_wf_exec(st), "delete_notif pre: cspace_wf");
    assert!(matches!(st.at(slot).cap.kind, CapKind::Notification(_)), "delete_notif pre: notif cap");
    let (n0, c0) = (st.n(), count_nonempty_exec(st));
    let fp = fingerprint(st);
    let tcbs0 = st.tcbs.clone();
    let chans0 = st.chans.clone();
    let timers0 = st.timers.clone();
    let head0 = st.timer_armed_head;
    let notifs0 = st.notifs.clone();
    let refs0 = st.refs.clone();

    delete(st, slot);

    // base `delete` contract
    assert!(cspace_wf_exec(st), "delete_notif post: cspace_wf preserved");
    assert_eq!(st.n(), n0, "delete_notif post: dom preserved");
    assert!(st.at(slot).cap.is_empty(), "delete_notif post: target slot emptied");
    assert!(count_nonempty_exec(st) < c0, "delete_notif post: count_nonempty drops");
    // the §4d conditional-notification object-view frame
    assert!(st.tcbs == tcbs0, "delete_notif: tcb_view unchanged");
    assert!(st.chans == chans0, "delete_notif: chan_view unchanged");
    assert!(st.timers == timers0, "delete_notif: timer_view unchanged");
    assert!(st.timer_armed_head == head0, "delete_notif: timer_head_view unchanged");
    assert!(st.notifs == notifs0, "delete_notif: notif_view unchanged");
    // every *other* slot is untouched (a notif delete re-parents nothing — it is a leaf).
    let fp_after = fingerprint(st);
    for i in 0..st.n() {
        if SlotId(i as u64) != slot {
            assert!(fp_after[i] == fp[i], "delete_notif: other slot unchanged");
        }
    }
    // refs: the designated notif dropped by one (host-checked, kept out of the frame).
    let mut expect_refs = refs0.clone();
    *expect_refs.get_mut(&n.0).unwrap() -= 1;
    assert_eq!(st.refs, expect_refs, "delete_notif: refs[n] -= 1");
}

// Run the real `thread::bind` and assert its §4d contract against the body: `cspace_wf`
// preserved; only `bind_bits[which]` changes in `tcb_view`; the bind slot ends holding
// the moved cap (or empty on a `None` src) with `src` emptied; and the refs delta —
// the displaced notification released (`-1`), the moved-in cap net-zero (a move, not a
// copy, unlike `channel::bind`'s `+1`). The TCB analog of `check_bind`.
fn check_thread_bind(st: &mut ArrayStore, t: ObjId, which: usize, notif_src: Option<SlotId>, bits: u64) {
    assert!(cspace_wf_exec(st), "thread_bind pre: cspace_wf");
    let slot = st.tcbs[&t.0].bind_slots[which];
    let old_displaced = match st.at(slot).cap.kind {
        CapKind::Notification(no) => Some(no),
        _ => None,
    };
    let refs0 = st.refs.clone();
    let tcbs0 = st.tcbs.clone();

    thread_bind(st, t, which, notif_src, bits);

    assert!(cspace_wf_exec(st), "thread_bind post: cspace_wf preserved");
    // tcb_view: only bind_bits[which] changed.
    let mut expect_tcbs = tcbs0.clone();
    expect_tcbs.get_mut(&t.0).unwrap().bind_bits[which] = bits;
    assert!(st.tcbs == expect_tcbs, "thread_bind: only bind_bits[which] changed in tcb_view");
    // slot effect.
    match notif_src {
        Some(src) => {
            assert!(!st.at(slot).cap.is_empty(), "thread_bind: moved cap now in the bind slot");
            assert!(st.at(src).cap.is_empty(), "thread_bind: src emptied by the move");
        }
        None => assert!(st.at(slot).cap.is_empty(), "thread_bind: unbind leaves the bind slot empty"),
    }
    // refs delta: displaced notif -1; the moved cap is net-zero.
    let mut expect_refs = refs0.clone();
    if let Some(no) = old_displaced {
        *expect_refs.get_mut(&no.0).unwrap() -= 1;
    }
    assert_eq!(st.refs, expect_refs, "thread_bind: displaced notif -1, move net-zero");
}

// `arm`'s ensures against the real body (plan §4e): `timer_wf` preserved; slot/chan/notif/
// tcb views framed; `t` ends armed at the list head bound to `notif`; and the net ref
// delta is `disarm`'s -1 (on re-arm) plus arm's +1 — net-zero on a same-notif re-arm.
fn check_arm(st: &mut ArrayStore, t: ObjId, notif: ObjId, bits: u64, deadline: u64) {
    assert!(timer_wf_exec(st), "arm pre: timer_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    let notifs = st.notifs.clone();
    let tcbs = st.tcbs.clone();
    let was_armed = st.timers[&t.0].armed;
    let old_notif = st.timers[&t.0].notif;
    let mut expect_refs = st.refs.clone();
    if was_armed {
        if let Some(m) = old_notif {
            *expect_refs.get_mut(&m.0).unwrap() -= 1;
        }
    }
    *expect_refs.get_mut(&notif.0).unwrap() += 1;

    arm(st, t, notif, bits, deadline);

    assert!(timer_wf_exec(st), "arm post: timer_wf preserved");
    assert!(fingerprint(st) == fp, "arm post: slot_view unchanged");
    assert!(st.chans == chans, "arm post: chan_view unchanged");
    assert!(st.notifs == notifs, "arm post: notif_view unchanged");
    assert!(st.tcbs == tcbs, "arm post: tcb_view unchanged");
    assert_eq!(st.refs, expect_refs, "arm: net ref delta (re-arm -1, arm +1)");
    assert!(st.timers[&t.0].armed, "arm: t armed");
    assert!(st.timers[&t.0].notif == Some(notif), "arm: bound to notif");
    assert_eq!(st.timers[&t.0].deadline, deadline, "arm: deadline set");
    assert_eq!(st.timers[&t.0].bits, bits, "arm: bits set");
    assert!(st.timer_armed_head == Some(t), "arm: pushed onto the list head");
}

// `disarm`'s ensures against the real body (plan §4e): `timer_wf` preserved; the views
// framed; `t` cleared (armed/notif/next) and off the armed list; its notif ref released
// iff it was armed.
fn check_disarm(st: &mut ArrayStore, t: ObjId) {
    assert!(timer_wf_exec(st), "disarm pre: timer_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    let notifs = st.notifs.clone();
    let tcbs = st.tcbs.clone();
    let was_armed = st.timers[&t.0].armed;
    let old_notif = st.timers[&t.0].notif;
    let mut expect_refs = st.refs.clone();
    if was_armed {
        if let Some(m) = old_notif {
            *expect_refs.get_mut(&m.0).unwrap() -= 1;
        }
    }

    disarm(st, t);

    assert!(timer_wf_exec(st), "disarm post: timer_wf preserved");
    assert!(fingerprint(st) == fp, "disarm post: slot_view unchanged");
    assert!(st.chans == chans, "disarm post: chan_view unchanged");
    assert!(st.notifs == notifs, "disarm post: notif_view unchanged");
    assert!(st.tcbs == tcbs, "disarm post: tcb_view unchanged");
    assert_eq!(st.refs, expect_refs, "disarm: released the timer's ref iff it was armed");
    assert!(!st.timers[&t.0].armed, "disarm: t unarmed");
    assert!(st.timers[&t.0].notif.is_none(), "disarm: notif cleared");
    assert!(st.timers[&t.0].next.is_none(), "disarm: next cleared");
    let mut cur = st.timer_armed_head;
    while let Some(c) = cur {
        assert!(c.0 != t.0, "disarm: t no longer on the armed list");
        cur = st.timers[&c.0].next;
    }
}

// `destroy_timer` (refs == 0): `disarm` of the timer object, `timer_wf` preserved.
fn check_destroy_timer(st: &mut ArrayStore, t: ObjId) {
    assert!(timer_wf_exec(st), "destroy_timer pre: timer_wf");
    destroy_timer(st, t);
    assert!(timer_wf_exec(st), "destroy_timer post: timer_wf preserved");
    assert!(!st.timers[&t.0].armed, "destroy_timer: disarmed");
}

// `check_expired`'s ensures against the real body (plan §4e): `timer_wf` preserved, slot/
// chan views framed, and — stronger than the verified contract — every timer still on the
// armed list is unexpired (every `deadline <= now` was fired and disarmed by the sweep).
fn check_check_expired(st: &mut ArrayStore, now: u64) {
    assert!(timer_wf_exec(st), "check_expired pre: timer_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    check_expired(st, now);
    assert!(timer_wf_exec(st), "check_expired post: timer_wf preserved");
    assert!(fingerprint(st) == fp, "check_expired post: slot_view unchanged");
    assert!(st.chans == chans, "check_expired post: chan_view unchanged");
    let mut cur = st.timer_armed_head;
    while let Some(c) = cur {
        assert!(st.timers[&c.0].deadline > now, "check_expired: every survivor is unexpired");
        cur = st.timers[&c.0].next;
    }
}

// `destroy_tcb`'s assumed structural contract (plan §4e) against the real body: `t` ends
// Halted with its queue link and both binding slots cleared, its report UNCHANGED
// (destruction fires no report, §5.1), and `cspace_wf` preserved.
fn check_destroy_tcb(st: &mut ArrayStore, t: ObjId) {
    assert!(cspace_wf_exec(st), "destroy_tcb pre: cspace_wf");
    let n = st.n();
    let report0 = st.tcbs[&t.0].report;
    let s0 = st.tcbs[&t.0].bind_slots[0];
    let s1 = st.tcbs[&t.0].bind_slots[1];

    destroy_tcb(st, t);

    assert!(cspace_wf_exec(st), "destroy_tcb post: cspace_wf preserved");
    assert_eq!(st.n(), n, "destroy_tcb: arena extent unchanged");
    assert_eq!(st.tcbs[&t.0].state, ThreadState::Halted, "destroy_tcb: t halted");
    assert!(st.tcbs[&t.0].qnext.is_none(), "destroy_tcb: queue link cleared");
    assert_eq!(st.tcbs[&t.0].report, report0, "destroy_tcb: report unchanged");
    assert!(st.at(s0).cap.is_empty(), "destroy_tcb: EXIT bind slot emptied");
    assert!(st.at(s1).cap.is_empty(), "destroy_tcb: FAULT bind slot emptied");
}

// A halted thread (ObjId 200) with two bind slots (1 = EXIT, 2 = FAULT). With
// `with_binding`, slot `1+which` holds a notification cap (ObjId 100, bits 0b101) the
// thread's death will fire; with `with_waiter`, a separate thread (ObjId 201) is
// blocked on that notification (holding a queued ref) so the fire takes the wake path.
fn report_terminal_fixture(which: usize, with_binding: bool, with_waiter: bool) -> (ArrayStore, ObjId) {
    let mut st = ArrayStore::new(4);
    let t = ObjId(200);
    let mut tcb = TcbState {
        state: ThreadState::Halted,
        report: Report::Running,
        bind_slots: [SlotId(1), SlotId(2)],
        ..tcb_state_default()
    };
    if with_binding {
        tcb.bind_bits[which] = 0b101;
        st.slots[1 + which] = detached(notif_cap(100));
        if with_waiter {
            let w = ObjId(201);
            st.tcbs.insert(
                201,
                TcbState { state: ThreadState::BlockedNotif, wait_notif: Some(ObjId(100)), ..tcb_state_default() },
            );
            st.notifs.insert(100, NotifState { word: 0, wait_head: Some(w), wait_tail: Some(w) });
            st.refs.insert(100, 2); // the bind cap's ref + the queued waiter's ref
        } else {
            st.notifs.insert(100, NotifState { word: 0, wait_head: None, wait_tail: None });
            st.refs.insert(100, 1); // the bind cap's ref
        }
    }
    st.tcbs.insert(200, tcb);
    (st, t)
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
fn delete_notif_frame() {
    // Deleting a notification cap leaves every object view and every other slot
    // untouched (the §4d conditional `delete` frame `thread::bind` reads off), and
    // drops only the designated notif's refcount. Refs start at 2 so the delete just
    // decrements (no destroy), isolating the frame.
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(frame_cap(0)); // an unrelated witness slot
    st.slots[1] = detached(notif_cap(100)); // the notification cap to delete
    st.slots[2] = detached(frame_cap(2)); // another unrelated cap
    st.refs.insert(100, 2);
    st.notifs.insert(100, NotifState { word: 7, wait_head: None, wait_tail: None });
    // a second notification + a TCB + a timer, present only to witness the frame.
    st.notifs.insert(101, NotifState { word: 0, wait_head: None, wait_tail: None });
    st.refs.insert(101, 1);
    st.tcbs.insert(200, tcb_state_default());
    st.timers.insert(300, TimerState { armed: false, deadline: 0, notif: None, bits: 0, next: None });
    check_delete_notif(&mut st, SlotId(1), ObjId(100));
}

#[test]
fn thread_bind_install_rebind_unbind() {
    // The TCB-binding analog of `bind_install_rebind_unbind` (channel): install onto an
    // unbound slot (move-in, ref unchanged), rebind to a different notif (displaced
    // notif released, new moved in), unbind (displaced notif released, slot empties).
    let mut st = ArrayStore::new(6);
    st.slots[0] = detached(frame_cap(0)); // witness; slots 1/2 are the bind slots (empty)
    st.slots[3] = detached(notif_cap(100));
    st.slots[4] = detached(notif_cap(101));
    st.refs.insert(100, 1);
    st.refs.insert(101, 1);
    st.notifs.insert(100, NotifState { word: 0, wait_head: None, wait_tail: None });
    st.notifs.insert(101, NotifState { word: 0, wait_head: None, wait_tail: None });
    let t = ObjId(200);
    st.tcbs.insert(200, TcbState { bind_slots: [SlotId(1), SlotId(2)], ..tcb_state_default() });

    // install onto the unbound EXIT slot: move notif 100 (slot 3) into bind slot 1.
    check_thread_bind(&mut st, t, BIND_EXIT, Some(SlotId(3)), 0b1);
    assert_eq!(st.refs[&100], 1, "a move keeps the cap's ref (move, not copy — no +1)");
    assert!(st.at(SlotId(3)).cap.is_empty(), "src slot emptied");

    // rebind EXIT to a different notif (slot 4): old notif 100 released, 101 moved in.
    check_thread_bind(&mut st, t, BIND_EXIT, Some(SlotId(4)), 0b10);
    assert_eq!(st.refs[&100], 0, "displaced notif 100 released");
    assert_eq!(st.refs[&101], 1, "new notif moved in (net-zero)");

    // unbind EXIT (None src): displaced notif 101 released, the bind slot empties.
    check_thread_bind(&mut st, t, BIND_EXIT, None, 0);
    assert_eq!(st.refs[&101], 0, "unbind released the displaced notif");
    assert!(st.at(SlotId(1)).cap.is_empty(), "unbind leaves the bind slot empty");
}

#[test]
fn report_terminal_first_call_wins_and_fires() {
    // ReportMonotone + the fire: a Running thread's first `report_terminal` records the
    // report and fires the EXIT binding (the queued waiter is woken); a second call is
    // an absorbing no-op.
    let (mut st, t) = report_terminal_fixture(BIND_EXIT, true, true);
    let w = ObjId(201);
    report_terminal(&mut st, t, Report::Exited(42));
    assert_eq!(st.tcbs[&t.0].report, Report::Exited(42), "first call records the report");
    assert_eq!(st.tcbs[&w.0].state, ThreadState::Runnable, "the bound waiter was woken (binding fired)");
    assert_eq!(st.tcbs[&w.0].retval, 0b101, "the waiter received the binding bits");
    assert_eq!(st.refs[&100], 1, "the woken waiter's queued ref released");

    let refs_before = st.refs.clone();
    let tcbs_before = st.tcbs.clone();
    report_terminal(&mut st, t, Report::Exited(99));
    assert_eq!(st.tcbs[&t.0].report, Report::Exited(42), "second call no-op: report unchanged (absorbing)");
    assert!(st.refs == refs_before && st.tcbs == tcbs_before, "second call touches nothing");
}

#[test]
fn report_terminal_fault_arm_fires_fault_binding() {
    // A Faulted report fires the FAULT binding (BIND_FAULT), not EXIT.
    let (mut st, t) = report_terminal_fixture(BIND_FAULT, true, true);
    let w = ObjId(201);
    report_terminal(&mut st, t, Report::Faulted { cause: 0x96, far: 0xdead_0000 });
    assert!(matches!(st.tcbs[&t.0].report, Report::Faulted { .. }), "fault recorded");
    assert_eq!(st.tcbs[&w.0].state, ThreadState::Runnable, "the FAULT binding fired");
}

#[test]
fn report_terminal_accumulate_no_waiter() {
    // Firing a binding with no queued waiter accumulates the bits into the word.
    let (mut st, t) = report_terminal_fixture(BIND_EXIT, true, false);
    report_terminal(&mut st, t, Report::Exited(7));
    assert_eq!(st.tcbs[&t.0].report, Report::Exited(7), "report recorded");
    assert_eq!(st.notifs[&100].word, 0b101, "no waiter: the binding bits accumulate in the word");
}

#[test]
fn report_terminal_firesafe_empty_slot() {
    // FireSafe: an empty bind slot (a revoke raced the death and cleared it) ⇒ the fire
    // is a no-op, no panic, and the report still records.
    let (mut st, t) = report_terminal_fixture(BIND_EXIT, false, false);
    report_terminal(&mut st, t, Report::Exited(5));
    assert_eq!(st.tcbs[&t.0].report, Report::Exited(5), "report recorded even with an empty bind slot");
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

// `wait` on a nonzero word consumes it without blocking; the queue and refs are
// untouched — the executable check of `wait`'s consume-path contract (§4b).
#[test]
fn wait_consume() {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.refs.insert(100, 1);
    st.notifs.insert(100, NotifState { word: 0b1010, wait_head: None, wait_tail: None });
    let cur = ObjId(200);
    st.tcbs.insert(200, TcbState { state: ThreadState::Runnable, ..tcb_state_default() });

    assert_eq!(wait(&mut st, n, cur), Some(0b1010), "a nonzero word is consumed");
    assert_eq!(st.notifs[&100].word, 0, "word cleared");
    assert_eq!(st.tcbs[&200].state, ThreadState::Runnable, "the thread did not block");
    assert_eq!(st.refs[&100], 1, "no ref acquired on the consume path");
    assert!(st.notifs[&100].wait_head.is_none(), "queue stays empty");
    assert!(notif_wf_exec(&st, n));
}

// Block two threads, then signal twice: wake order == block order (the FIFO
// `waiter_seq` theorem, exercised on the real `wait`/`signal` bodies). Tracks the
// per-op refcount deltas (`wait` +1, `signal` -1) end to end.
#[test]
fn wait_signal_fifo() {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.refs.insert(100, 1);
    st.notifs.insert(100, NotifState { word: 0, wait_head: None, wait_tail: None });
    let t1 = ObjId(200);
    let t2 = ObjId(201);
    st.tcbs.insert(200, TcbState { state: ThreadState::Runnable, ..tcb_state_default() });
    st.tcbs.insert(201, TcbState { state: ThreadState::Runnable, ..tcb_state_default() });

    // First waiter blocks at the head; acquires a ref.
    assert_eq!(wait(&mut st, n, t1), None, "word 0 ⇒ the first thread blocks");
    assert_eq!(st.tcbs[&200].state, ThreadState::BlockedNotif);
    assert!(st.tcbs[&200].wait_notif == Some(n));
    assert!(st.notifs[&100].wait_head == Some(t1));
    assert!(st.notifs[&100].wait_tail == Some(t1));
    assert_eq!(st.refs[&100], 2, "wait acquired the waiter's ref");
    assert!(notif_wf_exec(&st, n));

    // Second waiter blocks behind the first (FIFO tail), threaded via qnext.
    assert_eq!(wait(&mut st, n, t2), None, "the second thread blocks behind the first");
    assert!(st.notifs[&100].wait_head == Some(t1), "head unchanged");
    assert!(st.notifs[&100].wait_tail == Some(t2), "tail is the new waiter");
    assert!(st.tcbs[&200].qnext == Some(t2), "t1 → t2 threaded");
    assert_eq!(st.refs[&100], 3);
    assert!(notif_wf_exec(&st, n));

    // First signal wakes the HEAD (t1) — block order — delivering the word; -1 ref.
    signal(&mut st, n, 0b1);
    assert_eq!(st.tcbs[&200].state, ThreadState::Runnable, "the head t1 wakes first");
    assert_eq!(st.tcbs[&200].retval, 0b1, "t1 received the word");
    assert!(st.notifs[&100].wait_head == Some(t2), "t2 is now the head");
    assert_eq!(st.notifs[&100].word, 0, "delivered word cleared");
    assert_eq!(st.refs[&100], 2, "the wake released t1's queued ref");
    assert!(notif_wf_exec(&st, n));

    // Second signal wakes t2, emptying the queue.
    signal(&mut st, n, 0b10);
    assert_eq!(st.tcbs[&201].state, ThreadState::Runnable, "t2 wakes second");
    assert_eq!(st.tcbs[&201].retval, 0b10);
    assert!(st.notifs[&100].wait_head.is_none(), "queue now empty");
    assert!(st.notifs[&100].wait_tail.is_none());
    assert_eq!(st.refs[&100], 1);
    assert!(notif_wf_exec(&st, n));
}

// `destroy_notif` on an empty-queue notification is a no-op (§4b).
#[test]
fn destroy_notif_noop() {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.refs.insert(100, 0);
    st.notifs.insert(100, NotifState { word: 0, wait_head: None, wait_tail: None });
    let before = st.notifs[&100].clone();
    let refs_before = st.refs.clone();
    destroy_notif(&mut st, n);
    assert!(st.notifs[&100] == before, "destroy_notif leaves the notification untouched");
    assert_eq!(st.refs, refs_before, "destroy_notif touches no refcount");
}

// `remove_waiter` splices a waiter out of the FIFO queue at head / middle / tail and
// is a no-op when `t` is absent — the executable check of the proven §4c splice +
// the per-op refcount delta. Queue 200 → 201 → 202 on notification 100.
#[test]
fn remove_waiter_unlink() {
    let n = ObjId(100);
    let mk = || -> ArrayStore {
        let mut st = ArrayStore::new(0);
        st.refs.insert(100, 4); // a binding ref + three queued waiters
        st.notifs.insert(
            100,
            NotifState { word: 0, wait_head: Some(ObjId(200)), wait_tail: Some(ObjId(202)) },
        );
        for (id, nxt) in [(200u64, Some(ObjId(201))), (201, Some(ObjId(202))), (202, None)] {
            st.tcbs.insert(
                id,
                TcbState {
                    state: ThreadState::BlockedNotif,
                    wait_notif: Some(n),
                    qnext: nxt,
                    ..tcb_state_default()
                },
            );
        }
        st
    };

    // Middle: 201 unlinked; 200 re-threads to 202; head/tail unchanged.
    let mut st = mk();
    check_remove_waiter(&mut st, n, ObjId(201), true);
    assert!(st.notifs[&100].wait_head == Some(ObjId(200)), "head unchanged");
    assert!(st.notifs[&100].wait_tail == Some(ObjId(202)), "tail unchanged");
    assert!(st.tcbs[&200].qnext == Some(ObjId(202)), "predecessor re-threaded past 201");

    // Head: 200 unlinked; 201 becomes the head.
    let mut st = mk();
    check_remove_waiter(&mut st, n, ObjId(200), true);
    assert!(st.notifs[&100].wait_head == Some(ObjId(201)), "201 is the new head");
    assert!(st.notifs[&100].wait_tail == Some(ObjId(202)), "tail unchanged");

    // Tail: 202 unlinked; tail drops to 201, whose qnext becomes None.
    let mut st = mk();
    check_remove_waiter(&mut st, n, ObjId(202), true);
    assert!(st.notifs[&100].wait_head == Some(ObjId(200)), "head unchanged");
    assert!(st.notifs[&100].wait_tail == Some(ObjId(201)), "tail dropped to 201");
    assert!(st.tcbs[&201].qnext.is_none(), "new tail's qnext cleared");

    // Absent: a TCB not on the queue — store unchanged.
    let mut st = mk();
    st.tcbs.insert(300, TcbState { state: ThreadState::Inactive, ..tcb_state_default() });
    let before = st.notifs[&100].clone();
    check_remove_waiter(&mut st, n, ObjId(300), false);
    assert!(st.notifs[&100] == before, "absent removal leaves the queue untouched");

    // Single-element queue: removing the sole waiter empties head and tail.
    let mut st = ArrayStore::new(0);
    st.refs.insert(100, 2);
    st.notifs.insert(
        100,
        NotifState { word: 0, wait_head: Some(ObjId(200)), wait_tail: Some(ObjId(200)) },
    );
    st.tcbs.insert(
        200,
        TcbState {
            state: ThreadState::BlockedNotif,
            wait_notif: Some(n),
            qnext: None,
            ..tcb_state_default()
        },
    );
    check_remove_waiter(&mut st, n, ObjId(200), true);
    assert!(
        st.notifs[&100].wait_head.is_none() && st.notifs[&100].wait_tail.is_none(),
        "queue emptied"
    );
}

#[test]
fn binding_notif_wf_exec_has_teeth() {
    // `binding_notif_wf_exec` mirrors the §4b named invariant the channel ops carry;
    // it is only meaningful if it rejects bindings naming bad notifications.
    let ch = ObjId(7);
    assert!(binding_notif_wf_exec(&signal_fixture(false).0, ch),
        "all-unbound bindings are vacuously well-formed");

    // A binding naming the live, well-formed notification 100 ⇒ accepted.
    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().bindings
        .insert((0, EV_READABLE), Binding { notif: Some(ObjId(100)), bits: 1 });
    assert!(binding_notif_wf_exec(&st, ch), "a binding to a live wf notification is accepted");

    // A binding naming a non-resident notification ⇒ rejected.
    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().bindings
        .insert((0, EV_READABLE), Binding { notif: Some(ObjId(999)), bits: 1 });
    assert!(!binding_notif_wf_exec(&st, ch), "a binding to a non-resident notification is rejected");

    // A binding naming a malformed-queue notification (head/tail disagree) ⇒ rejected.
    let mut st = signal_fixture(false).0;
    st.notifs.insert(100, NotifState { word: 0, wait_head: Some(ObjId(200)), wait_tail: None });
    st.chans.get_mut(&7).unwrap().bindings
        .insert((0, EV_READABLE), Binding { notif: Some(ObjId(100)), bits: 1 });
    assert!(!binding_notif_wf_exec(&st, ch), "a binding to a malformed notification is rejected");
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

// ── Timer (plan §4e) ────────────────────────────────────────────────────────

// `timer_wf_exec` rejects each malformed armed list (so the timer-op `timer_wf`
// precondition is non-vacuous): a head pointing at a non-timer, an unarmed node on the
// chain, a node armed without a bound notification, a `next` cycle, and an armed timer
// absent from the chain (the completeness violation).
#[test]
fn timer_wf_exec_has_teeth() {
    let armed = |notif: Option<ObjId>, next: Option<ObjId>| TimerState {
        armed: true, deadline: 0, notif, bits: 0, next,
    };
    // A well-formed singleton list passes.
    let mut ok = ArrayStore::new(0);
    ok.timers.insert(300, armed(Some(ObjId(100)), None));
    ok.timer_armed_head = Some(ObjId(300));
    assert!(timer_wf_exec(&ok), "a well-formed armed list is accepted");

    // Head points at an unregistered timer.
    let mut dangling = ArrayStore::new(0);
    dangling.timer_armed_head = Some(ObjId(999));
    assert!(!timer_wf_exec(&dangling), "head names a non-timer");

    // A node on the chain is not armed.
    let mut unarmed = ArrayStore::new(0);
    unarmed.timers.insert(300, TimerState { armed: false, deadline: 0, notif: Some(ObjId(100)), bits: 0, next: None });
    unarmed.timer_armed_head = Some(ObjId(300));
    assert!(!timer_wf_exec(&unarmed), "a charted node must be armed");

    // A charted node has no bound notification.
    let mut no_notif = ArrayStore::new(0);
    no_notif.timers.insert(300, armed(None, None));
    no_notif.timer_armed_head = Some(ObjId(300));
    assert!(!timer_wf_exec(&no_notif), "a charted node must name a notification");

    // A `next` cycle (300 → 301 → 300).
    let mut cyclic = ArrayStore::new(0);
    cyclic.timers.insert(300, armed(Some(ObjId(100)), Some(ObjId(301))));
    cyclic.timers.insert(301, armed(Some(ObjId(100)), Some(ObjId(300))));
    cyclic.timer_armed_head = Some(ObjId(300));
    assert!(!timer_wf_exec(&cyclic), "a cycle is rejected");

    // An armed timer is not on the chain (completeness violation).
    let mut incomplete = ArrayStore::new(0);
    incomplete.timers.insert(300, armed(Some(ObjId(100)), None));
    incomplete.timers.insert(301, armed(Some(ObjId(101)), None)); // armed but unlinked
    incomplete.timer_armed_head = Some(ObjId(300));
    assert!(!timer_wf_exec(&incomplete), "an off-chain armed timer is rejected");
}

// `arm`/`disarm` lifecycle: install, same-notif re-arm (net-zero refs), different-notif
// re-arm (old -1, new +1), disarm (release + off the list), and an idempotent disarm of an
// already-disarmed timer — the per-op armed-timer refcount deltas end to end.
#[test]
fn arm_disarm_lifecycle() {
    let mut st = ArrayStore::new(0);
    let t = ObjId(300);
    let n1 = ObjId(100);
    let n2 = ObjId(101);
    st.timers.insert(300, TimerState { armed: false, deadline: 0, notif: None, bits: 0, next: None });
    st.refs.insert(100, 1);
    st.refs.insert(101, 1);

    check_arm(&mut st, t, n1, 0b1, 50);
    assert_eq!(st.refs[&100], 2, "arm +1 on n1");

    check_arm(&mut st, t, n1, 0b10, 60); // same-notif re-arm
    assert_eq!(st.refs[&100], 2, "same-notif re-arm is net-zero");

    check_arm(&mut st, t, n2, 0b100, 70); // different-notif re-arm
    assert_eq!(st.refs[&100], 1, "re-arm released n1");
    assert_eq!(st.refs[&101], 2, "re-arm acquired n2");

    check_disarm(&mut st, t);
    assert_eq!(st.refs[&101], 1, "disarm released n2");
    assert!(st.timer_armed_head.is_none(), "the armed list is empty");

    check_disarm(&mut st, t); // idempotent no-op
    assert_eq!(st.refs[&101], 1, "a second disarm touches no ref");
}

// `check_expired`: a sweep over `300 → 301` where 300 (deadline 50) is expired and binds a
// notification with a blocked waiter, and 301 (deadline 200) is not. The expired timer is
// disarmed and its waiter woken (timer ref released by `disarm`, waiter ref by the wake);
// the unexpired timer survives, now the list head.
#[test]
fn check_expired_wake_and_skip() {
    let now = 100u64;
    let mut st = ArrayStore::new(0);
    st.timers.insert(301, TimerState { armed: true, deadline: 200, notif: Some(ObjId(101)), bits: 0b10, next: None });
    st.timers.insert(300, TimerState { armed: true, deadline: 50, notif: Some(ObjId(100)), bits: 0b1, next: Some(ObjId(301)) });
    st.timer_armed_head = Some(ObjId(300));
    // notif 100 with a blocked waiter 400 (so the fire takes the wake path).
    st.tcbs.insert(400, TcbState { state: ThreadState::BlockedNotif, wait_notif: Some(ObjId(100)), ..tcb_state_default() });
    st.notifs.insert(100, NotifState { word: 0, wait_head: Some(ObjId(400)), wait_tail: Some(ObjId(400)) });
    st.refs.insert(100, 2); // the timer's ref + the waiter's ref
    // notif 101, no waiter.
    st.notifs.insert(101, NotifState { word: 0, wait_head: None, wait_tail: None });
    st.refs.insert(101, 1); // the timer's ref

    check_check_expired(&mut st, now);

    assert!(!st.timers[&300].armed, "the expired timer is disarmed");
    assert_eq!(st.tcbs[&400].state, ThreadState::Runnable, "its blocked waiter woke");
    assert_eq!(st.tcbs[&400].retval, 0b1, "the timer's bits were delivered");
    assert_eq!(st.refs[&100], 0, "disarm released the timer ref, the wake the waiter ref");
    assert!(st.timers[&301].armed, "the unexpired timer survives");
    assert!(st.timer_armed_head == Some(ObjId(301)), "the survivor is now the list head");
    assert_eq!(st.refs[&101], 1, "the untouched notif keeps its ref");
}

// `destroy_timer` of an armed timer (its last cap gone): `disarm`, releasing the notif ref
// and emptying the armed list.
#[test]
fn destroy_timer_disarms() {
    let mut st = ArrayStore::new(0);
    let t = ObjId(300);
    st.timers.insert(300, TimerState { armed: true, deadline: 10, notif: Some(ObjId(100)), bits: 0b1, next: None });
    st.timer_armed_head = Some(t);
    st.refs.insert(100, 1);
    st.refs.insert(300, 0); // last cap gone (the destroy_timer precondition)

    check_destroy_timer(&mut st, t);

    assert!(st.timer_armed_head.is_none(), "the armed list is now empty");
    assert_eq!(st.refs[&100], 0, "destroy_timer released the notif ref via disarm");
}

// `destroy_tcb`'s structural contract: a Runnable thread (200) holding two notification
// bind caps is halted, its queue link cleared, both bind slots emptied, and its report
// left untouched (destruction fires none). Bind-cap refs start at 2 so the deletes just
// decrement (no object teardown — the cross-object recursion is the deferred residue).
#[test]
fn destroy_tcb_structural() {
    let mut st = ArrayStore::new(2);
    st.slots[0] = detached(notif_cap(50));
    st.slots[1] = detached(notif_cap(51));
    st.refs.insert(50, 2);
    st.refs.insert(51, 2);
    let t = ObjId(200);
    st.tcbs.insert(
        200,
        TcbState {
            state: ThreadState::Runnable,
            report: Report::Running,
            bind_slots: [SlotId(0), SlotId(1)],
            ..tcb_state_default()
        },
    );

    check_destroy_tcb(&mut st, t);

    assert_eq!(st.refs[&50], 1, "EXIT bind cap deleted (ref decremented)");
    assert_eq!(st.refs[&51], 1, "FAULT bind cap deleted (ref decremented)");
}

// ── aspace `map_in` (plan §5d): the verified two-pass walk-allocate over arrays ──
//
// `map_in` is generic over `Store`; these checks run the **real** body against
// hand-built `[u64; 512]` / `Vec<[u64; 512]>` page tables, using `ArrayStore`
// purely as the `barrier_after_map` supplier. The post-map state is checked via
// the verified read-only walker (`range_mapped_in`/`lookup`) — the executable
// counterpart of the `pt_lookup` round-trip.

use crate::aspace::{lookup, map_in, pte_encode, range_mapped_in, MapError, PAGE, PERM_W, USER_VA_BASE, USER_VA_END};

// A fresh aspace: a zeroed L1, an `npools`-table zeroed pool at a page-aligned
// base, `pool_used == 0`. `pool_base` sits well inside the 48-bit address field.
fn map_fixture(npools: usize) -> ([u64; 512], Vec<[u64; 512]>, u64, u64) {
    ([0u64; 512], vec![[0u64; 512]; npools], 0u64, 0x4900_0000u64)
}

#[test]
fn map_in_single_page() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE, 0x4800_0000u64);
    map_in(&mut l1, &mut pool, &mut used, base, pa, va, 1, PERM_W, &mut store).unwrap();
    assert!(range_mapped_in(&l1, &pool, base, va, PAGE, true), "mapped writable");
    let (l3, e) = lookup(&l1, &pool, base, va).expect("present");
    assert_eq!(pool[l3][e], pte_encode(pa, PERM_W), "leaf is pte_encode(pa, W)");
    assert_eq!(used, 2, "one L2 + one L3 table allocated");
}

#[test]
fn map_in_multi_page() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE, 0x4800_0000u64);
    map_in(&mut l1, &mut pool, &mut used, base, pa, va, 4, PERM_W, &mut store).unwrap();
    assert!(range_mapped_in(&l1, &pool, base, va, 4 * PAGE, true));
    for i in 0..4u64 {
        let (l3, e) = lookup(&l1, &pool, base, va + i * PAGE).expect("present");
        assert_eq!(pool[l3][e], pte_encode(pa + i * PAGE, PERM_W), "page {i}");
    }
    assert_eq!(used, 2, "4 pages share one L3 table");
}

#[test]
fn map_in_carries_l2_index() {
    // A 2-page range straddling a 2 MiB L2 boundary forces a *second* L3 table.
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE + 511 * PAGE, 0x4800_0000u64);
    map_in(&mut l1, &mut pool, &mut used, base, pa, va, 2, PERM_W, &mut store).unwrap();
    assert!(range_mapped_in(&l1, &pool, base, va, 2 * PAGE, true));
    assert_eq!(used, 3, "one L2 + two L3 tables (the L2 carry)");
}

#[test]
fn map_in_already_mapped_atomic() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let va = USER_VA_BASE;
    let pa1 = 0x4800_0000u64;
    map_in(&mut l1, &mut pool, &mut used, base, pa1, va, 4, PERM_W, &mut store).unwrap();
    // Try to map pages 2..6 with a *different* PA: page 2 overlaps → AlreadyMapped.
    let pa2 = 0x4A00_0000u64;
    let r = map_in(&mut l1, &mut pool, &mut used, base, pa2, va + 2 * PAGE, 4, PERM_W, &mut store);
    assert_eq!(r, Err(MapError::AlreadyMapped));
    // Atomic: no leaf of the second request was written — pages 4/5 stay unmapped…
    assert!(!range_mapped_in(&l1, &pool, base, va + 4 * PAGE, 2 * PAGE, false), "no partial write");
    // …and the overlapped page keeps pa1's PTE (not overwritten with pa2's).
    let (l3, e) = lookup(&l1, &pool, base, va + 2 * PAGE).expect("present");
    assert_eq!(pool[l3][e], pte_encode(pa1 + 2 * PAGE, PERM_W), "original mapping intact");
}

#[test]
fn map_in_need_memory() {
    // A pool one table short of the L2+L3 a single page needs → NeedMemory.
    let (mut l1, mut pool, mut used, base) = map_fixture(1);
    let mut store = ArrayStore::new(0);
    let r = map_in(&mut l1, &mut pool, &mut used, base, 0x4800_0000, USER_VA_BASE, 1, PERM_W, &mut store);
    assert_eq!(r, Err(MapError::NeedMemory));
    assert!(!range_mapped_in(&l1, &pool, base, USER_VA_BASE, PAGE, false), "nothing mapped");
}

#[test]
fn map_in_readonly_rejects_write() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let va = USER_VA_BASE;
    map_in(&mut l1, &mut pool, &mut used, base, 0x4800_0000, va, 1, 0 /* RO */, &mut store).unwrap();
    assert!(range_mapped_in(&l1, &pool, base, va, PAGE, false), "present for reads");
    assert!(!range_mapped_in(&l1, &pool, base, va, PAGE, true), "rejected for writes");
}

#[test]
fn randomized_map_sweep() {
    // For many seeds, map a handful of disjoint ascending ranges into a fresh
    // pool, asserting after EACH map: the new range round-trips and **every prior
    // range still holds its exact PTEs** (the no-clobber frame, at scale).
    let mut trials = 0usize;
    for seed in 0..200u64 {
        let (mut l1, mut pool, mut used, base) = map_fixture(64);
        let mut store = ArrayStore::new(0);
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9).wrapping_add(1));
        let mut mapped: Vec<(u64, u64, u64, u64)> = Vec::new(); // (va, pages, pa, perms)
        let mut next_va = USER_VA_BASE;
        for _ in 0..4 {
            let gap = (rng.below(8) as u64) * PAGE;
            let pages = 1 + (rng.below(4) as u64);
            let va = next_va + gap;
            let pa = 0x4800_0000u64 + (rng.below(64) as u64) * PAGE;
            let perms = if rng.below(2) == 0 { PERM_W } else { 0 };
            if va + pages * PAGE > USER_VA_END {
                break;
            }
            match map_in(&mut l1, &mut pool, &mut used, base, pa, va, pages, perms, &mut store) {
                Ok(()) => {
                    assert!(range_mapped_in(&l1, &pool, base, va, pages * PAGE, perms & PERM_W != 0));
                    for i in 0..pages {
                        let (l3, e) = lookup(&l1, &pool, base, va + i * PAGE).expect("new range present");
                        assert_eq!(pool[l3][e], pte_encode(pa + i * PAGE, perms));
                    }
                    for &(mva, mpages, mpa, mperms) in &mapped {
                        for i in 0..mpages {
                            let (l3, e) = lookup(&l1, &pool, base, mva + i * PAGE).expect("prior range intact");
                            assert_eq!(pool[l3][e], pte_encode(mpa + i * PAGE, mperms), "no clobber");
                        }
                    }
                    mapped.push((va, pages, pa, perms));
                    next_va = va + pages * PAGE;
                    trials += 1;
                }
                Err(_) => break,
            }
        }
    }
    assert!(trials > 300, "sweep should map hundreds of ranges, ran {trials}");
}
