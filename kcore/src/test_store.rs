//! Concrete array-backed `Store` + the executable contract checks for the
//! cspace ops (`delete`, `cdt_unlink`, `slot_move`).
//!
//! Those three ops carry proven Verus contracts (their bodies are in-place
//! linked-list-splice walks). This module is the *executable counterpart* of
//! that proof: a plain-array `Store` over which the **real** op bodies run, with
//! hand-built and randomly-generated CDT shapes, asserting every clause of each
//! op's `ensures` — including the `cspace_wf` clauses
//! (`siblings_share_parent`/`parent_has_first_child`/`sib_acyclic`). If a body
//! ever violated its contract the assertion would fire; the contract is thus
//! continuously checked against the body in CI (`cargo test -p kcore`).
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
    cdt_unlink, delete, derive, destroy_cspace, map_frame, obj_unref, revoke, slot_move,
    unref_aspace, unref_cspace, Cap, CapKind, CapSlot, ChanEnd, Rights,
};
use crate::id::{ObjId, SlotId};
use crate::notification::{destroy_notif, remove_waiter, signal, wait};
use crate::store::{Binding, Store};
use crate::thread::{
    bind as thread_bind, destroy_tcb, report_terminal, Report, ThreadState, BIND_EXIT, BIND_FAULT,
};
use crate::timer::{arm, check_expired, destroy_timer, disarm};
use crate::untyped::{reset, retype_check, retype_install, ObjType, RetypeError};
use std::collections::{BTreeMap, VecDeque};

// ── The concrete store ────────────────────────────────────────────────────
//
// Slots are a `Vec<CapSlot>` (a `SlotId` is its index); object refcounts and
// cspace resident lists are keyed maps. The CDT/teardown path needs only the
// Frame/Untyped/CSpace/Aspace accessors.
//
// The **real channel state** (`chans`) — the `chan_*` accessors model the
// `ChanView` ghost view: per-end cap counts, per-ring FIFO cursors, event
// bindings, per-message lengths, and the ring cap-slot *handles* (the cap
// contents stay in `slots`, the single arena). It also carries the minimal
// notification + TCB state `notification::signal` touches, so `signal`'s frame
// contract (`slot_view`/`chan_view` unchanged) can be checked against the real
// body (`check_signal_frame`). The thread/timer seam `signal` never reaches
// stays `unimplemented!()` — a stray call panics loudly.

#[derive(Clone, PartialEq)]
struct ChanState {
    depth: u32,
    end_caps: [u32; 2],
    head: [u32; 2],
    count: [u32; 2],
    bindings: BTreeMap<(usize, usize), Binding>, // (end, ev)
    msg_len: BTreeMap<(usize, u32), u16>,        // (ring, index)
    ring_cap: BTreeMap<(usize, u32, usize), SlotId>, // (ring, index, cap) -> arena handle
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
    priority: u8,
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

#[derive(Clone)]
struct ArrayStore {
    slots: Vec<CapSlot>,
    refs: BTreeMap<u64, u32>,
    cspaces: BTreeMap<u64, Vec<SlotId>>,
    chans: BTreeMap<u64, ChanState>,
    notifs: BTreeMap<u64, NotifState>,
    tcbs: BTreeMap<u64, TcbState>,
    timers: BTreeMap<u64, TimerState>,
    timer_armed_head: Option<ObjId>,
    // The 32-level ready queue: per-level head/tail + a presence bitmap — the
    // executable counterpart of the `ready_view` ghost view (the `READY`/`READY_BITMAP`
    // kernel statics). `ready_enqueue`/`dequeue`/`unqueue`/`top_ready` run against these.
    ready_heads: Vec<Option<ObjId>>, // len NUM_PRIOS
    ready_tails: Vec<Option<ObjId>>, // len NUM_PRIOS
    ready_bitmap: u32,
    // The TLBI effect log: the executable counterpart of the `tlb_log_view`
    // ghost view. `tlb_invalidate_page` appends `(asid, va)`, so `check_unmap`
    // can assert `unmap_in` issues one TLBI per cleared page, in order (the
    // ordering theorem checked against the real body).
    tlb_log: Vec<(u16, u64)>,
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
            ready_heads: vec![None; crate::sysabi::NUM_PRIOS],
            ready_tails: vec![None; crate::sysabi::NUM_PRIOS],
            ready_bitmap: 0,
            tlb_log: Vec::new(),
        }
    }
    fn n(&self) -> usize {
        self.slots.len()
    }
    fn at(&self, s: SlotId) -> CapSlot {
        self.slots[s.0 as usize]
    }
    fn chan(&self, ch: ObjId) -> &ChanState {
        self.chans
            .get(&ch.0)
            .expect("chan_*: channel not registered in this test store")
    }
    fn chan_mut(&mut self, ch: ObjId) -> &mut ChanState {
        self.chans
            .get_mut(&ch.0)
            .expect("set_chan_*: channel not registered in this test store")
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
        *self
            .refs
            .get(&o.0)
            .expect("obj_refs: object not registered in this test store")
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
    // Last-reference teardown: drop `a` from the refcount map, matching
    // the `ExStore` contract `refs_view() == old.refs_view().remove(a)` (the real
    // kernel frees the aspace object here). `aspace_unmap` is page-table maintenance
    // with no object state, so the no-op faithfully "frames every view".
    fn aspace_destroy(&mut self, a: ObjId) {
        self.refs.remove(&a.0);
    }
    fn aspace_unmap(&mut self, _a: ObjId, _va: u64, _pages: u64) {}
    // The map-time twin of `aspace_unmap`: page-table machinery with no kcore object state, so
    // the no-op faithfully "frames every view". Always succeeds here (the real kernel can fail
    // with `NeedMemory`; `map_frame`'s `Err` arm — store unchanged — is verified, not exercised).
    fn aspace_map(
        &mut self,
        _a: ObjId,
        _pa: u64,
        _va: u64,
        _pages: u64,
        _perms: u64,
    ) -> Result<(), crate::aspace::MapError> {
        Ok(())
    }

    // ── channel state: the `chan_*` accessors backed by `chans` ──
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
        self.chan(ch)
            .bindings
            .get(&(end, ev))
            .copied()
            .unwrap_or(Binding::UNBOUND)
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
    // no-ops on the modelled state (the frame: `chan_view` unchanged).
    fn chan_msg_write(&mut self, _: ObjId, _: usize, _: u32, _: &[u8]) {}
    fn chan_msg_read(&self, _: ObjId, _: usize, _: u32, _: usize, _: &mut [u8]) {}

    // ── notification + TCB state `signal` touches ───────────────────────────
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
    fn tcb_priority(&self, t: ObjId) -> u8 {
        self.tcbs[&t.0].priority
    }
    fn set_tcb_priority(&mut self, t: ObjId, p: u8) {
        self.tcbs.get_mut(&t.0).unwrap().priority = p;
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
    // B8C: `make_runnable`/`unqueue_ready` are now **faithful** — they route through the verified
    // `ready_enqueue`/`ready_unqueue` ops so the host model realizes the seam contracts Verus
    // assumes (the ready-queue linkage now lives in `ready_view`, not below the abstract view).
    // `make_runnable` flips the thread Runnable then enqueues it to its priority level's tail;
    // `unqueue_ready` splices it back out. (The kernel's real realizations are B8C-3.)
    fn make_runnable(&mut self, t: ObjId) {
        self.tcbs.get_mut(&t.0).unwrap().state = ThreadState::Runnable;
        crate::ready::ready_enqueue(self, t);
    }
    fn unqueue_ready(&mut self, t: ObjId) {
        crate::ready::ready_unqueue(self, t);
    }
    // The real `dsb`/`isb` + TLBI is the kernel shell's job; here the barriers are
    // no-ops and `tlb_invalidate_page` records the `(asid, va)` log so `check_unmap`
    // can assert the TLBI ordering theorem against the real `unmap_in` body.
    fn tlb_invalidate_page(&mut self, asid: u16, va: u64) {
        self.tlb_log.push((asid, va));
    }
    fn barrier_after_map(&mut self) {}
    fn barrier_after_unmap(&mut self) {}
    fn timer_armed_head(&self) -> Option<ObjId> {
        self.timer_armed_head
    }
    fn set_timer_armed_head(&mut self, h: Option<ObjId>) {
        self.timer_armed_head = h;
    }
    fn ready_head(&self, level: usize) -> Option<ObjId> {
        self.ready_heads[level]
    }
    fn set_ready_head(&mut self, level: usize, h: Option<ObjId>) {
        self.ready_heads[level] = h;
    }
    fn ready_tail(&self, level: usize) -> Option<ObjId> {
        self.ready_tails[level]
    }
    fn set_ready_tail(&mut self, level: usize, t: Option<ObjId>) {
        self.ready_tails[level] = t;
    }
    fn ready_bitmap(&self) -> u32 {
        self.ready_bitmap
    }
    fn set_ready_bitmap(&mut self, b: u32) {
        self.ready_bitmap = b;
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
        if !(in_dom(s.parent) && in_dom(s.first_child) && in_dom(s.next_sib) && in_dom(s.prev_sib))
        {
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

// ── The refcount census mirror ──────────────────────────────────────────────
//
// `refcount_sound_exec` is the executable counterpart of the ghost
// `cspace::refcount_sound`: it recomputes every object's `obj_census` over the
// concrete store and checks it equals the stored `refs`. The teardown contracts
// (`delete`/`destroy_channel`/`destroy_tcb`) require and preserve
// `refcount_sound`, so this mirror host-checks that clause
// (`refcount_sound_exec_has_teeth` proves it is not vacuous).

// Exec mirror of the spec `cap_obj` (the object a cap designates, if any).
fn cap_obj_exec(cap: Cap) -> Option<ObjId> {
    match cap.kind {
        CapKind::Aspace(o)
        | CapKind::CSpace(o)
        | CapKind::Thread(o, _)
        | CapKind::Channel(o, _)
        | CapKind::Notification(o)
        | CapKind::Timer(o) => Some(o),
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
    }
}

// Exec mirror of the spec `cap_frame_aspace` (the aspace a mapped frame holds).
fn cap_frame_aspace_exec(cap: Cap) -> Option<ObjId> {
    match cap.kind {
        CapKind::Frame {
            mapping: Some((a, _)),
            ..
        } => Some(a),
        _ => None,
    }
}

// Exec mirror of `waiter_seq(o).len()`: `o`'s FIFO waiter-chain length, walked from
// `wait_head` via `qnext` (the `notif_wf_exec` walk). 0 for a non-notification `o`
// (absent from `notifs`); the bounded guard mirrors the chain's acyclicity.
fn waiter_count_exec(st: &ArrayStore, o: ObjId) -> u32 {
    let mut count: u32 = 0;
    if let Some(nst) = st.notifs.get(&o.0) {
        let mut cur = nst.wait_head;
        let mut guard = st.tcbs.len() + 1;
        while let Some(t) = cur {
            count += 1;
            if guard == 0 {
                break;
            }
            guard -= 1;
            cur = st.tcbs.get(&t.0).and_then(|tc| tc.qnext);
        }
    }
    count
}

// Exec mirror of `obj_census(o)`: the six census terms summed.
fn obj_census_exec(st: &ArrayStore, o: ObjId) -> u32 {
    let mut total: u32 = 0;
    // slot_refs + frame_map_refs over the single arena.
    for s in &st.slots {
        if cap_obj_exec(s.cap) == Some(o) {
            total += 1;
        }
        if cap_frame_aspace_exec(s.cap) == Some(o) {
            total += 1;
        }
    }
    // binding_refs: the (end, ev) ∈ {0,1}×{0,1,2} triples naming `o`.
    for ch in st.chans.values() {
        for e in 0..2usize {
            for v in 0..3usize {
                if ch.bindings.get(&(e, v)).and_then(|b| b.notif) == Some(o) {
                    total += 1;
                }
            }
        }
    }
    // waiter_refs.
    total += waiter_count_exec(st, o);
    // armed_timer_refs.
    for tm in st.timers.values() {
        if tm.armed && tm.notif == Some(o) {
            total += 1;
        }
    }
    // thread_hold_refs: cspace + aspace holds.
    for tc in st.tcbs.values() {
        if tc.cspace == Some(o) {
            total += 1;
        }
        if tc.aspace == Some(o) {
            total += 1;
        }
    }
    total
}

// Every live object's stored refcount equals its census (`cspace::refcount_sound`).
fn refcount_sound_exec(st: &ArrayStore) -> bool {
    st.refs
        .iter()
        .all(|(&o, &r)| obj_census_exec(st, ObjId(o)) == r)
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
    // ring-cap injectivity: distinct positions, distinct handles.
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

// The exec mirror of `timer_wf`: the armed list from `timer_armed_head`,
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

// ── The ready-queue mirrors (B8C) ───────────────────────────────────────────
//
// Executable counterparts of `cspace::ready_seq`/`ready_wf`/`ready_complete` (ghost, so
// erased and uncallable from test code). The verified `ready_enqueue`/`ready_dequeue`/
// `ready_unqueue`/`top_ready` ops run against the `ArrayStore` ready backing; these mirrors
// let the host tests assert the invariant those ops preserve, with teeth
// (`ready_wf_exec_has_teeth`/`ready_complete_exec_has_teeth`).

// Walk `level`'s ready chain from the head through `qnext`, bounded so a malformed cyclic
// chain can't loop forever (the surplus node makes the duplicate visible to the dup check).
fn ready_seq_exec(st: &ArrayStore, level: usize) -> Vec<ObjId> {
    let mut out = Vec::new();
    let mut cur = st.ready_heads[level];
    while let Some(t) = cur {
        out.push(t);
        if out.len() > st.tcbs.len() + 1 {
            break; // a cycle (walk longer than the TCB count) — caught by ready_wf_exec
        }
        cur = st.tcbs.get(&t.0).and_then(|tc| tc.qnext);
    }
    out
}

// `ready_seq_exec` as raw `u64` ids — `ObjId` has no `Debug`, so sequence assertions compare
// the tags (the `wait_signal_fifo` idiom of reading handles through `.0`).
fn ready_ids(st: &ArrayStore, level: usize) -> Vec<u64> {
    ready_seq_exec(st, level).iter().map(|x| x.0).collect()
}

// The exec mirror of `cspace::ready_wf` ∧ `ready_bitmap_coherent`: across all 32 levels,
// head-None iff tail-None; the presence bit is set iff the level is non-empty; the chain is
// duplicate-free (acyclic) with `head`/`tail` its first/last node; and every charted node is
// a resident, Runnable TCB at that level threaded by `qnext` (`None` at the tail). The
// structural ready-queue invariant the ops preserve — note it does NOT fold in
// `ready_complete` (that is a separate predicate, since `ready_unqueue` preserves only
// `ready_complete_except`).
fn ready_wf_exec(st: &ArrayStore) -> bool {
    for level in 0..crate::sysabi::NUM_PRIOS {
        let head = st.ready_heads[level];
        let tail = st.ready_tails[level];
        let bit = st.ready_bitmap & (1u32 << level) != 0;
        if head.is_none() != tail.is_none() {
            return false; // head/tail None agreement
        }
        let seq = ready_seq_exec(st, level);
        if bit != !seq.is_empty() {
            return false; // bitmap coherence: bit set iff level non-empty
        }
        if seq.is_empty() {
            continue;
        }
        // duplicate-free (acyclic).
        for i in 0..seq.len() {
            for j in (i + 1)..seq.len() {
                if seq[i].0 == seq[j].0 {
                    return false;
                }
            }
        }
        // head/tail are the chain's first/last node.
        if head != Some(seq[0]) || tail != Some(seq[seq.len() - 1]) {
            return false;
        }
        // per-node covenant: resident, Runnable, at `level`, qnext threads to the next.
        for (i, node) in seq.iter().enumerate() {
            let tc = match st.tcbs.get(&node.0) {
                Some(tc) => tc,
                None => return false, // a charted node is not a live TCB
            };
            if tc.state != ThreadState::Runnable || tc.priority as usize != level {
                return false;
            }
            let expect_next = if i + 1 < seq.len() {
                Some(seq[i + 1])
            } else {
                None
            };
            if tc.qnext != expect_next {
                return false;
            }
        }
    }
    true
}

// The exec mirror of `cspace::ready_complete`: every Runnable thread is charted on its
// priority level's ready chain (and that priority is in range). The completeness discipline
// `ready_unqueue`'s splice walk and `top_ready`'s pick rely on — the ready-queue analogue of
// `timer_complete`.
fn ready_complete_exec(st: &ArrayStore) -> bool {
    for (id, tc) in st.tcbs.iter() {
        if tc.state == ThreadState::Runnable {
            let level = tc.priority as usize;
            if level >= crate::sysabi::NUM_PRIOS {
                return false;
            }
            if !ready_seq_exec(st, level).iter().any(|x| x.0 == *id) {
                return false;
            }
        }
    }
    true
}

// ── The cap→object consistency mirror ────────────────────────────────────────
//
// `caps_consistent_exec` is the executable counterpart of the ghost
// `cspace::caps_consistent`: every live cap's designated object is well-formed (the
// per-kind clauses mirror `cap_consistent`). The teardown contracts
// (`delete`/`destroy_channel`/`destroy_tcb`) require and preserve it, so this mirror
// host-checks that clause against the real `ArrayStore` bodies
// (`caps_consistent_exec_has_teeth` proves it is not vacuous).
fn cap_consistent_exec(st: &ArrayStore, cap: Cap) -> bool {
    match cap.kind {
        CapKind::Channel(o, end) => {
            st.chans.contains_key(&o.0)
                && chan_wf_exec(st, o)
                && st.chan(o).end_caps[end_idx_exec(end)] > 0
                && binding_notif_wf_exec(st, o)
        }
        CapKind::CSpace(o) => {
            st.cspaces.contains_key(&o.0)
                && st.cspaces[&o.0].iter().all(|sid| (sid.0 as usize) < st.n())
        }
        CapKind::Thread(o, _) => {
            st.tcbs.contains_key(&o.0)
                && (st.tcbs[&o.0].bind_slots[0].0 as usize) < st.n()
                && (st.tcbs[&o.0].bind_slots[1].0 as usize) < st.n()
                // The bound cspace is resident-wf — mirrors the CSpace arm's residents-live
                // check for the TCB's `cspace`, when bound.
                && match st.tcbs[&o.0].cspace {
                    Some(cs) => {
                        st.cspaces.contains_key(&cs.0)
                            && st.cspaces[&cs.0].iter().all(|sid| (sid.0 as usize) < st.n())
                    }
                    None => true,
                }
                // Waiter-coherence: a BlockedNotif thread's wait_notif names a notif_wf
                // notification (the precondition `destroy_tcb`'s `remove_waiter` needs).
                && match (st.tcbs[&o.0].state, st.tcbs[&o.0].wait_notif) {
                    (ThreadState::BlockedNotif, Some(wn)) => notif_wf_exec(st, wn),
                    _ => true,
                }
        }
        CapKind::Notification(o) => notif_wf_exec(st, o),
        CapKind::Timer(o) => st.timers.contains_key(&o.0) && timer_wf_exec(st),
        // Empty / Untyped / Frame / Aspace: no destructor-bearing object constraint.
        _ => true,
    }
}

fn caps_consistent_exec(st: &ArrayStore) -> bool {
    (0..st.n()).all(|i| {
        let cap = st.slots[i].cap;
        cap.is_empty() || cap_consistent_exec(st, cap)
    })
}

// Exec mirror of `cspace::end_caps_sound`: every live channel's `end_caps[e]` equals the
// count of `Channel(ch, e)` caps in the arena (the rev1§3.3 per-endpoint census). Host-checks
// that clause against the real `ArrayStore` bodies (`end_caps_sound_exec_has_teeth` proves
// it is not vacuous).
fn end_caps_sound_exec(st: &ArrayStore) -> bool {
    st.chans.iter().all(|(&ch, cs)| {
        (0..2usize).all(|e| {
            let count = st
                .slots
                .iter()
                .filter(|s| {
                    matches!(s.cap.kind, CapKind::Channel(o, end)
                    if o.0 == ch && end_idx_exec(end) == e)
                })
                .count() as u32;
            cs.end_caps[e] == count
        })
    })
}

// ── Shape builders ─────────────────────────────────────────────────────────

fn detached(cap: Cap) -> CapSlot {
    CapSlot {
        cap,
        parent: None,
        first_child: None,
        next_sib: None,
        prev_sib: None,
    }
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
        priority: 0,
        bind_bits: [0, 0],
        bind_slots: [SlotId(0), SlotId(0)],
    }
}
fn frame_cap(base: u64) -> Cap {
    Cap {
        kind: CapKind::Frame {
            base,
            pages: 1,
            mapping: None,
        },
        rights: Rights(0xff),
    }
}
fn cspace_cap(o: u64) -> Cap {
    Cap {
        kind: CapKind::CSpace(ObjId(o)),
        rights: Rights(0xff),
    }
}
fn untyped_cap(base: u64, size: u64, watermark: u64) -> Cap {
    Cap {
        kind: CapKind::Untyped {
            base,
            size,
            watermark,
        },
        rights: Rights(0xff),
    }
}
fn notif_cap(o: u64) -> Cap {
    Cap {
        kind: CapKind::Notification(ObjId(o)),
        rights: Rights(0xff),
    }
}
fn thread_cap(o: u64, max_prio: u8) -> Cap {
    Cap {
        kind: CapKind::Thread(ObjId(o), max_prio),
        rights: Rights(0xff),
    }
}

// The exec mirror of the spec `CapKind::Untyped { base, size, watermark }`
// projection — `Some(geometry)` iff the cap is an untyped, used both to compute
// `retype_check`'s expected `Ok` triple and to read `reset`'s watermark edit.
fn untyped_geom(c: Cap) -> Option<(u64, u64, u64)> {
    match c.kind {
        CapKind::Untyped {
            base,
            size,
            watermark,
        } => Some((base, size, watermark)),
        _ => None,
    }
}

// A Debug+PartialEq snapshot of everything the two ops can observably touch
// (emptiness, the four CDT links, and any untyped geometry). `SlotId`s are
// flattened to `u64` because `SlotId` is not `Debug` (so `assert_eq!` on the
// raw handle would not compile). Used to assert the read-only / single-slot
// frames against the real bodies.
type SlotFp = (
    bool,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<(u64, u64, u64)>,
);
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
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
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
        derive(&mut st, src, dst, 0xff, 0xFF).expect("derive Frame child");
        nonempty.push(dst);
    }
    assert!(
        cspace_wf_exec(&st),
        "generator produced a non-cspace_wf forest"
    );
    st
}

// ── Contract checks (the op `ensures`, asserted against the real bodies) ────

// The `only_empties` frame, executable: every slot empty before is still empty after
// (teardown only clears caps, never installs one). Host-checks that clause on
// `delete`/`destroy_channel`/`destroy_tcb`.
fn assert_only_empties(before: &[CapSlot], st: &ArrayStore, ctx: &str) {
    for i in 0..before.len() {
        if before[i].cap.is_empty() {
            assert!(
                st.slots[i].cap.is_empty(),
                "{ctx}: empty slots stay empty (§6d only_empties)"
            );
        }
    }
}

fn check_delete(st: &mut ArrayStore, slot: SlotId) {
    assert!(cspace_wf_exec(st), "delete pre: cspace_wf");
    assert!(!st.at(slot).cap.is_empty(), "delete pre: slot non-empty");
    let (n0, c0) = (st.n(), count_nonempty_exec(st));
    let resid0 = st.cspaces.clone();
    let empty0 = st.slots.clone();
    // The census clause is conditional on the precondition `refcount_sound`,
    // so only assert it preserved when the fixture satisfied it (most generated
    // forests carry no object caps, so they are vacuously sound).
    let sound0 = refcount_sound_exec(st);
    // The cap→object invariant is likewise a guarded precondition.
    let consistent0 = caps_consistent_exec(st);
    // The endpoint-cap census is also a guarded precondition.
    let end_caps0 = end_caps_sound_exec(st);
    delete(st, slot);
    assert!(cspace_wf_exec(st), "delete post: cspace_wf preserved");
    assert_eq!(st.n(), n0, "delete post: dom preserved");
    assert!(
        st.at(slot).cap.is_empty(),
        "delete post: target slot emptied"
    );
    assert!(
        count_nonempty_exec(st) < c0,
        "delete post: count_nonempty strictly drops"
    );
    // residency is immutable across delete (the frame destroy_cspace's loop reads).
    assert!(
        st.cspaces == resid0,
        "delete post: cspace residency unchanged"
    );
    if sound0 {
        assert!(
            refcount_sound_exec(st),
            "delete post: refcount_sound preserved"
        );
    }
    if consistent0 {
        assert!(
            caps_consistent_exec(st),
            "delete post: caps_consistent preserved (§6d)"
        );
    }
    if end_caps0 {
        assert!(
            end_caps_sound_exec(st),
            "delete post: end_caps_sound preserved (§6d)"
        );
    }
    assert_only_empties(&empty0, st, "delete post");
}

// Assert `unref_aspace`'s contract against the real body. The caller hands an
// **off-by-one** state — `refs[a] == census(a)+1`, sound everywhere else — the state a
// real teardown reaches by clearing the mapping/hold naming `a` *before* the call
// (delete's frame-unmap branch; destroy_tcb's aspace release). Asserts the `-1` /
// last-ref-destroy split and that `refcount_sound` is restored.
fn check_unref_aspace(st: &mut ArrayStore, a: ObjId) {
    let r0 = st.refs[&a.0];
    assert!(r0 > 0, "unref_aspace pre: refs[a] > 0");
    assert_eq!(
        obj_census_exec(st, a) + 1,
        r0,
        "unref_aspace pre: off-by-one census at a"
    );
    for (&o, &r) in &st.refs {
        if o != a.0 {
            assert_eq!(
                obj_census_exec(st, ObjId(o)),
                r,
                "unref_aspace pre: sound at every other object"
            );
        }
    }
    unref_aspace(st, a);
    assert!(
        refcount_sound_exec(st),
        "unref_aspace post: refcount_sound restored"
    );
    if r0 == 1 {
        assert!(
            !st.refs.contains_key(&a.0),
            "unref_aspace post: last ref → aspace_destroy dropped a"
        );
    } else {
        assert_eq!(
            st.refs[&a.0],
            r0 - 1,
            "unref_aspace post: refs decremented, a still live"
        );
    }
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
        s.parent.is_none()
            && s.first_child.is_none()
            && s.next_sib.is_none()
            && s.prev_sib.is_none(),
        "cdt_unlink post: slot fully detached"
    );
    assert_eq!(
        s.cap.is_empty(),
        cap_was_empty,
        "cdt_unlink post: cap untouched"
    );
    assert_eq!(
        count_nonempty_exec(st),
        c0,
        "cdt_unlink post: count_nonempty unchanged"
    );
}

fn check_slot_move(st: &mut ArrayStore, src: SlotId, dst: SlotId) {
    assert!(cspace_wf_exec(st), "slot_move pre: cspace_wf");
    assert!(
        !st.at(src).cap.is_empty() && st.at(dst).cap.is_empty(),
        "slot_move pre: src live, dst empty"
    );
    let (n0, c0) = (st.n(), count_nonempty_exec(st));
    let moved = st.at(src).cap;
    slot_move(st, src, dst);
    assert!(cspace_wf_exec(st), "slot_move post: cspace_wf preserved");
    assert_eq!(st.n(), n0, "slot_move post: dom preserved");
    assert!(st.at(src).cap.is_empty(), "slot_move post: src emptied");
    assert!(
        !st.at(dst).cap.is_empty(),
        "slot_move post: dst now holds the cap"
    );
    assert!(
        matches!(st.at(dst).cap.kind, CapKind::Frame { base, .. } if matches!(moved.kind, CapKind::Frame { base: b, .. } if b == base)),
        "slot_move post: dst inherits src's cap"
    );
    assert_eq!(
        count_nonempty_exec(st),
        c0,
        "slot_move post: count_nonempty unchanged (one owner relocates)"
    );
}

// Re-derive `retype_check`'s spec result from the store state, then assert the
// real body returns exactly that AND left the arena untouched (the read-only
// frame, which holds on every path). Covers the geometry, the error precedence
// (NotUntyped before DestOccupied), and the channel `dst2` validity.
fn check_retype_check(
    st: &mut ArrayStore,
    ut: SlotId,
    ty: ObjType,
    dst: SlotId,
    dst2: Option<SlotId>,
) {
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
    assert_eq!(
        fingerprint(st),
        fp,
        "retype_check post: read-only on every path"
    );
    match (geom, dst_empty, chan_ok) {
        (None, _, _) => assert_eq!(
            res,
            Err(RetypeError::NotUntyped),
            "non-Untyped ut → NotUntyped (precedence)"
        ),
        (Some(g), true, true) => assert_eq!(res, Ok(g), "Ok returns the untyped's geometry"),
        (Some(_), _, _) => assert_eq!(
            res,
            Err(RetypeError::DestOccupied),
            "occupied/aliased/missing dst(2) → DestOccupied"
        ),
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
            assert_eq!(
                res,
                Err(RetypeError::NotUntyped),
                "non-Untyped → NotUntyped"
            );
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
            assert_eq!(
                fingerprint(st),
                expected,
                "reset Ok: only ut's watermark zeroed, all else intact"
            );
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
            CapKind::Untyped {
                base: b1,
                size: s1,
                watermark: w1,
            },
            CapKind::Untyped {
                base: b2,
                size: s2,
                watermark: w2,
            },
        ) => b1 == b2 && s1 == s2 && w1 == w2,
        (
            CapKind::Frame {
                base: b1,
                pages: p1,
                mapping: m1,
            },
            CapKind::Frame {
                base: b2,
                pages: p2,
                mapping: m2,
            },
        ) => b1 == b2 && p1 == p2 && m1 == m2,
        (CapKind::Aspace(o1), CapKind::Aspace(o2)) => o1 == o2,
        (CapKind::CSpace(o1), CapKind::CSpace(o2)) => o1 == o2,
        (CapKind::Thread(o1, mp1), CapKind::Thread(o2, mp2)) => o1 == o2 && mp1 == mp2,
        (CapKind::Channel(o1, e1), CapKind::Channel(o2, e2)) => o1 == o2 && e1 == e2,
        (CapKind::Notification(o1), CapKind::Notification(o2)) => o1 == o2,
        (CapKind::Timer(o1), CapKind::Timer(o2)) => o1 == o2,
        _ => false,
    }
}

// Assert `retype_install`'s contract against the real body: the watermark bump,
// the rev1§2.5 rights-inheritance table (incl. PHYS cleared for a sub-Untyped), the new
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

    assert!(
        cspace_wf_exec(st),
        "retype_install post: cspace_wf preserved"
    );
    // watermark advanced to `end - base`, base/size kept.
    assert_eq!(
        untyped_geom(st.at(ut).cap),
        Some((base, size, end - base)),
        "watermark advanced"
    );
    // `dst` holds the new cap as a CDT child of `ut`.
    assert!(
        cap_kind_eq(st.at(dst).cap.kind, kind),
        "dst holds the carved kind"
    );
    // SlotId is not Debug (see `fingerprint`), so compare with `assert!(==)`.
    assert!(st.at(dst).parent == Some(ut), "dst is a CDT child of ut");
    // rev1§2.5 rights-inheritance table.
    let expect_rights = match ty {
        ObjType::Frame => ut_rights,
        ObjType::Thread => Rights::THREAD_ALL.0,
        ObjType::Untyped => ut_rights & (Rights::READ | Rights::WRITE),
        _ => Rights::ALL.0,
    };
    assert_eq!(
        st.at(dst).cap.rights.0,
        expect_rights,
        "rights-inheritance table"
    );
    if matches!(ty, ObjType::Untyped) {
        assert_eq!(
            st.at(dst).cap.rights.0 & Rights::PHYS,
            0,
            "sub-Untyped never carries PHYS"
        );
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
            assert_eq!(
                st.refs, refs_before,
                "non-channel: refs untouched (init pre-counts dst)"
            );
            assert!(st.chans == chans_before, "non-channel: chan_view untouched");
        }
    }
}

// A well-formed one-deep channel (ObjId 7) + a notification (ObjId 100), the
// fixture for the `signal` frame check. The arena holds the channel's 8
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
        ChanState {
            depth: 1,
            end_caps: [1, 1],
            head: [0, 0],
            count: [1, 0],
            bindings,
            msg_len,
            ring_cap,
        },
    );

    let n = ObjId(100);
    st.refs.insert(100, 1);
    if with_waiter {
        let t = ObjId(200);
        st.tcbs.insert(
            200,
            TcbState {
                state: ThreadState::BlockedNotif,
                wait_notif: Some(n),
                ..tcb_state_default()
            },
        );
        st.notifs.insert(
            100,
            NotifState {
                word: 0,
                wait_head: Some(t),
                wait_tail: Some(t),
            },
        );
    } else {
        st.notifs.insert(
            100,
            NotifState {
                word: 0,
                wait_head: None,
                wait_tail: None,
            },
        );
    }
    (st, n)
}

// Run the real `notification::signal` and assert its frame holds: `slot_view`
// (the `fingerprint` observable) and `chan_view` (`chans`) are both unchanged.
// The executable counterpart of the contract — the check-against-its-body
// discipline.
fn check_signal_frame(st: &mut ArrayStore, n: ObjId, bits: u64) {
    assert!(notif_wf_exec(st, n), "signal pre: notif_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    signal(st, n, bits);
    assert!(fingerprint(st) == fp, "signal post: slot_view unchanged");
    assert!(st.chans == chans, "signal post: chan_view unchanged");
    assert!(notif_wf_exec(st, n), "signal post: notif_wf preserved");
    // B8C-4: signal's wake path enqueues the woken waiter via `make_runnable`
    // (→ `ready_enqueue`), so the ready queue stays well-formed (incl. bitmap coherence).
    assert!(ready_wf_exec(st), "signal post: ready_wf preserved");
}

// Run the real `notification::remove_waiter` and assert its proven frame: the
// `slot_view`/`chan_view` are untouched, `notif_wf` is preserved, and the queued-ref
// release happens iff `t` was on the queue (the host check of the per-op refcount
// delta + the splice). The executable counterpart of the proven contract.
fn check_remove_waiter(st: &mut ArrayStore, n: ObjId, t: ObjId, queued: bool) {
    assert!(notif_wf_exec(st, n), "remove_waiter pre: notif_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    let refs0 = st.refs[&n.0];
    remove_waiter(st, n, t);
    assert!(
        fingerprint(st) == fp,
        "remove_waiter post: slot_view unchanged"
    );
    assert!(st.chans == chans, "remove_waiter post: chan_view unchanged");
    assert!(
        notif_wf_exec(st, n),
        "remove_waiter post: notif_wf preserved"
    );
    if queued {
        assert!(
            st.tcbs[&t.0].qnext.is_none(),
            "removed waiter's qnext cleared"
        );
        assert!(
            st.tcbs[&t.0].wait_notif.is_none(),
            "removed waiter's wait_notif cleared"
        );
        assert_eq!(st.refs[&n.0], refs0 - 1, "queued ref released");
    } else {
        assert_eq!(st.refs[&n.0], refs0, "absent removal touches no ref");
    }
}

// The exec mirror of `cspace::binding_notif_wf`: every bound endpoint event of `ch`
// names a notification that is resident and `notif_wf`. The plain-Rust re-expression
// of the named binding invariant (the `notif_wf_exec` discipline).
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

// Run the real `endpoint_cap_dropped` and assert its contract against the
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

    assert!(
        fingerprint(st) == fp,
        "endpoint_cap_dropped: slot_view unchanged"
    );
    assert!(
        *st.chan(ch) == expect_chan,
        "endpoint_cap_dropped: only end_caps[end] decremented"
    );
    if before != 1 {
        assert!(
            st.refs == refs_before,
            "endpoint_cap_dropped: refs_view unchanged (no fire)"
        );
    }
}

// Run the real `bind` and assert its contract against the body: `slot_view`
// unchanged; the `(end, event)` binding installed with every other channel field
// untouched; and the `refs_view` delta — old notif released, new acquired, in the
// decrement-then-increment order so a same-notif rebind is net-zero
// (`bind_refs_post`).
fn check_bind(
    st: &mut ArrayStore,
    ch: ObjId,
    end: ChanEnd,
    event: usize,
    notif: Option<ObjId>,
    bits: u64,
) {
    let e = end_idx_exec(end);
    let old_notif = st
        .chan(ch)
        .bindings
        .get(&(e, event))
        .copied()
        .unwrap_or(Binding::UNBOUND)
        .notif;
    let fp = fingerprint(st);
    let refs_before = st.refs.clone();
    let mut expect_chan = st.chan(ch).clone();
    expect_chan
        .bindings
        .insert((e, event), Binding { notif, bits });
    let mut expect_refs = refs_before.clone();
    if let Some(no) = old_notif {
        *expect_refs.get_mut(&no.0).unwrap() -= 1;
    }
    if let Some(nn) = notif {
        *expect_refs.get_mut(&nn.0).unwrap() += 1;
    }

    bind(st, ch, end, event, notif, bits);

    assert!(fingerprint(st) == fp, "bind: slot_view unchanged");
    assert!(
        *st.chan(ch) == expect_chan,
        "bind: only the (end,event) binding changed"
    );
    assert_eq!(st.refs, expect_refs, "bind: refs delta (old -1, new +1)");
}

// Run the real `destroy_channel` and assert its contract against the body — the
// check-against-its-body discipline. The contract's checkable core: `cspace_wf`
// preserved, the arena unchanged in extent, and **every ring-cap slot emptied**.
// The host test also checks the part kept out of the formal contract: each bound
// binding's notif ref released once.
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
    let (c0, sound0) = (count_nonempty_exec(st), refcount_sound_exec(st));
    let consistent0 = caps_consistent_exec(st);
    let end_caps0 = end_caps_sound_exec(st);
    let empty0 = st.slots.clone();
    let resid0 = st.cspaces.clone();

    destroy_channel(st, ch);

    assert_only_empties(&empty0, st, "destroy_channel post");
    assert!(
        cspace_wf_exec(st),
        "destroy_channel post: cspace_wf preserved"
    );
    // residency is immutable across teardown (the frame obj_unref's Channel arm reads).
    assert!(
        st.cspaces == resid0,
        "destroy_channel post: cspace residency unchanged"
    );
    assert_eq!(st.n(), n, "destroy_channel: arena extent unchanged");
    for cs in ring_caps {
        assert!(
            st.at(cs).cap.is_empty(),
            "destroy_channel: every ring cap slot emptied"
        );
    }
    assert_eq!(
        st.refs, expect_refs,
        "destroy_channel: each binding's notif ref released once"
    );
    assert!(
        count_nonempty_exec(st) <= c0,
        "destroy_channel: count_nonempty non-increase (§6a)"
    );
    if sound0 {
        assert!(
            refcount_sound_exec(st),
            "destroy_channel post: refcount_sound preserved (§6a)"
        );
    }
    if consistent0 {
        assert!(
            caps_consistent_exec(st),
            "destroy_channel post: caps_consistent preserved (§6d)"
        );
    }
    if end_caps0 {
        assert!(
            end_caps_sound_exec(st),
            "destroy_channel post: end_caps_sound preserved (§6d)"
        );
    }
}

// Run the real `delete` on a **notification** cap and assert the conditional
// frame against the body — the executable check of `delete`'s `ensures` (the
// check-against-its-body discipline): the TCB/channel/timer/notif views and every
// *other* slot's cap are untouched, and the designated `refs[n]` drops
// by one (the part the formal contract leaves to the host test). Refs start > 1 so the
// delete just decrements (no `destroy_notif`), isolating the frame.
fn check_delete_notif(st: &mut ArrayStore, slot: SlotId, n: ObjId) {
    assert!(cspace_wf_exec(st), "delete_notif pre: cspace_wf");
    assert!(
        matches!(st.at(slot).cap.kind, CapKind::Notification(_)),
        "delete_notif pre: notif cap"
    );
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
    assert!(
        st.at(slot).cap.is_empty(),
        "delete_notif post: target slot emptied"
    );
    assert!(
        count_nonempty_exec(st) < c0,
        "delete_notif post: count_nonempty drops"
    );
    // the conditional-notification object-view frame
    assert!(st.tcbs == tcbs0, "delete_notif: tcb_view unchanged");
    assert!(st.chans == chans0, "delete_notif: chan_view unchanged");
    assert!(st.timers == timers0, "delete_notif: timer_view unchanged");
    assert!(
        st.timer_armed_head == head0,
        "delete_notif: timer_head_view unchanged"
    );
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

// Run the real `thread::bind` and assert its contract against the body: `cspace_wf`
// preserved; only `bind_bits[which]` changes in `tcb_view`; the bind slot ends holding
// the moved cap (or empty on a `None` src) with `src` emptied; and the refs delta —
// the displaced notification released (`-1`), the moved-in cap net-zero (a move, not a
// copy, unlike `channel::bind`'s `+1`). The TCB analog of `check_bind`.
fn check_thread_bind(
    st: &mut ArrayStore,
    t: ObjId,
    which: usize,
    notif_src: Option<SlotId>,
    bits: u64,
) {
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
    assert!(
        st.tcbs == expect_tcbs,
        "thread_bind: only bind_bits[which] changed in tcb_view"
    );
    // slot effect.
    match notif_src {
        Some(src) => {
            assert!(
                !st.at(slot).cap.is_empty(),
                "thread_bind: moved cap now in the bind slot"
            );
            assert!(
                st.at(src).cap.is_empty(),
                "thread_bind: src emptied by the move"
            );
        }
        None => assert!(
            st.at(slot).cap.is_empty(),
            "thread_bind: unbind leaves the bind slot empty"
        ),
    }
    // refs delta: displaced notif -1; the moved cap is net-zero.
    let mut expect_refs = refs0.clone();
    if let Some(no) = old_displaced {
        *expect_refs.get_mut(&no.0).unwrap() -= 1;
    }
    assert_eq!(
        st.refs, expect_refs,
        "thread_bind: displaced notif -1, move net-zero"
    );
}

// `arm`'s ensures against the real body: `timer_wf` preserved; slot/chan/notif/
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
    assert_eq!(
        st.refs, expect_refs,
        "arm: net ref delta (re-arm -1, arm +1)"
    );
    assert!(st.timers[&t.0].armed, "arm: t armed");
    assert!(st.timers[&t.0].notif == Some(notif), "arm: bound to notif");
    assert_eq!(st.timers[&t.0].deadline, deadline, "arm: deadline set");
    assert_eq!(st.timers[&t.0].bits, bits, "arm: bits set");
    assert!(
        st.timer_armed_head == Some(t),
        "arm: pushed onto the list head"
    );
}

// `disarm`'s ensures against the real body: `timer_wf` preserved; the views
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
    assert_eq!(
        st.refs, expect_refs,
        "disarm: released the timer's ref iff it was armed"
    );
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

// `check_expired`'s ensures against the real body: `timer_wf` preserved, slot/
// chan views framed, and — stronger than the verified contract — every timer still on the
// armed list is unexpired (every `deadline <= now` was fired and disarmed by the sweep).
fn check_check_expired(st: &mut ArrayStore, now: u64) {
    assert!(timer_wf_exec(st), "check_expired pre: timer_wf");
    let fp = fingerprint(st);
    let chans = st.chans.clone();
    check_expired(st, now);
    assert!(timer_wf_exec(st), "check_expired post: timer_wf preserved");
    assert!(
        fingerprint(st) == fp,
        "check_expired post: slot_view unchanged"
    );
    assert!(st.chans == chans, "check_expired post: chan_view unchanged");
    let mut cur = st.timer_armed_head;
    while let Some(c) = cur {
        assert!(
            st.timers[&c.0].deadline > now,
            "check_expired: every survivor is unexpired"
        );
        cur = st.timers[&c.0].next;
    }
}

// `destroy_tcb`'s structural contract against the real body: `t` ends Halted with its
// queue link and both binding slots cleared, its report UNCHANGED (destruction fires no
// report, rev1§5.1), and `cspace_wf` preserved.
fn check_destroy_tcb(st: &mut ArrayStore, t: ObjId) {
    assert!(cspace_wf_exec(st), "destroy_tcb pre: cspace_wf");
    let n = st.n();
    let report0 = st.tcbs[&t.0].report;
    let s0 = st.tcbs[&t.0].bind_slots[0];
    let s1 = st.tcbs[&t.0].bind_slots[1];
    let (c0, sound0) = (count_nonempty_exec(st), refcount_sound_exec(st));
    let consistent0 = caps_consistent_exec(st);
    let end_caps0 = end_caps_sound_exec(st);
    let empty0 = st.slots.clone();
    let resid0 = st.cspaces.clone();
    let chans0 = st.chans.clone();

    destroy_tcb(st, t);

    assert_only_empties(&empty0, st, "destroy_tcb post");
    assert!(cspace_wf_exec(st), "destroy_tcb post: cspace_wf preserved");
    // residency is immutable across teardown (the frame obj_unref's Thread arm reads).
    assert!(
        st.cspaces == resid0,
        "destroy_tcb post: cspace residency unchanged"
    );
    // the channel skeleton is immutable — `destroy_tcb` touches no channel layout
    // (it deletes notification bind caps and unrefs cspace/aspace), so `chan_struct_frame`
    // (the ensures the Thread-arm `obj_unref` reads) holds. The real body leaves `chans`
    // wholly unchanged, which implies it.
    assert!(
        st.chans == chans0,
        "destroy_tcb post: channel state (skeleton) unchanged"
    );
    assert_eq!(st.n(), n, "destroy_tcb: arena extent unchanged");
    assert_eq!(
        st.tcbs[&t.0].state,
        ThreadState::Halted,
        "destroy_tcb: t halted"
    );
    assert!(
        st.tcbs[&t.0].qnext.is_none(),
        "destroy_tcb: queue link cleared"
    );
    // B8C-4: the faithful detach. A Runnable `t` was spliced out of its ready chain
    // (`unqueue_ready` → `ready_unqueue`) and then halted, so the ready queue is well-formed
    // and `t` sits on no chain at any level — `ready_complete` is restored for the survivors.
    assert!(ready_wf_exec(st), "destroy_tcb post: ready_wf preserved");
    for level in 0..crate::sysabi::NUM_PRIOS {
        assert!(
            !ready_seq_exec(st, level).iter().any(|x| x.0 == t.0),
            "destroy_tcb post: t spliced off every ready chain"
        );
    }
    assert_eq!(
        st.tcbs[&t.0].report, report0,
        "destroy_tcb: report unchanged"
    );
    assert!(
        st.at(s0).cap.is_empty(),
        "destroy_tcb: EXIT bind slot emptied"
    );
    assert!(
        st.at(s1).cap.is_empty(),
        "destroy_tcb: FAULT bind slot emptied"
    );
    assert!(
        count_nonempty_exec(st) <= c0,
        "destroy_tcb: count_nonempty non-increase (§6a)"
    );
    if sound0 {
        assert!(
            refcount_sound_exec(st),
            "destroy_tcb post: refcount_sound preserved (§6a)"
        );
    }
    if consistent0 {
        assert!(
            caps_consistent_exec(st),
            "destroy_tcb post: caps_consistent preserved (§6d)"
        );
    }
    if end_caps0 {
        assert!(
            end_caps_sound_exec(st),
            "destroy_tcb post: end_caps_sound preserved (§6d)"
        );
    }
}

// A halted thread (ObjId 200) with two bind slots (1 = EXIT, 2 = FAULT). With
// `with_binding`, slot `1+which` holds a notification cap (ObjId 100, bits 0b101) the
// thread's death will fire; with `with_waiter`, a separate thread (ObjId 201) is
// blocked on that notification (holding a queued ref) so the fire takes the wake path.
fn report_terminal_fixture(
    which: usize,
    with_binding: bool,
    with_waiter: bool,
) -> (ArrayStore, ObjId) {
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
                TcbState {
                    state: ThreadState::BlockedNotif,
                    wait_notif: Some(ObjId(100)),
                    ..tcb_state_default()
                },
            );
            st.notifs.insert(
                100,
                NotifState {
                    word: 0,
                    wait_head: Some(w),
                    wait_tail: Some(w),
                },
            );
            st.refs.insert(100, 2); // the bind cap's ref + the queued waiter's ref
        } else {
            st.notifs.insert(
                100,
                NotifState {
                    word: 0,
                    wait_head: None,
                    wait_tail: None,
                },
            );
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
        .find(|s| {
            !st.at(*s).cap.is_empty()
                && st.at(*s).first_child.is_some()
                && st.at(*s).parent.is_some()
        })
        .or_else(|| {
            (0..st.n())
                .map(|i| SlotId(i as u64))
                .find(|s| st.at(*s).first_child.is_some())
        })
        .expect("a non-leaf node");
    check_delete(&mut st, target);
}

#[test]
fn delete_notif_frame() {
    // Deleting a notification cap leaves every object view and every other slot
    // untouched (the conditional `delete` frame `thread::bind` reads off), and
    // drops only the designated notif's refcount. Refs start at 2 so the delete just
    // decrements (no destroy), isolating the frame.
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(frame_cap(0)); // an unrelated witness slot
    st.slots[1] = detached(notif_cap(100)); // the notification cap to delete
    st.slots[2] = detached(frame_cap(2)); // another unrelated cap
    st.refs.insert(100, 2);
    st.notifs.insert(
        100,
        NotifState {
            word: 7,
            wait_head: None,
            wait_tail: None,
        },
    );
    // a second notification + a TCB + a timer, present only to witness the frame.
    st.notifs.insert(
        101,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    st.refs.insert(101, 1);
    st.tcbs.insert(200, tcb_state_default());
    st.timers.insert(
        300,
        TimerState {
            armed: false,
            deadline: 0,
            notif: None,
            bits: 0,
            next: None,
        },
    );
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
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    st.notifs.insert(
        101,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    let t = ObjId(200);
    st.tcbs.insert(
        200,
        TcbState {
            bind_slots: [SlotId(1), SlotId(2)],
            ..tcb_state_default()
        },
    );

    // install onto the unbound EXIT slot: move notif 100 (slot 3) into bind slot 1.
    check_thread_bind(&mut st, t, BIND_EXIT, Some(SlotId(3)), 0b1);
    assert_eq!(
        st.refs[&100], 1,
        "a move keeps the cap's ref (move, not copy — no +1)"
    );
    assert!(st.at(SlotId(3)).cap.is_empty(), "src slot emptied");

    // rebind EXIT to a different notif (slot 4): old notif 100 released, 101 moved in.
    check_thread_bind(&mut st, t, BIND_EXIT, Some(SlotId(4)), 0b10);
    assert_eq!(st.refs[&100], 0, "displaced notif 100 released");
    assert_eq!(st.refs[&101], 1, "new notif moved in (net-zero)");

    // unbind EXIT (None src): displaced notif 101 released, the bind slot empties.
    check_thread_bind(&mut st, t, BIND_EXIT, None, 0);
    assert_eq!(st.refs[&101], 0, "unbind released the displaced notif");
    assert!(
        st.at(SlotId(1)).cap.is_empty(),
        "unbind leaves the bind slot empty"
    );
}

#[test]
fn report_terminal_first_call_wins_and_fires() {
    // ReportMonotone + the fire: a Running thread's first `report_terminal` records the
    // report and fires the EXIT binding (the queued waiter is woken); a second call is
    // an absorbing no-op.
    let (mut st, t) = report_terminal_fixture(BIND_EXIT, true, true);
    let w = ObjId(201);
    report_terminal(&mut st, t, Report::Exited(42));
    assert_eq!(
        st.tcbs[&t.0].report,
        Report::Exited(42),
        "first call records the report"
    );
    assert_eq!(
        st.tcbs[&w.0].state,
        ThreadState::Runnable,
        "the bound waiter was woken (binding fired)"
    );
    assert_eq!(
        st.tcbs[&w.0].retval, 0b101,
        "the waiter received the binding bits"
    );
    assert_eq!(st.refs[&100], 1, "the woken waiter's queued ref released");

    let refs_before = st.refs.clone();
    let tcbs_before = st.tcbs.clone();
    report_terminal(&mut st, t, Report::Exited(99));
    assert_eq!(
        st.tcbs[&t.0].report,
        Report::Exited(42),
        "second call no-op: report unchanged (absorbing)"
    );
    assert!(
        st.refs == refs_before && st.tcbs == tcbs_before,
        "second call touches nothing"
    );
}

#[test]
fn report_terminal_fault_arm_fires_fault_binding() {
    // A Faulted report fires the FAULT binding (BIND_FAULT), not EXIT.
    let (mut st, t) = report_terminal_fixture(BIND_FAULT, true, true);
    let w = ObjId(201);
    report_terminal(
        &mut st,
        t,
        Report::Faulted {
            cause: 0x96,
            far: 0xdead_0000,
        },
    );
    assert!(
        matches!(st.tcbs[&t.0].report, Report::Faulted { .. }),
        "fault recorded"
    );
    assert_eq!(
        st.tcbs[&w.0].state,
        ThreadState::Runnable,
        "the FAULT binding fired"
    );
}

#[test]
fn report_terminal_accumulate_no_waiter() {
    // Firing a binding with no queued waiter accumulates the bits into the word.
    let (mut st, t) = report_terminal_fixture(BIND_EXIT, true, false);
    report_terminal(&mut st, t, Report::Exited(7));
    assert_eq!(st.tcbs[&t.0].report, Report::Exited(7), "report recorded");
    assert_eq!(
        st.notifs[&100].word, 0b101,
        "no waiter: the binding bits accumulate in the word"
    );
}

#[test]
fn report_terminal_firesafe_empty_slot() {
    // FireSafe: an empty bind slot (a revoke raced the death and cleared it) ⇒ the fire
    // is a no-op, no panic, and the report still records.
    let (mut st, t) = report_terminal_fixture(BIND_EXIT, false, false);
    report_terminal(&mut st, t, Report::Exited(5));
    assert_eq!(
        st.tcbs[&t.0].report,
        Report::Exited(5),
        "report recorded even with an empty bind slot"
    );
}

#[test]
fn cdt_unlink_middle_sibling() {
    // Three siblings under one parent; unlink the middle one.
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(frame_cap(0)); // parent (root)
                                          // children c1=1, c2=2, c3=3 as 0's first_child chain
    derive(&mut st, SlotId(0), SlotId(3), 0xff, 0xFF).unwrap(); // 0.first_child = 3
    derive(&mut st, SlotId(0), SlotId(2), 0xff, 0xFF).unwrap(); // 0.first_child = 2, 2.next = 3
    derive(&mut st, SlotId(0), SlotId(1), 0xff, 0xFF).unwrap(); // 0.first_child = 1, 1.next = 2
    assert!(cspace_wf_exec(&st));
    check_cdt_unlink(&mut st, SlotId(2)); // the middle sibling
}

#[test]
fn derive_preserves_thread_priority_ceiling() {
    // rev1§5.4/rev1§2.3 monotone priority axis: with the no-reduction sentinel
    // (`prio_ceiling = 0xFF`), a derived thread cap carries the same — hence `<=` —
    // max-controlled-priority ceiling as its parent. This is the executable witness
    // of `derive`'s ceiling `ensures` on the real body for the default `cap_copy`,
    // the priority analogue of the rights-subset witnesses.
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(thread_cap(42, 19)); // parent ceiling = 19
    st.refs.insert(42, 1); // derive bumps the designated TCB's refcount
    derive(&mut st, SlotId(0), SlotId(1), 0xff, 0xFF).expect("derive thread child");
    match (st.at(SlotId(0)).cap.kind, st.at(SlotId(1)).cap.kind) {
        (CapKind::Thread(po, pmp), CapKind::Thread(co, cmp)) => {
            assert!(co.0 == po.0, "derived thread cap designates the same TCB");
            assert_eq!(
                cmp, pmp,
                "ceiling preserved across derivation (no-reduction sentinel)"
            );
            assert!(
                cmp <= pmp,
                "rev1§5.4 ceiling attenuates monotonically (child <= parent)"
            );
        }
        _ => panic!("derived cap is not a thread cap"),
    }
}

#[test]
fn derive_attenuates_thread_priority_ceiling() {
    // rev1§2.3 supervision grant: a thread-cap copy can carry a *strictly lower*
    // ceiling — `min(parent, prio_ceiling)`. Executable witness of
    // `derived_kind`'s reducing `Thread` arm + `derive`'s strengthened ceiling
    // `ensures` on the real body.
    let mut st = ArrayStore::new(4);
    st.slots[0] = detached(thread_cap(42, 19)); // parent ceiling = 19
    st.refs.insert(42, 1);
    // Request ceiling 5 < parent 19 ⇒ child ceiling = min(19, 5) = 5.
    derive(&mut st, SlotId(0), SlotId(1), 0xff, 5).expect("derive attenuated thread child");
    match (st.at(SlotId(0)).cap.kind, st.at(SlotId(1)).cap.kind) {
        (CapKind::Thread(po, pmp), CapKind::Thread(co, cmp)) => {
            assert!(co.0 == po.0, "derived thread cap designates the same TCB");
            assert_eq!(pmp, 19, "parent ceiling unchanged by the copy");
            assert_eq!(cmp, 5, "child ceiling = min(parent, prio_ceiling) = 5");
            assert!(
                cmp <= pmp,
                "rev1§5.4 ceiling still monotone (child <= parent)"
            );
        }
        _ => panic!("derived cap is not a thread cap"),
    }
    // A `prio_ceiling` above the parent does not raise it (min is a floor on shrink).
    let mut st2 = ArrayStore::new(4);
    st2.slots[0] = detached(thread_cap(7, 4)); // parent ceiling = 4
    st2.refs.insert(7, 1);
    derive(&mut st2, SlotId(0), SlotId(1), 0xff, 30).expect("derive thread child");
    match st2.at(SlotId(1)).cap.kind {
        CapKind::Thread(_, cmp) => assert_eq!(cmp, 4, "ceiling cannot be raised above parent"),
        _ => panic!("derived cap is not a thread cap"),
    }
}

#[test]
fn set_priority_writes_within_ceiling() {
    // `thread::set_priority` accepts an in-ceiling request, writing the priority
    // into the TCB through the verified Store seam — the post-state priority is
    // exactly the requested value (hence `<= ceiling`) and the call returns `Ok`.
    // Executable witness on `ArrayStore`.
    let mut st = ArrayStore::new(1);
    st.tcbs.insert(9, tcb_state_default()); // priority starts at 0
    assert!(crate::thread::set_priority(&mut st, ObjId(9), 5, 16).is_ok(), "in-ceiling accepted");
    assert_eq!(st.tcb_priority(ObjId(9)), 5, "priority written exactly");
    assert!(st.tcb_priority(ObjId(9)) <= 16, "priority within ceiling");
    // Boundary: prio == ceiling is admissible.
    assert!(crate::thread::set_priority(&mut st, ObjId(9), 16, 16).is_ok(), "prio == ceiling accepted");
    assert_eq!(st.tcb_priority(ObjId(9)), 16, "prio == ceiling allowed");
}

#[test]
fn set_priority_refuses_over_ceiling() {
    // The rev1§6.1(d) gate: `thread::set_priority` *refuses* an over-ceiling
    // request — returns `Err` and leaves the TCB's priority untouched, no shell
    // `if` involved. A subsequent in-ceiling request still succeeds.
    let mut st = ArrayStore::new(1);
    st.tcbs.insert(7, tcb_state_default()); // priority starts at 0
    // Seed a known in-ceiling priority so the refusal's "untouched" is observable.
    assert!(crate::thread::set_priority(&mut st, ObjId(7), 4, 16).is_ok());
    assert_eq!(st.tcb_priority(ObjId(7)), 4);
    // Over-ceiling: refused, priority unchanged.
    assert!(crate::thread::set_priority(&mut st, ObjId(7), 20, 16).is_err(), "over-ceiling refused");
    assert_eq!(st.tcb_priority(ObjId(7)), 4, "refused write leaves priority untouched");
    // The op stays usable: a fresh in-ceiling request is accepted.
    assert!(crate::thread::set_priority(&mut st, ObjId(7), 9, 16).is_ok(), "in-ceiling still accepted");
    assert_eq!(st.tcb_priority(ObjId(7)), 9, "later in-ceiling write lands");
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
    check_retype_check(
        &mut st,
        SlotId(0),
        ObjType::Channel,
        SlotId(1),
        Some(SlotId(3)),
    );
    // Channel DestOccupied: dst2 missing.
    check_retype_check(&mut st, SlotId(0), ObjType::Channel, SlotId(1), None);
    // Channel DestOccupied: dst2 aliases dst.
    check_retype_check(
        &mut st,
        SlotId(0),
        ObjType::Channel,
        SlotId(1),
        Some(SlotId(1)),
    );
    // Channel DestOccupied: dst2 occupied.
    check_retype_check(
        &mut st,
        SlotId(0),
        ObjType::Channel,
        SlotId(1),
        Some(SlotId(2)),
    );
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
        CapKind::Frame {
            base: 0x2000,
            pages: 1,
            mapping: None,
        },
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
        CapKind::Thread(ObjId(50), 7),
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
        CapKind::Untyped {
            base: 0x2000,
            size: 0x1000,
            watermark: 0,
        },
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
    // The cross-object case: deleting the last cap to a cspace tears down its
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
    assert!(
        !st.at(SlotId(2)).cap.is_empty(),
        "resident survives (object still live)"
    );
    // Now drop the second cap: refcount hits zero and the resident is reclaimed.
    check_delete(&mut st, SlotId(1));
    assert_eq!(st.refs[&10], 0);
    assert!(
        st.at(SlotId(2)).cap.is_empty(),
        "resident reclaimed at last ref"
    );
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
    assert!(
        !cspace_wf_exec(&st),
        "half-linked siblings must be rejected"
    );
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
        st.slots[2] = CapSlot {
            cap: frame_cap(2),
            parent: Some(SlotId(0)),
            first_child: None,
            next_sib: None,
            prev_sib: None,
        };
        st.slots[1] = detached(frame_cap(1));
        // ring: 1.next=1 self-loop, consistent prev, parented but not the head
        st.slots[1].parent = Some(SlotId(0));
        st.slots[1].next_sib = Some(SlotId(1));
        st.slots[1].prev_sib = Some(SlotId(1));
        st
    };
    assert!(
        !no_cycle(&st, |s| s.next_sib),
        "sibling self-loop must be a cycle"
    );
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
            let live: Vec<SlotId> = (0..st.n())
                .map(|i| SlotId(i as u64))
                .filter(|s| !st.at(*s).cap.is_empty())
                .collect();
            let pick = live[(seed as usize) % live.len()];
            check_delete(&mut st, pick);
            trials += 1;
        }
        // cdt_unlink
        {
            let mut st = gen_forest(seed.wrapping_mul(5).wrapping_add(2), n, edges);
            let live: Vec<SlotId> = (0..st.n())
                .map(|i| SlotId(i as u64))
                .filter(|s| !st.at(*s).cap.is_empty())
                .collect();
            let pick = live[(seed as usize * 7) % live.len()];
            check_cdt_unlink(&mut st, pick);
            trials += 1;
        }
        // slot_move
        {
            let mut st = gen_forest(seed.wrapping_mul(7).wrapping_add(3), n, edges);
            let live: Vec<SlotId> = (0..st.n())
                .map(|i| SlotId(i as u64))
                .filter(|s| !st.at(*s).cap.is_empty())
                .collect();
            let free: Vec<SlotId> = (0..st.n())
                .map(|i| SlotId(i as u64))
                .filter(|s| st.at(*s).cap.is_empty())
                .collect();
            if !free.is_empty() {
                let src = live[(seed as usize * 11) % live.len()];
                let dst = free[(seed as usize) % free.len()];
                check_slot_move(&mut st, src, dst);
                trials += 1;
            }
        }
    }
    assert!(
        trials > 500,
        "sweep should exercise hundreds of trials, ran {trials}"
    );
}

// ── Channel ghost view ─────────────────────────────────────────────────────

#[test]
fn signal_frame() {
    // The `signal` contract: the real body leaves `slot_view`/`chan_view`
    // untouched on BOTH paths, while its intended effects (accumulate / deliver)
    // still happen — so the frame is real, not a no-op masquerading as one.

    // No-waiter: the bits accumulate in the word; nothing else moves.
    let (mut st, n) = signal_fixture(false);
    assert!(
        chan_wf_exec(&st, ObjId(7)),
        "fixture channel is well-formed"
    );
    check_signal_frame(&mut st, n, 0b101);
    assert_eq!(
        st.notifs[&100].word, 0b101,
        "no-waiter signal accumulated the bits"
    );

    // One waiter: the whole word is delivered, cleared, and the queued ref freed
    // — all OUTSIDE the slot/chan frame the contract pins.
    let (mut st, n) = signal_fixture(true);
    check_signal_frame(&mut st, n, 0b110);
    assert_eq!(st.notifs[&100].word, 0, "delivered word cleared");
    assert_eq!(
        st.tcbs[&200].retval, 0b110,
        "waiter received the whole word"
    );
    assert!(st.notifs[&100].wait_head.is_none(), "waiter dequeued");
    assert_eq!(st.refs[&100], 0, "waiter's queued ref released");
    // The `make_runnable` contract, host-checked: the woken thread is Runnable.
    assert_eq!(
        st.tcbs[&200].state,
        ThreadState::Runnable,
        "woken waiter made Runnable"
    );
    // B8C-4: and enqueued at the tail of its priority level (0) — the sole node there, with
    // the presence bit set and its qnext cleared (the precise `ready_enqueue` placement).
    assert_eq!(ready_ids(&st, 0), vec![200], "woken waiter is the sole level-0 ready node");
    assert!(st.ready_heads[0] == Some(ObjId(200)) && st.ready_tails[0] == Some(ObjId(200)));
    assert_eq!(st.ready_bitmap & 1, 1, "level-0 presence bit set");
    assert!(st.tcbs[&200].qnext.is_none(), "the enqueued thread's qnext is cleared");
}

#[test]
fn chan_wf_exec_has_teeth() {
    // `chan_wf_exec` (and so `check_signal_frame`'s precondition) is only
    // meaningful if it rejects malformed channels. Each shape violates exactly
    // one clause; the windowing coupling (out-of-window slot non-empty) is the
    // load-bearing clause.
    let ch = ObjId(7);
    assert!(
        chan_wf_exec(&signal_fixture(false).0, ch),
        "a well-formed channel must be accepted"
    );

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
    assert!(
        !chan_wf_exec(&st, ch),
        "out-of-window non-empty ring cap must be rejected"
    );

    let mut st = signal_fixture(false).0;
    st.chans
        .get_mut(&7)
        .unwrap()
        .ring_cap
        .insert((1, 0, 0), SlotId(999));
    assert!(
        !chan_wf_exec(&st, ch),
        "ring cap handle outside the arena must be rejected"
    );

    // Injectivity: (1,0,1) aliases (1,0,0)'s slot 5. Both ring-1 caps are
    // out-of-window and slot 5 is empty, so the windowing clause is satisfied —
    // only the injectivity clause rejects this.
    let mut st = signal_fixture(false).0;
    st.chans
        .get_mut(&7)
        .unwrap()
        .ring_cap
        .insert((1, 0, 1), SlotId(5));
    assert!(
        !chan_wf_exec(&st, ch),
        "two ring positions aliasing one slot must be rejected"
    );

    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().bindings.remove(&(1, 2));
    assert!(
        !chan_wf_exec(&st, ch),
        "incomplete bindings domain must be rejected"
    );

    assert!(
        !chan_wf_exec(&signal_fixture(false).0, ObjId(999)),
        "unknown channel must be rejected"
    );
}

// ── Notification waiter-queue well-formedness ───────────────────────────────

// A notification (ObjId 100) with a two-deep FIFO waiter chain 200 → 201: the
// `notif_wf` fixture for the teeth test. `notif_wf_exec` must accept it.
fn notif_fixture() -> ArrayStore {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: Some(ObjId(200)),
            wait_tail: Some(ObjId(201)),
        },
    );
    st.tcbs.insert(
        200,
        TcbState {
            state: ThreadState::BlockedNotif,
            wait_notif: Some(n),
            qnext: Some(ObjId(201)),
            ..tcb_state_default()
        },
    );
    st.tcbs.insert(
        201,
        TcbState {
            state: ThreadState::BlockedNotif,
            wait_notif: Some(n),
            qnext: None,
            ..tcb_state_default()
        },
    );
    st
}

#[test]
fn notif_wf_exec_has_teeth() {
    // `notif_wf_exec` (the precondition the `signal`/`wait`/`remove_waiter`
    // proofs rest on) is only meaningful if it rejects malformed queues. Each shape
    // violates exactly one clause; a well-formed queue is accepted.
    let n = ObjId(100);
    assert!(
        notif_wf_exec(&notif_fixture(), n),
        "a well-formed waiter queue must be accepted"
    );

    // empty-queue head/tail disagreement: head Some, tail None.
    let mut st = notif_fixture();
    st.notifs.get_mut(&100).unwrap().wait_tail = None;
    assert!(
        !notif_wf_exec(&st, n),
        "head/tail disagreement must be rejected"
    );

    // a qnext cycle (201 → 200 → 201 …): the walk never terminates.
    let mut st = notif_fixture();
    st.tcbs.get_mut(&201).unwrap().qnext = Some(ObjId(200));
    assert!(!notif_wf_exec(&st, n), "a qnext cycle must be rejected");

    // a charted node naming the wrong notification.
    let mut st = notif_fixture();
    st.tcbs.get_mut(&201).unwrap().wait_notif = Some(ObjId(999));
    assert!(
        !notif_wf_exec(&st, n),
        "a waiter naming another notification must be rejected"
    );

    // a charted node not in BlockedNotif.
    let mut st = notif_fixture();
    st.tcbs.get_mut(&201).unwrap().state = ThreadState::Runnable;
    assert!(
        !notif_wf_exec(&st, n),
        "a non-BlockedNotif waiter must be rejected"
    );

    // wait_tail names a node that is not the chain's end.
    let mut st = notif_fixture();
    st.notifs.get_mut(&100).unwrap().wait_tail = Some(ObjId(200));
    assert!(
        !notif_wf_exec(&st, n),
        "wait_tail off the chain end must be rejected"
    );

    // a charted node that is not a live TCB (201 removed, 200 still points at it).
    let mut st = notif_fixture();
    st.tcbs.remove(&201);
    assert!(
        !notif_wf_exec(&st, n),
        "a charted node with no live TCB must be rejected"
    );

    assert!(
        !notif_wf_exec(&notif_fixture(), ObjId(999)),
        "unknown notification must be rejected"
    );
}

// A store exercising **all six** census terms at once, with `refs` set to each
// object's true `obj_census` — the positive witness and the base for the per-term
// teeth perturbations. Objects: cspace C=1, aspace A=2, notif N=3, timer T=4,
// channel CH=5, thread TH=6.
fn refcount_sound_fixture() -> ArrayStore {
    let mut st = ArrayStore::new(7);
    // One designating slot cap per object (slot_refs).
    st.slots[0] = detached(cspace_cap(1));
    st.slots[1] = detached(Cap {
        kind: CapKind::Aspace(ObjId(2)),
        rights: Rights(0xff),
    });
    st.slots[2] = detached(notif_cap(3));
    st.slots[3] = detached(Cap {
        kind: CapKind::Timer(ObjId(4)),
        rights: Rights(0xff),
    });
    st.slots[4] = detached(Cap {
        kind: CapKind::Channel(ObjId(5), ChanEnd::A),
        rights: Rights(0xff),
    });
    st.slots[5] = detached(thread_cap(6, 5));
    // A frame mapped into A → frame_map_refs(A) += 1.
    st.slots[6] = detached(Cap {
        kind: CapKind::Frame {
            base: 0x1000,
            pages: 1,
            mapping: Some((ObjId(2), 0x4000)),
        },
        rights: Rights(0xff),
    });
    // A channel binding (end 0, ev 0) naming N → binding_refs(N) += 1.
    let mut bindings = BTreeMap::new();
    for e in 0..2usize {
        for v in 0..3usize {
            let notif = if (e, v) == (0, 0) {
                Some(ObjId(3))
            } else {
                None
            };
            bindings.insert((e, v), Binding { notif, bits: 0 });
        }
    }
    st.chans.insert(
        5,
        ChanState {
            depth: 0,
            end_caps: [0, 0],
            head: [0, 0],
            count: [0, 0],
            bindings,
            msg_len: BTreeMap::new(),
            ring_cap: BTreeMap::new(),
        },
    );
    // TH blocked on N (waiter_refs(N) += 1) and holding C/A (thread_hold).
    st.notifs.insert(
        3,
        NotifState {
            word: 0,
            wait_head: Some(ObjId(6)),
            wait_tail: Some(ObjId(6)),
        },
    );
    st.tcbs.insert(
        6,
        TcbState {
            state: ThreadState::BlockedNotif,
            wait_notif: Some(ObjId(3)),
            cspace: Some(ObjId(1)),
            aspace: Some(ObjId(2)),
            ..tcb_state_default()
        },
    );
    // An armed timer bound to N → armed_timer_refs(N) += 1.
    st.timers.insert(
        4,
        TimerState {
            armed: true,
            deadline: 0,
            notif: Some(ObjId(3)),
            bits: 0,
            next: None,
        },
    );
    st.timer_armed_head = Some(ObjId(4));
    // refs = census: C 1+1=2, A 1+1+1=3, N 1+1+1+1=4, T 1, CH 1, TH 1.
    for (o, r) in [(1u64, 2u32), (2, 3), (3, 4), (4, 1), (5, 1), (6, 1)] {
        st.refs.insert(o, r);
    }
    st
}

// `refcount_sound_exec` (the census mirror the teardown contracts are host-checked
// against) is only meaningful if it rejects a census mismatch in
// **each** term. The assembled fixture is sound; perturbing any one term — slot,
// frame-mapping, binding, waiter, armed-timer, thread-hold — must be rejected.
#[test]
fn refcount_sound_exec_has_teeth() {
    let base = refcount_sound_fixture();
    assert!(
        refcount_sound_exec(&base),
        "the all-terms fixture must be refcount_sound"
    );

    // slot_refs: drop a designating cap without lowering refs.
    let mut st = base.clone();
    st.slots[5] = detached(Cap::EMPTY);
    assert!(!refcount_sound_exec(&st), "teeth: slot_refs term");

    // frame_map_refs: unmap the frame (mapping → None).
    let mut st = base.clone();
    st.slots[6] = detached(frame_cap(0x1000));
    assert!(!refcount_sound_exec(&st), "teeth: frame_map_refs term");

    // binding_refs: drop the channel binding's notification.
    let mut st = base.clone();
    st.chan_mut(ObjId(5)).bindings.insert(
        (0, 0),
        Binding {
            notif: None,
            bits: 0,
        },
    );
    assert!(!refcount_sound_exec(&st), "teeth: binding_refs term");

    // waiter_refs: empty the waiter chain.
    let mut st = base.clone();
    st.notifs.get_mut(&3).unwrap().wait_head = None;
    assert!(!refcount_sound_exec(&st), "teeth: waiter_refs term");

    // armed_timer_refs: disarm the timer.
    let mut st = base.clone();
    st.timers.get_mut(&4).unwrap().armed = false;
    assert!(!refcount_sound_exec(&st), "teeth: armed_timer_refs term");

    // thread_hold_refs: clear the thread's cspace hold.
    let mut st = base.clone();
    st.tcbs.get_mut(&6).unwrap().cspace = None;
    assert!(!refcount_sound_exec(&st), "teeth: thread_hold_refs term");
}

// A `caps_consistent` store: slots 0..4 hold one well-formed object cap each (the objects
// it designates are wf), slots 4/5 empty. The Channel arm is exercised separately by
// `chan_wf_exec_has_teeth` + the `check_destroy_channel` path (a `chan_wf` channel needs the
// full ring/window setup `signal_fixture` builds); this fixture covers the four kinds whose
// objects are cheap to construct, which is enough to show the mirror is not vacuous.
fn caps_consistent_fixture() -> ArrayStore {
    let mut st = ArrayStore::new(6);
    // CSpace(10): residents are the two in-bounds empty slots 4, 5.
    st.slots[0] = detached(cspace_cap(10));
    st.cspaces.insert(10, vec![SlotId(4), SlotId(5)]);
    // Notification(20): an empty (and so well-formed) waiter queue.
    st.slots[1] = detached(notif_cap(20));
    st.notifs.insert(
        20,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    // Timer(30): disarmed, so the armed chain (empty) is complete and wf.
    st.slots[2] = detached(Cap {
        kind: CapKind::Timer(ObjId(30)),
        rights: Rights(0xff),
    });
    st.timers.insert(
        30,
        TimerState {
            armed: false,
            deadline: 0,
            notif: None,
            bits: 0,
            next: None,
        },
    );
    // Thread(40): both bind slots in-bounds.
    st.slots[3] = detached(thread_cap(40, 9));
    st.tcbs.insert(
        40,
        TcbState {
            bind_slots: [SlotId(4), SlotId(5)],
            ..tcb_state_default()
        },
    );
    st
}

#[test]
fn caps_consistent_exec_has_teeth() {
    let base = caps_consistent_fixture();
    assert!(
        caps_consistent_exec(&base),
        "the all-kinds fixture must be caps_consistent"
    );

    // CSpace arm: a resident handle outside the arena.
    let mut st = base.clone();
    st.cspaces.get_mut(&10).unwrap()[0] = SlotId(999);
    assert!(
        !caps_consistent_exec(&st),
        "teeth: CSpace resident out of arena"
    );

    // Notification arm: a head/tail disagreement breaks `notif_wf`.
    let mut st = base.clone();
    st.notifs.get_mut(&20).unwrap().wait_head = Some(ObjId(40));
    assert!(
        !caps_consistent_exec(&st),
        "teeth: Notification waiter-chain malformed"
    );

    // Timer arm: an armed timer absent from the (empty) armed chain breaks `timer_wf`.
    let mut st = base.clone();
    st.timers.get_mut(&30).unwrap().armed = true;
    assert!(!caps_consistent_exec(&st), "teeth: armed timer not charted");

    // Thread arm: a bind slot outside the arena.
    let mut st = base.clone();
    st.tcbs.get_mut(&40).unwrap().bind_slots[0] = SlotId(999);
    assert!(
        !caps_consistent_exec(&st),
        "teeth: Thread bind slot out of arena"
    );

    // The designating object missing entirely (CSpace cap with no cspace) also fails.
    let mut st = base.clone();
    st.cspaces.remove(&10);
    assert!(
        !caps_consistent_exec(&st),
        "teeth: CSpace cap with no live cspace"
    );
}

// A minimal `end_caps_sound` fixture: channel 7 with `end_caps == [1, 1]` and exactly
// one `Channel(7, A)` and one `Channel(7, B)` cap in the arena.
fn end_caps_fixture() -> ArrayStore {
    let mut st = ArrayStore::new(2);
    st.slots[0] = detached(Cap {
        kind: CapKind::Channel(ObjId(7), ChanEnd::A),
        rights: Rights(0xff),
    });
    st.slots[1] = detached(Cap {
        kind: CapKind::Channel(ObjId(7), ChanEnd::B),
        rights: Rights(0xff),
    });
    st.chans.insert(
        7,
        ChanState {
            depth: 1,
            end_caps: [1, 1],
            head: [0, 0],
            count: [0, 0],
            bindings: BTreeMap::new(),
            msg_len: BTreeMap::new(),
            ring_cap: BTreeMap::new(),
        },
    );
    st
}

// `end_caps_sound_exec` is non-vacuous: it accepts the matched fixture and rejects both an
// over-count (`end_caps` claims more caps than the arena holds) and an under-count (the
// stranding shape — a live `(co, end)` cap `end_caps` fails to count).
#[test]
fn end_caps_sound_exec_has_teeth() {
    let base = end_caps_fixture();
    assert!(
        end_caps_sound_exec(&base),
        "the matched fixture must be end_caps_sound"
    );

    let mut st = base.clone();
    st.chan_mut(ObjId(7)).end_caps[0] = 2;
    assert!(
        !end_caps_sound_exec(&st),
        "teeth: end_caps[A] overcounts the arena"
    );

    let mut st = base.clone();
    st.chan_mut(ObjId(7)).end_caps[1] = 0;
    assert!(
        !end_caps_sound_exec(&st),
        "teeth: end_caps[B] undercounts the arena (stranding)"
    );
}

// ── Aspace teardown: `unref_aspace` + delete's frame-unmap branch ─────────────

// A `refcount_sound` store whose only references to aspace `a` are `nframes` detached
// Frame caps mapped into it (`refs[a] == nframes`), so `census(a) == frame_map_refs ==
// nframes`. Deleting one frame exercises delete's `aspace_unmap` + `unref_aspace` path.
fn mapped_frame_fixture(a: u64, nframes: usize) -> ArrayStore {
    let mut st = ArrayStore::new(nframes);
    for i in 0..nframes {
        let off = i as u64 * 0x1000;
        st.slots[i] = detached(Cap {
            kind: CapKind::Frame {
                base: 0x1000 + off,
                pages: 1,
                mapping: Some((ObjId(a), 0x4000 + off)),
            },
            rights: Rights(0xff),
        });
    }
    st.refs.insert(a, nframes as u32);
    st
}

// Deleting a non-last mapped frame drops the aspace ref by one (the frame-mapping
// census term moves in lockstep with `unref_aspace`'s `-1`); the aspace
// survives. The generic `check_delete` asserts `cspace_wf`/count-drop/`refcount_sound`;
// this adds the aspace-specific outcome.
#[test]
fn delete_mapped_frame_drops_aspace_ref() {
    let mut st = mapped_frame_fixture(2, 2);
    assert!(refcount_sound_exec(&st), "fixture is refcount_sound");
    check_delete(&mut st, SlotId(0));
    assert_eq!(
        st.refs[&2], 1,
        "delete mapped frame: aspace ref dropped, not destroyed"
    );
    assert_eq!(
        obj_census_exec(&st, ObjId(2)),
        1,
        "delete mapped frame: census == refs preserved"
    );
}

// ── B8A: `map_frame` — the verified cap-side map record (the inverse of delete's branch) ──

// Records `Some((asp, va))` on an unmapped frame cap and bumps the aspace refcount (the
// `frame_map_refs` census term rises with `ref_aspace`'s `+1`), leaving the store sound. The
// host counterpart of `map_frame`'s `ensures`; mirrors `check_unref_aspace`.
fn check_map_frame(st: &mut ArrayStore, slot: SlotId, asp: ObjId, va: u64) {
    assert!(refcount_sound_exec(st), "map_frame pre: refcount_sound");
    assert!(
        matches!(st.slots[slot.0 as usize].cap.kind, CapKind::Frame { mapping: None, .. }),
        "map_frame pre: the frame is unmapped"
    );
    assert!(st.refs.contains_key(&asp.0), "map_frame pre: the aspace is live");
    let r0 = st.refs[&asp.0];
    let res = map_frame(st, slot, asp, va, 0);
    assert!(res.is_ok(), "map_frame: the test store's aspace_map always succeeds");
    assert!(
        cap_frame_aspace_exec(st.slots[slot.0 as usize].cap) == Some(asp),
        "map_frame post: the mapping is recorded on the frame cap"
    );
    assert_eq!(st.refs[&asp.0], r0 + 1, "map_frame post: the aspace ref is bumped");
    assert!(refcount_sound_exec(st), "map_frame post: refcount_sound restored");
}

#[test]
fn map_frame_records_and_bumps() {
    let mut st = ArrayStore::new(1);
    st.slots[0] = detached(frame_cap(0x1000)); // an unmapped frame cap
    st.refs.insert(2, 0); // a live, as-yet-unreferenced aspace
    check_map_frame(&mut st, SlotId(0), ObjId(2), 0x4000);
    assert_eq!(st.refs[&2], 1, "map_frame: refs[asp] == 1 after one mapping");
}

// `map_frame` then `delete` is the identity on the aspace refcount: map records + bumps,
// delete's frame-unmap branch clears + drops. The symmetry B8A delivers (derive proves
// unmapped-on-copy, `map_frame` record-on-map, `delete` clear-on-unmap).
#[test]
fn map_then_delete_roundtrip() {
    let mut st = ArrayStore::new(2);
    // slot 0: already mapped into aspace 2 (keeps it alive across the delete); slot 1: unmapped.
    st.slots[0] = detached(Cap {
        kind: CapKind::Frame {
            base: 0x1000,
            pages: 1,
            mapping: Some((ObjId(2), 0x4000)),
        },
        rights: Rights(0xff),
    });
    st.slots[1] = detached(frame_cap(0x2000));
    st.refs.insert(2, 1); // one mapped frame ⇒ census(2) == 1
    assert!(refcount_sound_exec(&st), "fixture is refcount_sound");
    map_frame(&mut st, SlotId(1), ObjId(2), 0x8000, 0).expect("map_frame ok");
    assert_eq!(st.refs[&2], 2, "after map: aspace ref bumped to 2");
    assert!(refcount_sound_exec(&st), "after map: refcount_sound");
    check_delete(&mut st, SlotId(1)); // delete the newly-mapped frame
    assert_eq!(st.refs[&2], 1, "after delete: aspace ref restored to 1");
    assert!(refcount_sound_exec(&st), "after delete: refcount_sound");
}

// Deleting the *last* mapped frame drives `unref_aspace` to zero, firing
// `aspace_destroy` (the trusted page-table free) — the aspace leaves the live set.
#[test]
fn delete_last_mapped_frame_destroys_aspace() {
    let mut st = mapped_frame_fixture(2, 1);
    assert!(refcount_sound_exec(&st), "fixture is refcount_sound");
    check_delete(&mut st, SlotId(0));
    assert!(
        !st.refs.contains_key(&2),
        "delete last mapped frame: aspace_destroy removed A"
    );
}

// `unref_aspace` on a non-last ref: the off-by-one state (the all-terms census fixture
// with `refs[A]` bumped by one, mirroring a caller that already cleared a hold naming A)
// decrements back to soundness; A stays live.
#[test]
fn unref_aspace_non_last_decrements() {
    let mut st = refcount_sound_fixture();
    *st.refs.get_mut(&2).unwrap() += 1; // refs[A] = census(A) + 1
    check_unref_aspace(&mut st, ObjId(2));
    assert!(
        st.refs.contains_key(&2),
        "unref_aspace non-last: A still live"
    );
}

// `unref_aspace` on the last ref: census(A) == 0, refs[A] == 1 (the sole dangling
// reference), so the `-1` reaches zero and `aspace_destroy` fires.
#[test]
fn unref_aspace_last_ref_destroys() {
    let mut st = ArrayStore::new(0);
    st.refs.insert(2, 1);
    check_unref_aspace(&mut st, ObjId(2));
}

// ── Cross-object teardown refcount plumbing: obj_unref / unref_cspace /
//    destroy_cspace, driven on the real ArrayStore bodies. These are differential
//    regression guards (the erasure + the ArrayStore seam); destroy_cspace's loop and
//    the nested cross-object recursion through `delete` are exercised at runtime. ──

// cap_obj as plain Rust (the spec `cap_obj` is not exec-callable from the harness).
fn cap_obj_of(cap: Cap) -> Option<ObjId> {
    match cap.kind {
        CapKind::Empty | CapKind::Untyped { .. } | CapKind::Frame { .. } => None,
        CapKind::Aspace(o)
        | CapKind::CSpace(o)
        | CapKind::Thread(o, _)
        | CapKind::Channel(o, _)
        | CapKind::Notification(o)
        | CapKind::Timer(o) => Some(o),
    }
}

// Drive `obj_unref`. For a designating cap it must be handed the off-by-one state
// (`refs[o] == census(o) + 1`, sound elsewhere — the caller already cleared o's slot);
// the `-1` restores full soundness, firing the type-specific destructor at zero. For a
// non-designating cap it is a no-op (the store is untouched).
fn check_obj_unref(st: &mut ArrayStore, cap: Cap) {
    assert!(cspace_wf_exec(st), "obj_unref pre: cspace_wf");
    let c0 = count_nonempty_exec(st);
    match cap_obj_of(cap) {
        None => {
            let fp0 = fingerprint(st);
            let refs0 = st.refs.clone();
            obj_unref(st, cap);
            assert_eq!(
                fingerprint(st),
                fp0,
                "obj_unref(non-designating): slots untouched"
            );
            assert_eq!(st.refs, refs0, "obj_unref(non-designating): refs untouched");
        }
        Some(o) => {
            let r0 = st.refs[&o.0];
            assert!(r0 > 0, "obj_unref pre: refs[o] > 0");
            assert_eq!(
                obj_census_exec(st, o) + 1,
                r0,
                "obj_unref pre: off-by-one census at o"
            );
            for (&x, &r) in &st.refs {
                if x != o.0 {
                    assert_eq!(
                        obj_census_exec(st, ObjId(x)),
                        r,
                        "obj_unref pre: sound at every other object"
                    );
                }
            }
            obj_unref(st, cap);
            assert!(
                refcount_sound_exec(st),
                "obj_unref post: refcount_sound restored"
            );
            assert!(
                count_nonempty_exec(st) <= c0,
                "obj_unref post: count_nonempty non-increase"
            );
        }
    }
}

// Drive `unref_cspace` on the off-by-one state: the `-1`, then the at-zero
// `destroy_cspace` (residents emptied) or the plain decrement (cspace survives).
fn check_unref_cspace(st: &mut ArrayStore, cs: ObjId) {
    assert!(cspace_wf_exec(st), "unref_cspace pre: cspace_wf");
    let r0 = st.refs[&cs.0];
    assert!(r0 > 0, "unref_cspace pre: refs[cs] > 0");
    assert_eq!(
        obj_census_exec(st, cs) + 1,
        r0,
        "unref_cspace pre: off-by-one census at cs"
    );
    let residents: Vec<SlotId> = st.cspaces[&cs.0].clone();
    let c0 = count_nonempty_exec(st);
    unref_cspace(st, cs);
    assert!(cspace_wf_exec(st), "unref_cspace post: cspace_wf preserved");
    assert!(
        refcount_sound_exec(st),
        "unref_cspace post: refcount_sound restored"
    );
    assert!(
        count_nonempty_exec(st) <= c0,
        "unref_cspace post: count_nonempty non-increase"
    );
    if r0 == 1 {
        for sid in residents {
            assert!(
                st.at(sid).cap.is_empty(),
                "unref_cspace last ref: every resident emptied"
            );
        }
    }
}

// Drive `destroy_cspace`'s resident loop (the precondition is `refs[cs] == 0`). Every
// resident is emptied (delete + its cross-object recursion), `cspace_wf` and
// `refcount_sound` preserved, the live-slot count non-increasing.
fn check_destroy_cspace(st: &mut ArrayStore, cs: ObjId) {
    assert!(cspace_wf_exec(st), "destroy_cspace pre: cspace_wf");
    assert_eq!(
        st.refs.get(&cs.0).copied().unwrap_or(0),
        0,
        "destroy_cspace pre: refs[cs] == 0"
    );
    let residents: Vec<SlotId> = st.cspaces[&cs.0].clone();
    let (c0, sound0) = (count_nonempty_exec(st), refcount_sound_exec(st));
    destroy_cspace(st, cs);
    assert!(
        cspace_wf_exec(st),
        "destroy_cspace post: cspace_wf preserved"
    );
    for sid in residents {
        assert!(
            st.at(sid).cap.is_empty(),
            "destroy_cspace: every resident emptied"
        );
    }
    assert!(
        count_nonempty_exec(st) <= c0,
        "destroy_cspace: count_nonempty non-increase"
    );
    if sound0 {
        assert!(
            refcount_sound_exec(st),
            "destroy_cspace post: refcount_sound preserved"
        );
    }
}

// destroy_cspace on a cspace whose residents are bare frame caps (no nested objects):
// both residents are deleted, the count drops to zero, the census stays sound.
#[test]
fn destroy_cspace_empties_frame_residents() {
    let mut st = ArrayStore::new(2);
    st.slots[0] = detached(frame_cap(0x1000));
    st.slots[1] = detached(frame_cap(0x2000));
    st.refs.insert(10, 0);
    st.cspaces.insert(10, vec![SlotId(0), SlotId(1)]);
    assert!(refcount_sound_exec(&st), "fixture is refcount_sound");
    check_destroy_cspace(&mut st, ObjId(10));
}

// The nested case: cspace 10's sole resident is the *one* cap to cspace 11, which owns
// its own frame residents. destroy_cspace(10) → delete(the CSpace(11) cap) → obj_unref →
// destroy_cspace(11) → delete 11's residents — the `delete`-recurses path,
// exercised at runtime against the real `delete` body.
#[test]
fn destroy_cspace_nested_recurses_through_delete() {
    let mut st = ArrayStore::new(3);
    st.slots[0] = detached(cspace_cap(11)); // the one cap to cspace 11 (resident of 10)
    st.slots[1] = detached(frame_cap(0x1000));
    st.slots[2] = detached(frame_cap(0x2000));
    st.refs.insert(10, 0);
    st.refs.insert(11, 1);
    st.cspaces.insert(10, vec![SlotId(0)]);
    st.cspaces.insert(11, vec![SlotId(1), SlotId(2)]);
    assert!(refcount_sound_exec(&st), "nested fixture is refcount_sound");
    check_destroy_cspace(&mut st, ObjId(10));
    // The nested cspace's residents were emptied by the cross-object recursion too.
    assert!(st.at(SlotId(1)).cap.is_empty(), "nested resident emptied");
    assert!(st.at(SlotId(2)).cap.is_empty(), "nested resident emptied");
    assert_eq!(st.refs[&11], 0, "nested cspace's last ref dropped to zero");
}

// unref_cspace on a non-last ref: a cap designates cspace 10 (census == 1) and the
// off-by-one bump makes refs == 2, so the `-1` lands at 1 — the cspace survives, its
// resident untouched.
#[test]
fn unref_cspace_non_last_decrements() {
    let mut st = ArrayStore::new(2);
    st.slots[0] = detached(cspace_cap(10)); // a cap designating 10 → census(10) == 1
    st.slots[1] = detached(frame_cap(0x1000)); // a resident of 10
    st.refs.insert(10, 2); // off-by-one: census(10) + 1
    st.cspaces.insert(10, vec![SlotId(1)]);
    assert!(
        !refcount_sound_exec(&st),
        "the off-by-one state is not yet sound at 10"
    );
    check_unref_cspace(&mut st, ObjId(10));
    assert_eq!(st.refs[&10], 1, "unref_cspace non-last: cspace survives");
    assert!(
        !st.at(SlotId(1)).cap.is_empty(),
        "unref_cspace non-last: resident untouched"
    );
}

// unref_cspace on the last ref: census(10) == 0, refs == 1 (the sole holder), so the
// `-1` reaches zero and destroy_cspace empties the resident.
#[test]
fn unref_cspace_last_ref_destroys() {
    let mut st = ArrayStore::new(1);
    st.slots[0] = detached(frame_cap(0x1000)); // a resident of 10
    st.refs.insert(10, 1); // census(10) == 0, off-by-one
    st.cspaces.insert(10, vec![SlotId(0)]);
    check_unref_cspace(&mut st, ObjId(10));
    assert!(
        !st.refs.contains_key(&10) || st.refs[&10] == 0,
        "unref_cspace last ref: cspace torn down"
    );
}

// obj_unref on a CSpace cap, last ref (off-by-one ⇒ census(10) == 0): the dispatch fires
// destroy_cspace, emptying the resident.
#[test]
fn obj_unref_cspace_last_ref_destroys() {
    let mut st = ArrayStore::new(1);
    st.slots[0] = detached(frame_cap(0x1000)); // a resident of 10
    st.refs.insert(10, 1); // census(10) == 0, off-by-one
    st.cspaces.insert(10, vec![SlotId(0)]);
    check_obj_unref(&mut st, cspace_cap(10));
    assert!(
        st.at(SlotId(0)).cap.is_empty(),
        "obj_unref(CSpace) last ref: resident emptied"
    );
}

// obj_unref on a non-designating Frame cap: a pure no-op (the off-by-one bookkeeping
// for the aspace ride is delete's frame-unmap branch, not obj_unref).
#[test]
fn obj_unref_frame_is_noop() {
    let mut st = refcount_sound_fixture();
    check_obj_unref(&mut st, frame_cap(0x9999));
}

// `wait` on a nonzero word consumes it without blocking; the queue and refs are
// untouched — the executable check of `wait`'s consume-path contract.
#[test]
fn wait_consume() {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.refs.insert(100, 1);
    st.notifs.insert(
        100,
        NotifState {
            word: 0b1010,
            wait_head: None,
            wait_tail: None,
        },
    );
    let cur = ObjId(200);
    st.tcbs.insert(
        200,
        TcbState {
            state: ThreadState::Runnable,
            ..tcb_state_default()
        },
    );

    assert_eq!(
        wait(&mut st, n, cur),
        Some(0b1010),
        "a nonzero word is consumed"
    );
    assert_eq!(st.notifs[&100].word, 0, "word cleared");
    assert_eq!(
        st.tcbs[&200].state,
        ThreadState::Runnable,
        "the thread did not block"
    );
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
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    let t1 = ObjId(200);
    let t2 = ObjId(201);
    st.tcbs.insert(
        200,
        TcbState {
            state: ThreadState::Runnable,
            ..tcb_state_default()
        },
    );
    st.tcbs.insert(
        201,
        TcbState {
            state: ThreadState::Runnable,
            ..tcb_state_default()
        },
    );

    // First waiter blocks at the head; acquires a ref.
    assert_eq!(
        wait(&mut st, n, t1),
        None,
        "word 0 ⇒ the first thread blocks"
    );
    assert_eq!(st.tcbs[&200].state, ThreadState::BlockedNotif);
    assert!(st.tcbs[&200].wait_notif == Some(n));
    assert!(st.notifs[&100].wait_head == Some(t1));
    assert!(st.notifs[&100].wait_tail == Some(t1));
    assert_eq!(st.refs[&100], 2, "wait acquired the waiter's ref");
    assert!(notif_wf_exec(&st, n));

    // Second waiter blocks behind the first (FIFO tail), threaded via qnext.
    assert_eq!(
        wait(&mut st, n, t2),
        None,
        "the second thread blocks behind the first"
    );
    assert!(st.notifs[&100].wait_head == Some(t1), "head unchanged");
    assert!(
        st.notifs[&100].wait_tail == Some(t2),
        "tail is the new waiter"
    );
    assert!(st.tcbs[&200].qnext == Some(t2), "t1 → t2 threaded");
    assert_eq!(st.refs[&100], 3);
    assert!(notif_wf_exec(&st, n));

    // First signal wakes the HEAD (t1) — block order — delivering the word; -1 ref.
    signal(&mut st, n, 0b1);
    assert_eq!(
        st.tcbs[&200].state,
        ThreadState::Runnable,
        "the head t1 wakes first"
    );
    assert_eq!(st.tcbs[&200].retval, 0b1, "t1 received the word");
    assert!(st.notifs[&100].wait_head == Some(t2), "t2 is now the head");
    assert_eq!(st.notifs[&100].word, 0, "delivered word cleared");
    assert_eq!(st.refs[&100], 2, "the wake released t1's queued ref");
    assert!(notif_wf_exec(&st, n));

    // Second signal wakes t2, emptying the queue.
    signal(&mut st, n, 0b10);
    assert_eq!(
        st.tcbs[&201].state,
        ThreadState::Runnable,
        "t2 wakes second"
    );
    assert_eq!(st.tcbs[&201].retval, 0b10);
    assert!(st.notifs[&100].wait_head.is_none(), "queue now empty");
    assert!(st.notifs[&100].wait_tail.is_none());
    assert_eq!(st.refs[&100], 1);
    assert!(notif_wf_exec(&st, n));
}

// `destroy_notif` on an empty-queue notification is a no-op.
#[test]
fn destroy_notif_noop() {
    let mut st = ArrayStore::new(0);
    let n = ObjId(100);
    st.refs.insert(100, 0);
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    let before = st.notifs[&100].clone();
    let refs_before = st.refs.clone();
    destroy_notif(&mut st, n);
    assert!(
        st.notifs[&100] == before,
        "destroy_notif leaves the notification untouched"
    );
    assert_eq!(st.refs, refs_before, "destroy_notif touches no refcount");
}

// `remove_waiter` splices a waiter out of the FIFO queue at head / middle / tail and
// is a no-op when `t` is absent — the executable check of the proven splice +
// the per-op refcount delta. Queue 200 → 201 → 202 on notification 100.
#[test]
fn remove_waiter_unlink() {
    let n = ObjId(100);
    let mk = || -> ArrayStore {
        let mut st = ArrayStore::new(0);
        st.refs.insert(100, 4); // a binding ref + three queued waiters
        st.notifs.insert(
            100,
            NotifState {
                word: 0,
                wait_head: Some(ObjId(200)),
                wait_tail: Some(ObjId(202)),
            },
        );
        for (id, nxt) in [
            (200u64, Some(ObjId(201))),
            (201, Some(ObjId(202))),
            (202, None),
        ] {
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
    assert!(
        st.notifs[&100].wait_head == Some(ObjId(200)),
        "head unchanged"
    );
    assert!(
        st.notifs[&100].wait_tail == Some(ObjId(202)),
        "tail unchanged"
    );
    assert!(
        st.tcbs[&200].qnext == Some(ObjId(202)),
        "predecessor re-threaded past 201"
    );

    // Head: 200 unlinked; 201 becomes the head.
    let mut st = mk();
    check_remove_waiter(&mut st, n, ObjId(200), true);
    assert!(
        st.notifs[&100].wait_head == Some(ObjId(201)),
        "201 is the new head"
    );
    assert!(
        st.notifs[&100].wait_tail == Some(ObjId(202)),
        "tail unchanged"
    );

    // Tail: 202 unlinked; tail drops to 201, whose qnext becomes None.
    let mut st = mk();
    check_remove_waiter(&mut st, n, ObjId(202), true);
    assert!(
        st.notifs[&100].wait_head == Some(ObjId(200)),
        "head unchanged"
    );
    assert!(
        st.notifs[&100].wait_tail == Some(ObjId(201)),
        "tail dropped to 201"
    );
    assert!(st.tcbs[&201].qnext.is_none(), "new tail's qnext cleared");

    // Absent: a TCB not on the queue — store unchanged.
    let mut st = mk();
    st.tcbs.insert(
        300,
        TcbState {
            state: ThreadState::Inactive,
            ..tcb_state_default()
        },
    );
    let before = st.notifs[&100].clone();
    check_remove_waiter(&mut st, n, ObjId(300), false);
    assert!(
        st.notifs[&100] == before,
        "absent removal leaves the queue untouched"
    );

    // Single-element queue: removing the sole waiter empties head and tail.
    let mut st = ArrayStore::new(0);
    st.refs.insert(100, 2);
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: Some(ObjId(200)),
            wait_tail: Some(ObjId(200)),
        },
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
    // `binding_notif_wf_exec` mirrors the named invariant the channel ops carry;
    // it is only meaningful if it rejects bindings naming bad notifications.
    let ch = ObjId(7);
    assert!(
        binding_notif_wf_exec(&signal_fixture(false).0, ch),
        "all-unbound bindings are vacuously well-formed"
    );

    // A binding naming the live, well-formed notification 100 ⇒ accepted.
    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().bindings.insert(
        (0, EV_READABLE),
        Binding {
            notif: Some(ObjId(100)),
            bits: 1,
        },
    );
    assert!(
        binding_notif_wf_exec(&st, ch),
        "a binding to a live wf notification is accepted"
    );

    // A binding naming a non-resident notification ⇒ rejected.
    let mut st = signal_fixture(false).0;
    st.chans.get_mut(&7).unwrap().bindings.insert(
        (0, EV_READABLE),
        Binding {
            notif: Some(ObjId(999)),
            bits: 1,
        },
    );
    assert!(
        !binding_notif_wf_exec(&st, ch),
        "a binding to a non-resident notification is rejected"
    );

    // A binding naming a malformed-queue notification (head/tail disagree) ⇒ rejected.
    let mut st = signal_fixture(false).0;
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: Some(ObjId(200)),
            wait_tail: None,
        },
    );
    st.chans.get_mut(&7).unwrap().bindings.insert(
        (0, EV_READABLE),
        Binding {
            notif: Some(ObjId(100)),
            bits: 1,
        },
    );
    assert!(
        !binding_notif_wf_exec(&st, ch),
        "a binding to a malformed notification is rejected"
    );
}

// ── Evidence that a "revoked cap survives" rule framed as "`delete` empties only
//    the deleted slot's CDT subtree" is UNSOUND. These two tests run the real
//    `delete`/`revoke` and show that framing is false under cross-object teardown. ──

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
    assert!(
        st.at(SlotId(1)).cap.is_empty(),
        "resident outside the subtree was emptied"
    );
    assert!(
        st.at(SlotId(2)).cap.is_empty(),
        "resident outside the subtree was emptied"
    );
    // => "delete empties only the deleted subtree" is FALSE.
}

#[test]
fn revoke_can_empty_its_own_root_zombie() {
    // The seL4-zombie: the revoked root `slot 0` is itself a resident of a cspace
    // whose last surviving cap lies in slot 0's OWN subtree (its child slot 1).
    //   cspace 10 residents = [slot 0];  slot 0 (Frame) ── child ──▶ slot 1 = CSpace(10).
    // revoke(slot 0) descends to the leaf slot 1 and deletes it → last ref to
    // cspace 10 → destroy_cspace(10) → deletes its resident slot 0 → the *root* is
    // emptied. So "revoked cap survives" does NOT hold unconditionally, and a
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
    // negative witness: `slot 0` IS homed (a resident of cspace 10), so it fails the
    // non-zombie precondition `!is_homed` that `check_revoke_root_survives` relies on — exactly
    // the shape `revoke`'s conditional root-survival theorem excludes.
    assert!(
        is_homed_exec(&st, SlotId(0)),
        "the zombie root is homed (precondition violated)"
    );

    revoke(&mut st, SlotId(0));

    assert!(
        cspace_wf_exec(&st),
        "revoke preserves cspace_wf (its real guarantee)"
    );
    assert!(
        st.at(SlotId(0)).first_child.is_none(),
        "revoke: subtree empty"
    );
    // The headline: the revoked root itself was emptied by the cross-object
    // teardown — the documented gap, here a concrete witness.
    assert!(
        st.at(SlotId(0)).cap.is_empty(),
        "revoke emptied its own root (zombie)"
    );
}

// `is_homed`'s executable mirror: `x` is some object's internal home handle — a
// cspace resident, a channel ring cap, or a TCB bind slot. Host-checks `revoke`'s non-zombie
// precondition both ways (the zombie root is homed; the survivor root is not).
fn is_homed_exec(st: &ArrayStore, x: SlotId) -> bool {
    st.cspaces.values().any(|slots| slots.contains(&x))
        || st
            .chans
            .values()
            .any(|c| c.ring_cap.values().any(|&s| s == x))
        || st.tcbs.values().any(|t| t.bind_slots.contains(&x))
}

#[test]
fn check_revoke_root_survives() {
    // Non-zombie shape: the revoked root `slot 0` is **un-homed** — no cspace
    // resident, ring cap, or bind slot — so the cross-object teardown the revoke walk fires
    // cannot reach it. slot 0 (Frame) ── child ──▶ slot 1 = CSpace(10) (its last cap); cspace
    // 10's resident is slot 2 (NOT slot 0). revoke(slot 0) deletes slot 1 → destroy_cspace(10)
    // empties slot 2 — a genuine cross-object teardown — yet the un-homed slot 0 survives.
    let mut st = ArrayStore::new(3);
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
    st.slots[2] = detached(frame_cap(2));
    st.refs.insert(10, 1); // slot 1 is the one (and last) cap to cspace 10
    st.cspaces.insert(10, vec![SlotId(2)]); // cspace 10's resident is slot 2, NOT slot 0
    assert!(cspace_wf_exec(&st), "the non-zombie shape is well-formed");
    assert!(
        !is_homed_exec(&st, SlotId(0)),
        "the revoke root is un-homed (the §6e precondition)"
    );

    revoke(&mut st, SlotId(0));

    assert!(cspace_wf_exec(&st), "revoke preserves cspace_wf");
    assert!(
        st.at(SlotId(0)).first_child.is_none(),
        "revoke: subtree empty"
    );
    // The headline: the un-homed revoked root survives the cross-object teardown.
    assert!(
        !st.at(SlotId(0)).cap.is_empty(),
        "revoke: the un-homed root SURVIVES"
    );
    // …and the teardown genuinely crossed objects (cspace 10's resident was emptied).
    assert!(
        st.at(SlotId(2)).cap.is_empty(),
        "the cross-object teardown fired (resident emptied)"
    );
}

#[test]
fn check_revoke_root_survives_homed_external_ref() {
    // The **faithful resident-with-external-reference** shape: the revoked root `slot 0`
    // *is* homed — a resident of cspace 10 — so the conservative `!is_homed` survival
    // theorem does NOT apply (this is exactly the residue case). Yet `slot 0` still
    // survives, because cspace 10 keeps a live reference *outside* `slot 0`'s subtree:
    //   cspace 10 residents = [slot 0];
    //   slot 0 (Frame) ── child ──▶ slot 1 = CSpace(10)   (a cap to 10, IN slot 0's subtree)
    //   slot 2 = CSpace(10)                                (an EXTERNAL un-homed cap to 10)
    //   refs[10] = 2 (slots 1 and 2 both designate cspace 10).
    // revoke(slot 0) deletes the subtree (slot 1) → refs[10] drops 2 → 1, NOT zero, so
    // destroy_cspace(10) never fires and the resident `slot 0` is never emptied. This is the
    // contrapositive of `revoke`'s `ensures`: no homing object of `slot 0` was destroyed
    // (refs[10] stayed ≥ 1, witnessed by the external slot 2), so `slot 0` survived.
    // Contrast `revoke_can_empty_its_own_root_zombie`, where slot 1 is the *only* cap to 10 (no
    // external ref) ⟹ destroy_cspace(10) fires ⟹ the homed root self-empties.
    let mut st = ArrayStore::new(3);
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
    // slot 2: an external, un-homed cap to cspace 10 — keeps refs[10] alive across the revoke.
    st.slots[2] = detached(cspace_cap(10));
    st.refs.insert(10, 2); // slots 1 and 2 both designate cspace 10
    st.cspaces.insert(10, vec![SlotId(0)]); // cspace 10 homes slot 0 as its resident
    assert!(
        cspace_wf_exec(&st),
        "the resident-with-external-ref shape is well-formed"
    );
    // `slot 0` IS homed (a resident of cspace 10) — the conservative theorem excludes it.
    assert!(
        is_homed_exec(&st, SlotId(0)),
        "the revoke root is homed (a cspace resident)"
    );
    // `slot 2` is un-homed (no cspace resident / ring cap / bind slot designates it) and external
    // to slot 0's subtree — the live reference that keeps cspace 10 alive.
    assert!(
        !is_homed_exec(&st, SlotId(2)),
        "the external ref is un-homed (outside the subtree)"
    );

    revoke(&mut st, SlotId(0));

    assert!(cspace_wf_exec(&st), "revoke preserves cspace_wf");
    assert!(
        st.at(SlotId(0)).first_child.is_none(),
        "revoke: subtree empty"
    );
    // The headline: the *homed* revoked root survives because its homing cspace kept a
    // live external reference (no homing object died — the contrapositive of the `ensures`).
    assert!(
        !st.at(SlotId(0)).cap.is_empty(),
        "revoke: the homed root SURVIVES (external ref alive)"
    );
    // The subtree cap to cspace 10 was deleted, but cspace 10 stayed alive via the external ref.
    assert!(
        st.at(SlotId(1)).cap.is_empty(),
        "the subtree cap to cspace 10 was revoked"
    );
    assert!(
        !st.at(SlotId(2)).cap.is_empty(),
        "the external cap to cspace 10 survives the revoke"
    );
    assert_eq!(
        st.refs[&10], 1,
        "cspace 10 keeps the external reference (never destroyed)"
    );
}

#[test]
fn revoke_sees_through_queued_descendant() {
    // **Sees through queues (rev1§3.4).** A cap *queued in an in-flight message* is an
    // ordinary CDT descendant — its ring slot carries the parent edge that `slot_move` (the op
    // `send` uses) inherited from the source — so the real `revoke` walk finds and empties it
    // like any other descendant, with no special case. This drives the **real** `revoke` through
    // a slot that is *both* a CDT child of the target *and* a registered channel ring cap.
    //
    //   slot 0 (Frame, un-homed revoke target) ── child ──▶ slot 1 = ring 0 / idx 0 / cap 0 of
    //   channel 7, in its live window — a genuine cap queued in an in-flight A→B message.
    //
    // The arena holds the channel's 8 ring-cap slots (1..=8); the queued cap lives at slot 1, the
    // other 7 ring slots are empty (a one-cap message; ring 1 idle). revoke(slot 0) descends to
    // the queued ring cap and deletes it: the ring slot is left empty — the rev1§3.4 "receivers
    // must tolerate null cap slots" outcome — while the un-homed target survives.
    let mut st = ArrayStore::new(9);
    st.slots[0] = CapSlot {
        cap: frame_cap(0),
        parent: None,
        first_child: Some(SlotId(1)),
        next_sib: None,
        prev_sib: None,
    };
    // slot 1: the in-flight queued cap — a CDT child of the revoke target AND channel 7's ring cap.
    st.slots[1] = CapSlot {
        cap: frame_cap(5),
        parent: Some(SlotId(0)),
        first_child: None,
        next_sib: None,
        prev_sib: None,
    };
    // Register the depth-1 channel's ring caps: ring 0 / idx 0 / cap 0 is the queued slot 1; the
    // remaining 7 ring slots (2..=8) are empty (a single-cap in-window message, ring 1 idle).
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
    msg_len.insert((0usize, 0u32), 5u16); // a queued A→B message
    msg_len.insert((1usize, 0u32), 0u16);
    st.chans.insert(
        7,
        ChanState {
            depth: 1,
            end_caps: [1, 1],
            head: [0, 0],
            count: [1, 0],
            bindings,
            msg_len,
            ring_cap,
        },
    );

    assert!(
        cspace_wf_exec(&st),
        "the queued-descendant shape is well-formed"
    );
    assert!(
        is_homed_exec(&st, SlotId(1)),
        "slot 1 is a real channel ring cap (genuinely queued)"
    );
    assert!(
        !is_homed_exec(&st, SlotId(0)),
        "the revoke target is un-homed (so it survives)"
    );
    assert!(
        !st.at(SlotId(1)).cap.is_empty(),
        "the queued cap is live before revoke"
    );

    revoke(&mut st, SlotId(0));

    assert!(cspace_wf_exec(&st), "revoke preserves cspace_wf");
    assert!(
        st.at(SlotId(0)).first_child.is_none(),
        "revoke: subtree empty"
    );
    // The headline: the real revoke walk reached *through the queue* and destroyed the in-flight cap.
    assert!(
        st.at(SlotId(1)).cap.is_empty(),
        "revoke sees through the queue: the queued cap is destroyed"
    );
    // The ring_cap handle still points at the now-empty slot — the rev1§3.4 null-cap-slot a receiver tolerates.
    assert!(
        st.chan_ring_cap(ObjId(7), 0, 0, 0) == SlotId(1),
        "the ring handle is unchanged (now null)"
    );
    // The un-homed target survives (no homing object of slot 0 was destroyed).
    assert!(
        !st.at(SlotId(0)).cap.is_empty(),
        "the un-homed revoke target survives"
    );
}

// ── Channel send/recv: the FIFO core, host-differential ─────────────────────
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
        ChanState {
            depth,
            end_caps: [1, 1],
            head: [0, 0],
            count: [0, 0],
            bindings,
            msg_len,
            ring_cap,
        },
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
    assert_eq!(
        send(
            &mut st,
            ch,
            ChanEnd::A,
            &[1u8, 2, 3],
            &[Some(send_cap), None, None, None]
        ),
        Ok(())
    );
    assert!(chan_wf_exec(&st, ch));
    assert_eq!(st.chan(ch).count[0], 1, "A sends on ring 0");
    assert!(
        st.at(send_cap).cap.is_empty(),
        "sender slot emptied (move totality)"
    );
    // msg 2: len 5, no caps.
    assert_eq!(send(&mut st, ch, ChanEnd::A, &[0u8; 5], &[None; 4]), Ok(()));
    assert_eq!(st.chan(ch).count[0], 2);

    // B receives on ring 0 (1 - end_idx(B)); the head (msg 1) comes out first.
    let dest = SlotId((scratch0 + 1) as u64);
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(
            &mut st,
            ch,
            ChanEnd::B,
            &mut buf,
            &[Some(dest), None, None, None]
        ),
        Ok((3, 0b1)),
        "FIFO head delivered first, carrying its cap (mask bit 0)"
    );
    assert!(
        !st.at(dest).cap.is_empty(),
        "cap delivered to the dest slot"
    );
    assert_eq!(st.chan(ch).count[0], 1);
    assert!(chan_wf_exec(&st, ch));
    // msg 2 next, in order.
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]),
        Ok((5, 0)),
        "second message in order"
    );
    assert_eq!(st.chan(ch).count[0], 0);
}

#[test]
fn send_full_and_recv_empty() {
    // recv on an empty ring → Empty (unchanged); fill the depth-1 ring; the next
    // send → Full (unchanged) — the read-only guard frames.
    let (mut st, ch, _) = chan_fixture(1, 0);
    let chans0 = st.chans.clone();
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]),
        Err(ChanError::Empty)
    );
    assert!(st.chans == chans0, "recv Empty: channel unchanged");

    assert_eq!(send(&mut st, ch, ChanEnd::A, &[7u8], &[None; 4]), Ok(()));
    assert_eq!(st.chan(ch).count[0], 1);
    let fp = fingerprint(&st);
    let chans1 = st.chans.clone();
    assert_eq!(
        send(&mut st, ch, ChanEnd::A, &[8u8], &[None; 4]),
        Err(ChanError::Full)
    );
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
    assert_eq!(
        send(
            &mut st,
            ch,
            ChanEnd::A,
            &[1u8],
            &[Some(send_cap), None, None, None]
        ),
        Ok(())
    );
    let fp = fingerprint(&st);
    let chans = st.chans.clone();
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]),
        Err(ChanError::NoCapSlot)
    );
    assert_eq!(fingerprint(&st), fp, "NoCapSlot: arena unchanged");
    assert!(st.chans == chans, "NoCapSlot: message fully queued");
    assert_eq!(st.chan(ch).count[0], 1);
}

#[test]
fn recv_null_slot_tolerance() {
    // A sends a cap; revocation empties the queued ring cap in flight; B's recv
    // delivers it as absent (mask bit clear) — never a panic (rev1§3.4 null slots).
    let (mut st, ch, scratch0) = chan_fixture(1, 2);
    let send_cap = SlotId(scratch0 as u64);
    st.slots[scratch0] = detached(frame_cap(7));
    assert_eq!(
        send(
            &mut st,
            ch,
            ChanEnd::A,
            &[0u8; 3],
            &[Some(send_cap), None, None, None]
        ),
        Ok(())
    );
    // simulate a revoke emptying the queued ring cap (an in-window slot may be empty).
    let rc = st.chan(ch).ring_cap[&(0, 0, 0)];
    st.slots[rc.0 as usize] = CapSlot::empty();
    assert!(chan_wf_exec(&st, ch));
    let dest = SlotId((scratch0 + 1) as u64);
    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(
            &mut st,
            ch,
            ChanEnd::B,
            &mut buf,
            &[Some(dest), None, None, None]
        ),
        Ok((3, 0)),
        "null cap delivered as absent (mask 0), no panic"
    );
    assert!(
        st.at(dest).cap.is_empty(),
        "dest stays empty (nothing moved)"
    );
    assert_eq!(st.chan(ch).count[0], 0, "still dequeued");
}

#[test]
fn recv_installs_exact_caps_and_mask() {
    // Witness for recv's exported receive-half: a multi-cap message (caps in
    // message slots 0 and 2) is received; each arriving cap lands — by exact value —
    // in the dest the caller named (ensures B), the returned mask names exactly those
    // slots, 0b101 (ensures A), and every dequeued queue slot is empty (ensures C).
    let (mut st, ch, scratch0) = chan_fixture(1, 8);
    let s0 = SlotId(scratch0 as u64);
    let s2 = SlotId((scratch0 + 1) as u64);
    let d0 = SlotId((scratch0 + 2) as u64);
    let d2 = SlotId((scratch0 + 3) as u64);
    st.slots[scratch0] = detached(frame_cap(100));
    st.slots[scratch0 + 1] = detached(frame_cap(200));
    assert_eq!(
        send(
            &mut st,
            ch,
            ChanEnd::A,
            &[1u8, 2, 3, 4],
            &[Some(s0), None, Some(s2), None]
        ),
        Ok(())
    );
    assert!(
        st.at(s0).cap.is_empty() && st.at(s2).cap.is_empty(),
        "sources emptied at send"
    );

    let mut buf = [0u8; MSG_PAYLOAD];
    assert_eq!(
        recv(
            &mut st,
            ch,
            ChanEnd::B,
            &mut buf,
            &[Some(d0), None, Some(d2), None]
        ),
        Ok((4, 0b101)),
        "mask names exactly the filled dests (bits 0 and 2)"
    );
    // (B) the exact caps landed where the caller asked — Cap has no PartialEq, so match.
    match st.at(d0).cap.kind {
        CapKind::Frame { base, .. } => assert_eq!(base, 100, "cap 0 moved into d0, by value"),
        _ => panic!("d0 should hold the moved frame cap"),
    }
    match st.at(d2).cap.kind {
        CapKind::Frame { base, .. } => assert_eq!(base, 200, "cap 2 moved into d2, by value"),
        _ => panic!("d2 should hold the moved frame cap"),
    }
    // (C) every cap slot of the dequeued head is empty afterward.
    for c in 0..4usize {
        let rc = st.chan(ch).ring_cap[&(0, 0, c)];
        assert!(st.at(rc).cap.is_empty(), "dequeued head ring slot emptied");
    }
    assert_eq!(st.chan(ch).count[0], 0, "message dequeued");
    assert!(chan_wf_exec(&st, ch));
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
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let do_send = (rng >> 33) & 1 == 0;
            if do_send && model.len() < depth as usize {
                let len = ((rng >> 3) % 200) as u16;
                assert_eq!(
                    send(
                        &mut st,
                        ch,
                        ChanEnd::A,
                        &vec![0u8; len as usize],
                        &[None; 4]
                    ),
                    Ok(())
                );
                model.push_back(len);
                trials += 1;
            } else if !model.is_empty() {
                let mut buf = [0u8; MSG_PAYLOAD];
                let r = recv(&mut st, ch, ChanEnd::B, &mut buf, &[None; 4]);
                assert_eq!(
                    r,
                    Ok((model.pop_front().unwrap() as usize, 0)),
                    "FIFO head len matches model"
                );
                trials += 1;
            }
            assert!(chan_wf_exec(&st, ch), "chan_wf preserved through the sweep");
            assert_eq!(
                st.chan(ch).count[0],
                model.len() as u32,
                "count tracks the model"
            );
        }
    }
    assert!(
        trials > 300,
        "sweep should exercise hundreds of ops, ran {trials}"
    );
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
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    st.chan_mut(ch).bindings.insert(
        (1, EV_PEER_CLOSED),
        Binding {
            notif: Some(n),
            bits: 0b100,
        },
    );
    check_endpoint_cap_dropped(&mut st, ch, ChanEnd::A);
    assert_eq!(st.chan(ch).end_caps, [0, 1]);
    assert_eq!(
        st.notifs[&100].word, 0b100,
        "peer-closed fired into the bound notif"
    );
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
    st.chan_mut(ch).bindings.insert(
        (0, EV_PEER_CLOSED),
        Binding {
            notif: Some(n1),
            bits: 0b1,
        },
    );
    st.chan_mut(ch).bindings.insert(
        (1, EV_READABLE),
        Binding {
            notif: Some(n2),
            bits: 0b1,
        },
    );

    check_destroy_channel(&mut st, ch);

    assert_eq!(st.refs[&100], 2, "peer-closed binding's notif released");
    assert_eq!(st.refs[&101], 2, "readable binding's notif released");
    // the proven body **clears** each binding so `binding_refs` falls in lockstep
    // with the `refs -= 1`.
    assert!(
        st.chan(ch).bindings[&(0, EV_PEER_CLOSED)].notif.is_none(),
        "peer-closed binding cleared"
    );
    assert!(
        st.chan(ch).bindings[&(1, EV_READABLE)].notif.is_none(),
        "readable binding cleared"
    );
}

#[test]
fn destroy_channel_bound_preserves_refcount_sound() {
    // A genuinely `refcount_sound` bound channel: one binding to a live notification whose
    // only reference is that binding, empty rings, no endpoint caps. The proven
    // `destroy_channel` clears the binding and drops `refs[n]` in lockstep, so
    // `check_destroy_channel`'s `refcount_sound` assertion (skipped when the fixture is
    // unsound) actually fires here — the differential check of the proven census contract.
    let (mut st, ch, _) = chan_fixture(1, 0);
    st.chan_mut(ch).end_caps = [0, 0]; // no `Channel(ch,_)` caps ⇒ `end_caps_sound` with empty slots
    let n = ObjId(100);
    st.refs.insert(100, 1); // census(n) == 1: the single binding below
    st.chan_mut(ch).bindings.insert(
        (0, EV_READABLE),
        Binding {
            notif: Some(n),
            bits: 0b1,
        },
    );
    assert!(
        refcount_sound_exec(&st),
        "fixture is refcount_sound (so the post-check fires)"
    );
    assert!(end_caps_sound_exec(&st), "fixture is end_caps_sound");

    check_destroy_channel(&mut st, ch); // asserts refcount_sound preserved (sound0 is true here)

    assert_eq!(st.refs[&100], 0, "the binding's notif ref released");
    assert!(
        st.chan(ch).bindings[&(0, EV_READABLE)].notif.is_none(),
        "binding cleared in lockstep"
    );
}

// ── Timer ───────────────────────────────────────────────────────────────────

// `timer_wf_exec` rejects each malformed armed list (so the timer-op `timer_wf`
// precondition is non-vacuous): a head pointing at a non-timer, an unarmed node on the
// chain, a node armed without a bound notification, a `next` cycle, and an armed timer
// absent from the chain (the completeness violation).
#[test]
fn timer_wf_exec_has_teeth() {
    let armed = |notif: Option<ObjId>, next: Option<ObjId>| TimerState {
        armed: true,
        deadline: 0,
        notif,
        bits: 0,
        next,
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
    unarmed.timers.insert(
        300,
        TimerState {
            armed: false,
            deadline: 0,
            notif: Some(ObjId(100)),
            bits: 0,
            next: None,
        },
    );
    unarmed.timer_armed_head = Some(ObjId(300));
    assert!(!timer_wf_exec(&unarmed), "a charted node must be armed");

    // A charted node has no bound notification.
    let mut no_notif = ArrayStore::new(0);
    no_notif.timers.insert(300, armed(None, None));
    no_notif.timer_armed_head = Some(ObjId(300));
    assert!(
        !timer_wf_exec(&no_notif),
        "a charted node must name a notification"
    );

    // A `next` cycle (300 → 301 → 300).
    let mut cyclic = ArrayStore::new(0);
    cyclic
        .timers
        .insert(300, armed(Some(ObjId(100)), Some(ObjId(301))));
    cyclic
        .timers
        .insert(301, armed(Some(ObjId(100)), Some(ObjId(300))));
    cyclic.timer_armed_head = Some(ObjId(300));
    assert!(!timer_wf_exec(&cyclic), "a cycle is rejected");

    // An armed timer is not on the chain (completeness violation).
    let mut incomplete = ArrayStore::new(0);
    incomplete.timers.insert(300, armed(Some(ObjId(100)), None));
    incomplete.timers.insert(301, armed(Some(ObjId(101)), None)); // armed but unlinked
    incomplete.timer_armed_head = Some(ObjId(300));
    assert!(
        !timer_wf_exec(&incomplete),
        "an off-chain armed timer is rejected"
    );
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
    st.timers.insert(
        300,
        TimerState {
            armed: false,
            deadline: 0,
            notif: None,
            bits: 0,
            next: None,
        },
    );
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
    st.timers.insert(
        301,
        TimerState {
            armed: true,
            deadline: 200,
            notif: Some(ObjId(101)),
            bits: 0b10,
            next: None,
        },
    );
    st.timers.insert(
        300,
        TimerState {
            armed: true,
            deadline: 50,
            notif: Some(ObjId(100)),
            bits: 0b1,
            next: Some(ObjId(301)),
        },
    );
    st.timer_armed_head = Some(ObjId(300));
    // notif 100 with a blocked waiter 400 (so the fire takes the wake path).
    st.tcbs.insert(
        400,
        TcbState {
            state: ThreadState::BlockedNotif,
            wait_notif: Some(ObjId(100)),
            ..tcb_state_default()
        },
    );
    st.notifs.insert(
        100,
        NotifState {
            word: 0,
            wait_head: Some(ObjId(400)),
            wait_tail: Some(ObjId(400)),
        },
    );
    st.refs.insert(100, 2); // the timer's ref + the waiter's ref
                            // notif 101, no waiter.
    st.notifs.insert(
        101,
        NotifState {
            word: 0,
            wait_head: None,
            wait_tail: None,
        },
    );
    st.refs.insert(101, 1); // the timer's ref

    check_check_expired(&mut st, now);

    assert!(!st.timers[&300].armed, "the expired timer is disarmed");
    assert_eq!(
        st.tcbs[&400].state,
        ThreadState::Runnable,
        "its blocked waiter woke"
    );
    assert_eq!(st.tcbs[&400].retval, 0b1, "the timer's bits were delivered");
    assert_eq!(
        st.refs[&100], 0,
        "disarm released the timer ref, the wake the waiter ref"
    );
    assert!(st.timers[&301].armed, "the unexpired timer survives");
    assert!(
        st.timer_armed_head == Some(ObjId(301)),
        "the survivor is now the list head"
    );
    assert_eq!(st.refs[&101], 1, "the untouched notif keeps its ref");
}

// `destroy_timer` of an armed timer (its last cap gone): `disarm`, releasing the notif ref
// and emptying the armed list.
#[test]
fn destroy_timer_disarms() {
    let mut st = ArrayStore::new(0);
    let t = ObjId(300);
    st.timers.insert(
        300,
        TimerState {
            armed: true,
            deadline: 10,
            notif: Some(ObjId(100)),
            bits: 0b1,
            next: None,
        },
    );
    st.timer_armed_head = Some(t);
    st.refs.insert(100, 1);
    st.refs.insert(300, 0); // last cap gone (the destroy_timer precondition)

    check_destroy_timer(&mut st, t);

    assert!(st.timer_armed_head.is_none(), "the armed list is now empty");
    assert_eq!(
        st.refs[&100], 0,
        "destroy_timer released the notif ref via disarm"
    );
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
    // `t` is dead (its last designating cap is gone — `obj_unref` calls `destroy_tcb` only at
    // `refs[t] == 0`), the precondition the `dead_tcb_frozen` frame needs.
    st.refs.insert(200, 0);
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

// `destroy_tcb` on a thread genuinely *on* its ready chain: like `destroy_tcb_structural`,
// but `t` (200) sits between two siblings at a shared level, so the teardown exercises the
// faithful `unqueue_ready` splice (predecessor re-thread; the level stays non-empty so its
// presence bit survives) before the halt promotes `ready_complete_except(t)` back to
// `ready_complete`. The B8C-4 ready-queue half of `check_destroy_tcb`.
#[test]
fn destroy_tcb_splices_out_of_ready_queue() {
    let mut st = ArrayStore::new(2);
    st.slots[0] = detached(notif_cap(50));
    st.slots[1] = detached(notif_cap(51));
    st.refs.insert(50, 2);
    st.refs.insert(51, 2);
    st.refs.insert(200, 0); // dead (last designating cap gone) — the destroy_tcb precondition
    let t = ObjId(200);
    let prio = 5u8;
    st.tcbs.insert(
        200,
        TcbState {
            state: ThreadState::Inactive, // ready_enqueue flips it Runnable
            report: Report::Running,
            priority: prio,
            bind_slots: [SlotId(0), SlotId(1)],
            ..tcb_state_default()
        },
    );
    let a = ObjId(201);
    let b = ObjId(202);
    for id in [201u64, 202] {
        st.tcbs.insert(
            id,
            TcbState {
                state: ThreadState::Inactive,
                priority: prio,
                ..tcb_state_default()
            },
        );
    }
    // chain at `prio`: [a, t, b] — t in the middle.
    crate::ready::ready_enqueue(&mut st, a);
    crate::ready::ready_enqueue(&mut st, t);
    crate::ready::ready_enqueue(&mut st, b);
    assert_eq!(
        ready_ids(&st, prio as usize),
        vec![201, 200, 202],
        "fixture: t between two siblings"
    );
    assert!(
        ready_wf_exec(&st) && ready_complete_exec(&st),
        "fixture is a well-formed, complete ready queue"
    );

    check_destroy_tcb(&mut st, t); // asserts ready_wf + t off every chain (B8C-4 extension)

    // the splice re-threaded a→b; the level stays non-empty with the bit set.
    assert_eq!(
        ready_ids(&st, prio as usize),
        vec![201, 202],
        "siblings re-threaded around t"
    );
    assert!(
        st.tcbs[&a.0].qnext == Some(b),
        "predecessor re-threaded to successor"
    );
    assert!(st.ready_heads[prio as usize] == Some(a));
    assert!(st.ready_tails[prio as usize] == Some(b));
    assert!(
        st.ready_bitmap & (1 << prio) != 0,
        "level still non-empty, presence bit stays"
    );
    assert!(
        ready_complete_exec(&st),
        "survivors stay charted (ready_complete restored after the halt)"
    );
    assert_eq!(st.refs[&50], 1, "EXIT bind cap deleted");
    assert_eq!(st.refs[&51], 1, "FAULT bind cap deleted");
}

// ── Ready queue (B8C): the verified ops over the ArrayStore backing ──────────────

// enqueue spreads threads across two levels; `top_ready` picks the highest non-empty level;
// `ready_dequeue` is FIFO within a level (round-robin) and clears the presence bit as each
// level empties; `ready_wf` (incl. bitmap coherence) holds throughout.
#[test]
fn ready_enqueue_top_dequeue_round_robin() {
    let mut st = ArrayStore::new(0);
    for (id, prio) in [(200u64, 5u8), (201, 5), (202, 5), (203, 9)] {
        st.tcbs.insert(
            id,
            TcbState {
                state: ThreadState::Inactive,
                priority: prio,
                ..tcb_state_default()
            },
        );
    }
    for id in [200u64, 201, 202, 203] {
        crate::ready::ready_enqueue(&mut st, ObjId(id));
        assert!(ready_wf_exec(&st), "ready_wf after enqueue {id}");
    }
    assert_eq!(
        ready_ids(&st, 5),
        vec![200, 201, 202],
        "level-5 chain is tail-append (FIFO) insertion order"
    );

    // top_ready picks the highest non-empty level (9 over 5).
    assert_eq!(crate::ready::top_ready(&st), Some(9), "level 9 outranks level 5");
    assert_eq!(
        crate::ready::ready_dequeue(&mut st, 9).map(|x| x.0),
        Some(203),
        "dequeue level 9 pops its sole thread"
    );
    assert!(ready_wf_exec(&st));
    assert_eq!(st.ready_bitmap & (1 << 9), 0, "emptied level 9 clears its bit");
    assert_eq!(crate::ready::top_ready(&st), Some(5), "now level 5 is highest");

    // round-robin within level 5: FIFO 200, 201, 202.
    assert_eq!(crate::ready::ready_dequeue(&mut st, 5).map(|x| x.0), Some(200));
    assert_eq!(crate::ready::ready_dequeue(&mut st, 5).map(|x| x.0), Some(201));
    assert!(ready_wf_exec(&st));
    assert!(
        st.ready_bitmap & (1 << 5) != 0,
        "level 5 still non-empty (one thread left)"
    );
    assert_eq!(crate::ready::ready_dequeue(&mut st, 5).map(|x| x.0), Some(202));

    // fully drained.
    assert!(
        crate::ready::ready_dequeue(&mut st, 5).is_none(),
        "an empty level yields None"
    );
    assert_eq!(crate::ready::top_ready(&st), None, "empty queue: no ready thread");
    assert_eq!(st.ready_bitmap, 0, "all presence bits clear");
    assert!(ready_wf_exec(&st));
    // dequeued threads are left Runnable-and-off-chain (maybe_switch sets them Running next).
    for id in [200u64, 201, 202, 203] {
        assert!(st.tcbs[&id].qnext.is_none(), "{id} qnext cleared on dequeue");
        assert_eq!(
            st.tcbs[&id].state,
            ThreadState::Runnable,
            "{id} stays Runnable post-dequeue"
        );
    }
}

// `ready_unqueue` splices a thread out from any position — head, middle, tail, or sole —
// re-threading the predecessor and clearing the presence bit only when the level empties.
#[test]
fn ready_unqueue_splices_arbitrary_position() {
    let level = 7usize;
    let build = |ids: &[u64]| -> ArrayStore {
        let mut st = ArrayStore::new(0);
        for &id in ids {
            st.tcbs.insert(
                id,
                TcbState {
                    state: ThreadState::Inactive,
                    priority: level as u8,
                    ..tcb_state_default()
                },
            );
        }
        for &id in ids {
            crate::ready::ready_enqueue(&mut st, ObjId(id));
        }
        st
    };

    // middle.
    let mut st = build(&[10, 11, 12]);
    crate::ready::ready_unqueue(&mut st, ObjId(11));
    assert!(ready_wf_exec(&st));
    assert_eq!(ready_ids(&st, level), vec![10, 12], "middle node removed");
    assert!(st.tcbs[&11].qnext.is_none(), "spliced node's qnext cleared");
    assert!(
        st.tcbs[&10].qnext == Some(ObjId(12)),
        "predecessor re-threaded past the removed node"
    );
    assert!(st.ready_bitmap & (1 << level) != 0, "non-empty level keeps its bit");

    // head.
    let mut st = build(&[10, 11, 12]);
    crate::ready::ready_unqueue(&mut st, ObjId(10));
    assert!(ready_wf_exec(&st));
    assert_eq!(ready_ids(&st, level), vec![11, 12], "head removed");
    assert!(st.ready_heads[level] == Some(ObjId(11)), "head advanced");

    // tail.
    let mut st = build(&[10, 11, 12]);
    crate::ready::ready_unqueue(&mut st, ObjId(12));
    assert!(ready_wf_exec(&st));
    assert_eq!(ready_ids(&st, level), vec![10, 11], "tail removed");
    assert!(
        st.ready_tails[level] == Some(ObjId(11)),
        "tail fixed up to the predecessor"
    );

    // sole node — the level empties and the bit clears.
    let mut st = build(&[10]);
    crate::ready::ready_unqueue(&mut st, ObjId(10));
    assert!(ready_wf_exec(&st));
    assert!(ready_ids(&st, level).is_empty(), "sole node removed");
    assert_eq!(
        st.ready_bitmap & (1 << level),
        0,
        "emptied level clears its bit"
    );
    assert!(st.ready_heads[level].is_none() && st.ready_tails[level].is_none());
}

// A randomized op sequence — enqueue / unqueue / dequeue over a thread pool spread across
// priority levels — asserting `ready_wf` (incl. bitmap coherence) and `ready_complete` after
// every op. The executable counterpart of the per-op ready-queue proofs across a sequence
// (the ready-queue analogue of `randomized_fifo_sweep`). The model keeps "Runnable iff on a
// chain" by recycling a removed thread's state to Inactive, so `ready_complete` is meaningful.
#[test]
fn randomized_ready_sweep() {
    let pool: [(u64, u8); 10] = [
        (100, 0),
        (101, 0),
        (102, 5),
        (103, 5),
        (104, 5),
        (105, 9),
        (106, 9),
        (107, 31),
        (108, 3),
        (109, 9),
    ];
    let mut trials = 0usize;
    let (mut saw_enq, mut saw_unq, mut saw_deq) = (0usize, 0usize, 0usize);
    for seed in 0..400u64 {
        let mut st = ArrayStore::new(0);
        for &(id, prio) in pool.iter() {
            st.tcbs.insert(
                id,
                TcbState {
                    state: ThreadState::Inactive,
                    priority: prio,
                    ..tcb_state_default()
                },
            );
        }
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        for _ in 0..30 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let r = rng >> 33;
            // off-queue (not Runnable) vs on-chain (Runnable) — the model keeps these in sync
            // with chain membership by recycling removed threads' state below.
            let off: Vec<u64> = pool
                .iter()
                .map(|&(id, _)| id)
                .filter(|id| st.tcbs[id].state != ThreadState::Runnable)
                .collect();
            let on: Vec<u64> = pool
                .iter()
                .map(|&(id, _)| id)
                .filter(|id| st.tcbs[id].state == ThreadState::Runnable)
                .collect();
            match r % 3 {
                0 => {
                    if !off.is_empty() {
                        let id = off[(r as usize / 3) % off.len()];
                        crate::ready::ready_enqueue(&mut st, ObjId(id));
                        saw_enq += 1;
                    }
                }
                1 => {
                    if !on.is_empty() {
                        let id = on[(r as usize / 3) % on.len()];
                        crate::ready::ready_unqueue(&mut st, ObjId(id));
                        st.tcbs.get_mut(&id).unwrap().state = ThreadState::Inactive;
                        saw_unq += 1;
                    }
                }
                _ => {
                    let nonempty: Vec<usize> = (0..crate::sysabi::NUM_PRIOS)
                        .filter(|&l| !ready_seq_exec(&st, l).is_empty())
                        .collect();
                    if !nonempty.is_empty() {
                        let lvl = nonempty[(r as usize / 3) % nonempty.len()];
                        if let Some(popped) = crate::ready::ready_dequeue(&mut st, lvl) {
                            st.tcbs.get_mut(&popped.0).unwrap().state = ThreadState::Inactive;
                            saw_deq += 1;
                        }
                    }
                }
            }
            assert!(ready_wf_exec(&st), "ready_wf after op (seed {seed})");
            assert!(ready_complete_exec(&st), "ready_complete after op (seed {seed})");
            trials += 1;
        }
    }
    assert!(trials > 5000, "sweep ran {trials} trials");
    assert!(
        saw_enq > 0 && saw_unq > 0 && saw_deq > 0,
        "every op exercised: enq={saw_enq} unq={saw_unq} deq={saw_deq}"
    );
}

// `ready_wf_exec` must reject each way the structural ready-queue invariant can break, else
// the ready-queue checks above are vacuous (the `*_exec_has_teeth` discipline).
#[test]
fn ready_wf_exec_has_teeth() {
    let level = 5usize;
    let build = |ids: &[u64]| -> ArrayStore {
        let mut st = ArrayStore::new(0);
        for &id in ids {
            st.tcbs.insert(
                id,
                TcbState {
                    state: ThreadState::Inactive,
                    priority: level as u8,
                    ..tcb_state_default()
                },
            );
        }
        for &id in ids {
            crate::ready::ready_enqueue(&mut st, ObjId(id));
        }
        st
    };
    assert!(ready_wf_exec(&build(&[10, 11])), "a well-formed ready queue is accepted");

    // bit set on an empty level.
    let mut st = build(&[]);
    st.ready_bitmap |= 1 << level;
    assert!(!ready_wf_exec(&st), "presence bit set on an empty level rejected");

    // bit clear on a non-empty level.
    let mut st = build(&[10]);
    st.ready_bitmap &= !(1 << level);
    assert!(!ready_wf_exec(&st), "presence bit clear on a non-empty level rejected");

    // a charted node not Runnable.
    let mut st = build(&[10, 11]);
    st.tcbs.get_mut(&10).unwrap().state = ThreadState::Inactive;
    assert!(!ready_wf_exec(&st), "non-Runnable charted node rejected");

    // a charted node at the wrong level.
    let mut st = build(&[10, 11]);
    st.tcbs.get_mut(&10).unwrap().priority = level as u8 + 1;
    assert!(!ready_wf_exec(&st), "charted node with mismatched priority rejected");

    // tail disagreeing with the chain end.
    let mut st = build(&[10, 11]);
    st.ready_tails[level] = Some(ObjId(10));
    assert!(!ready_wf_exec(&st), "tail not the last node rejected");

    // a cycle (qnext loops back to the head).
    let mut st = build(&[10, 11]);
    st.tcbs.get_mut(&11).unwrap().qnext = Some(ObjId(10));
    assert!(!ready_wf_exec(&st), "cyclic chain rejected");

    // head/tail None disagreement.
    let mut st = build(&[10]);
    st.ready_heads[level] = None;
    assert!(!ready_wf_exec(&st), "head None with tail Some rejected");
}

// `ready_complete_exec` must reject a Runnable thread that is off every chain, else the
// completeness assertions in the sweep / `check_destroy_tcb` are vacuous.
#[test]
fn ready_complete_exec_has_teeth() {
    let level = 4usize;
    let mut st = ArrayStore::new(0);
    st.tcbs.insert(
        10,
        TcbState {
            state: ThreadState::Inactive,
            priority: level as u8,
            ..tcb_state_default()
        },
    );
    crate::ready::ready_enqueue(&mut st, ObjId(10));
    assert!(
        ready_complete_exec(&st),
        "a charted Runnable thread satisfies completeness"
    );
    // a Runnable thread sitting on no chain breaks completeness.
    st.tcbs.insert(
        11,
        TcbState {
            state: ThreadState::Runnable,
            priority: level as u8,
            ..tcb_state_default()
        },
    );
    assert!(
        !ready_complete_exec(&st),
        "an off-chain Runnable thread is rejected"
    );
}

// ── aspace `map_in`: the verified two-pass walk-allocate over arrays ─────────────
//
// `map_in` is generic over `Store`; these checks run the **real** body against
// hand-built `[u64; 512]` / `Vec<[u64; 512]>` page tables, using `ArrayStore`
// purely as the `barrier_after_map` supplier. The post-map state is checked via
// the verified read-only walker (`range_mapped_in`/`lookup`) — the executable
// counterpart of the `pt_lookup` round-trip.

use crate::aspace::{
    lookup, map_in, pte_encode, range_mapped_in, unmap_in, MapError, PAGE, PERM_W, USER_VA_BASE,
    USER_VA_END,
};

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
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 1, PERM_W, &mut store,
    )
    .unwrap();
    assert!(
        range_mapped_in(&l1, &pool, base, va, PAGE, true),
        "mapped writable"
    );
    let (l3, e) = lookup(&l1, &pool, base, va).expect("present");
    assert_eq!(
        pool[l3][e],
        pte_encode(pa, PERM_W),
        "leaf is pte_encode(pa, W)"
    );
    assert_eq!(used, 2, "one L2 + one L3 table allocated");
}

#[test]
fn map_in_multi_page() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE, 0x4800_0000u64);
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 4, PERM_W, &mut store,
    )
    .unwrap();
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
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 2, PERM_W, &mut store,
    )
    .unwrap();
    assert!(range_mapped_in(&l1, &pool, base, va, 2 * PAGE, true));
    assert_eq!(used, 3, "one L2 + two L3 tables (the L2 carry)");
}

#[test]
fn map_in_already_mapped_atomic() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let va = USER_VA_BASE;
    let pa1 = 0x4800_0000u64;
    map_in(
        &mut l1, &mut pool, &mut used, base, pa1, va, 4, PERM_W, &mut store,
    )
    .unwrap();
    // Try to map pages 2..6 with a *different* PA: page 2 overlaps → AlreadyMapped.
    let pa2 = 0x4A00_0000u64;
    let r = map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        pa2,
        va + 2 * PAGE,
        4,
        PERM_W,
        &mut store,
    );
    assert_eq!(r, Err(MapError::AlreadyMapped));
    // Atomic: no leaf of the second request was written — pages 4/5 stay unmapped…
    assert!(
        !range_mapped_in(&l1, &pool, base, va + 4 * PAGE, 2 * PAGE, false),
        "no partial write"
    );
    // …and the overlapped page keeps pa1's PTE (not overwritten with pa2's).
    let (l3, e) = lookup(&l1, &pool, base, va + 2 * PAGE).expect("present");
    assert_eq!(
        pool[l3][e],
        pte_encode(pa1 + 2 * PAGE, PERM_W),
        "original mapping intact"
    );
}

#[test]
fn map_in_need_memory() {
    // A pool one table short of the L2+L3 a single page needs → NeedMemory.
    let (mut l1, mut pool, mut used, base) = map_fixture(1);
    let mut store = ArrayStore::new(0);
    let r = map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        0x4800_0000,
        USER_VA_BASE,
        1,
        PERM_W,
        &mut store,
    );
    assert_eq!(r, Err(MapError::NeedMemory));
    assert!(
        !range_mapped_in(&l1, &pool, base, USER_VA_BASE, PAGE, false),
        "nothing mapped"
    );
}

#[test]
fn map_in_readonly_rejects_write() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let va = USER_VA_BASE;
    map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        0x4800_0000,
        va,
        1,
        0, /* RO */
        &mut store,
    )
    .unwrap();
    assert!(
        range_mapped_in(&l1, &pool, base, va, PAGE, false),
        "present for reads"
    );
    assert!(
        !range_mapped_in(&l1, &pool, base, va, PAGE, true),
        "rejected for writes"
    );
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
            match map_in(
                &mut l1, &mut pool, &mut used, base, pa, va, pages, perms, &mut store,
            ) {
                Ok(()) => {
                    assert!(range_mapped_in(
                        &l1,
                        &pool,
                        base,
                        va,
                        pages * PAGE,
                        perms & PERM_W != 0
                    ));
                    for i in 0..pages {
                        let (l3, e) =
                            lookup(&l1, &pool, base, va + i * PAGE).expect("new range present");
                        assert_eq!(pool[l3][e], pte_encode(pa + i * PAGE, perms));
                    }
                    for &(mva, mpages, mpa, mperms) in &mapped {
                        for i in 0..mpages {
                            let (l3, e) = lookup(&l1, &pool, base, mva + i * PAGE)
                                .expect("prior range intact");
                            assert_eq!(
                                pool[l3][e],
                                pte_encode(mpa + i * PAGE, mperms),
                                "no clobber"
                            );
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
    assert!(
        trials > 300,
        "sweep should map hundreds of ranges, ran {trials}"
    );
}

// ── aspace `unmap_in`: the verified leaf-clear + TLBI effect-log ─────────────
//
// `unmap_in` runs the **real** body against the same hand-built arrays, driving
// the per-page TLBI through `ArrayStore`'s real `tlb_log`. The host checks assert
// both halves of the contract: the pages are cleared (and others framed,
// `range_mapped_in`/`lookup`) and the TLBI log equals the expected `(asid, va)`
// sequence, in ascending order — the executable counterpart of the ordering
// theorem (`unmap_log`).

const TEST_ASID: u16 = 7;

#[test]
fn unmap_clears_and_logs() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE, 0x4800_0000u64);
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 4, PERM_W, &mut store,
    )
    .unwrap();
    assert!(
        range_mapped_in(&l1, &pool, base, va, 4 * PAGE, false),
        "mapped before unmap"
    );

    unmap_in(&l1, &mut pool, base, TEST_ASID, va, 4, &mut store);
    // Every page in the range is now unmapped…
    assert!(
        !range_mapped_in(&l1, &pool, base, va, 4 * PAGE, false),
        "range cleared"
    );
    for i in 0..4u64 {
        let (l3, e) = lookup(&l1, &pool, base, va + i * PAGE).expect("L3 table still present");
        assert_eq!(pool[l3][e], 0, "leaf {i} zeroed");
    }
    // …and one TLBI per cleared page, in ascending order, then a (no-op) barrier.
    let expect: Vec<(u16, u64)> = (0..4).map(|i| (TEST_ASID, va + i * PAGE)).collect();
    assert_eq!(store.tlb_log, expect, "one TLBI per cleared page, in order");
}

#[test]
fn unmap_absent_l3_region_no_tlbi() {
    // `unmap_in` skips a page only when its whole L3 table is absent — the skip is
    // per-L3-table (2 MiB), not per-leaf (`lookup` is `Some` for any present chain,
    // even a zero leaf). Map one page in region 0, then unmap a *different* 2 MiB
    // region (l2 slot 1, no L3 table) — no panic, no TLBI.
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        0x4800_0000,
        USER_VA_BASE,
        1,
        PERM_W,
        &mut store,
    )
    .unwrap();
    store.tlb_log.clear();
    // USER_VA_BASE + 512*PAGE is the next 2 MiB region — no L2/L3 there.
    unmap_in(
        &l1,
        &mut pool,
        base,
        TEST_ASID,
        USER_VA_BASE + 512 * PAGE,
        4,
        &mut store,
    );
    assert!(store.tlb_log.is_empty(), "absent L3 region ⇒ no TLBI");
    // The unrelated mapping is untouched (the frame).
    assert!(
        range_mapped_in(&l1, &pool, base, USER_VA_BASE, PAGE, false),
        "other mapping intact"
    );
}

#[test]
fn unmap_skips_absent_l3_at_region_boundary() {
    // A range straddling the 2 MiB L3 boundary: the present half is cleared + TLBI'd,
    // the absent half is skipped (the genuine per-L3 skip). Page 0's mapping creates
    // the L3 table for region 0 (pages 0..511); pages 512.. live in the absent region 1.
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        0x4800_0000,
        USER_VA_BASE,
        1,
        PERM_W,
        &mut store,
    )
    .unwrap();
    store.tlb_log.clear();
    // [base+510*PAGE, base+514*PAGE): pages 510,511 (region 0, present) + 512,513 (region 1, absent).
    unmap_in(
        &l1,
        &mut pool,
        base,
        TEST_ASID,
        USER_VA_BASE + 510 * PAGE,
        4,
        &mut store,
    );
    assert_eq!(
        store.tlb_log,
        vec![
            (TEST_ASID, USER_VA_BASE + 510 * PAGE),
            (TEST_ASID, USER_VA_BASE + 511 * PAGE)
        ],
        "only the present-L3 pages are TLBI'd; the absent region is skipped"
    );
}

#[test]
fn unmap_partial_overlap_frames_the_rest() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE, 0x4800_0000u64);
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 4, PERM_W, &mut store,
    )
    .unwrap();
    store.tlb_log.clear();
    // Unmap only the middle two pages (1..3).
    unmap_in(&l1, &mut pool, base, TEST_ASID, va + PAGE, 2, &mut store);
    // Pages 1,2 gone; pages 0,3 keep their exact PTEs (the frame).
    assert!(
        !range_mapped_in(&l1, &pool, base, va + PAGE, 2 * PAGE, false),
        "middle cleared"
    );
    let (l3, e) = lookup(&l1, &pool, base, va).expect("present");
    assert_eq!(pool[l3][e], pte_encode(pa, PERM_W), "page 0 framed");
    let (l3, e) = lookup(&l1, &pool, base, va + 3 * PAGE).expect("present");
    assert_eq!(
        pool[l3][e],
        pte_encode(pa + 3 * PAGE, PERM_W),
        "page 3 framed"
    );
    assert_eq!(
        store.tlb_log,
        vec![(TEST_ASID, va + PAGE), (TEST_ASID, va + 2 * PAGE)]
    );
}

#[test]
fn unmap_tlbis_present_l3_including_zero_leaves() {
    // Two disjoint single pages (a hole between them) share one L3 table, so the
    // whole 4-page span has a present chain. `unmap_in` TLBIs every page in a
    // present L3 region — the holes (zero leaves) included — one per page, in order.
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let va = USER_VA_BASE;
    map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        0x4800_0000,
        va,
        1,
        PERM_W,
        &mut store,
    )
    .unwrap();
    map_in(
        &mut l1,
        &mut pool,
        &mut used,
        base,
        0x4900_0000,
        va + 2 * PAGE,
        1,
        PERM_W,
        &mut store,
    )
    .unwrap();
    store.tlb_log.clear();
    unmap_in(&l1, &mut pool, base, TEST_ASID, va, 4, &mut store);
    assert!(!range_mapped_in(&l1, &pool, base, va, 4 * PAGE, false));
    let expect: Vec<(u16, u64)> = (0..4).map(|i| (TEST_ASID, va + i * PAGE)).collect();
    assert_eq!(
        store.tlb_log, expect,
        "one TLBI per page of the present L3, in order"
    );
}

#[test]
fn map_unmap_remap_roundtrip() {
    let (mut l1, mut pool, mut used, base) = map_fixture(8);
    let mut store = ArrayStore::new(0);
    let (va, pa) = (USER_VA_BASE, 0x4800_0000u64);
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 3, PERM_W, &mut store,
    )
    .unwrap();
    unmap_in(&l1, &mut pool, base, TEST_ASID, va, 3, &mut store);
    assert!(
        !range_mapped_in(&l1, &pool, base, va, 3 * PAGE, false),
        "unmapped"
    );
    // The cleared leaves are reusable: a fresh map of the same range succeeds.
    map_in(
        &mut l1, &mut pool, &mut used, base, pa, va, 3, PERM_W, &mut store,
    )
    .unwrap();
    assert!(
        range_mapped_in(&l1, &pool, base, va, 3 * PAGE, true),
        "remapped writable"
    );
}
