//! The `World`: harness-owned kernel objects with real provenance (plan §4,
//! §2.2 rule 2). Every object lives in an ordinary Rust allocation, so the
//! pointers CBMC and Miri see carry provenance — no integer→pointer casts.
//! The `repr(C)` pools reproduce the trailing-inline-array layout that
//! `CSpaceObj::slot` and `Channel::slot` address (`this.add(1)`), exactly as
//! `kernel/src/main.rs`'s `RootCSpace` does in production; a layout
//! assertion in the builders pins it.
//!
//! Bounds come from [`super::bounds`] (the TLC scope). Builders here are
//! deterministic; the nondet shape builders the Kani harnesses drive sit
//! alongside them (plan §4.1).

use super::bounds::*;
use super::ghost::GhostEnv;
use crate::aspace::AspaceObj;
use crate::channel::{Binding, Channel, MsgSlot, MSG_CAPS, MSG_PAYLOAD};
use crate::cspace::{CapSlot, CSpaceObj, ObjHeader};
use crate::notification::NotifObj;
use crate::thread::Tcb;
use crate::timer::TimerObj;
use core::ptr;

// ── const "empty object" constructors (proofs see the pub(crate) fields) ──

const fn empty_channel() -> Channel {
    Channel {
        hdr: ObjHeader { refs: 0 },
        depth: CHAN_DEPTH,
        end_caps: [0, 0],
        head: [0, 0],
        count: [0, 0],
        bindings: [[Binding { notif: ptr::null_mut(), bits: 0 }; 3]; 2],
    }
}

const fn empty_msgslot() -> MsgSlot {
    MsgSlot {
        len: 0,
        payload: [0u8; MSG_PAYLOAD],
        caps: [const { CapSlot::empty() }; MSG_CAPS],
    }
}

const fn empty_notif() -> NotifObj {
    NotifObj {
        hdr: ObjHeader { refs: 0 },
        word: 0,
        wait_head: ptr::null_mut(),
        wait_tail: ptr::null_mut(),
    }
}

const fn empty_timer() -> TimerObj {
    TimerObj {
        hdr: ObjHeader { refs: 0 },
        armed: false,
        deadline: 0,
        notif: ptr::null_mut(),
        bits: 0,
        next: ptr::null_mut(),
    }
}

const fn empty_aspace() -> AspaceObj {
    AspaceObj {
        hdr: ObjHeader { refs: 0 },
        asid: 0,
        l1: 0,
        pool_base: 0,
        pool_pages: 0,
        pool_used: 0,
    }
}

// ── object pools: header + its inline slot array, repr(C) ────────────────

#[repr(C)]
pub struct CSpacePool {
    pub obj: CSpaceObj,
    pub slots: [CapSlot; CS_SLOTS as usize],
}

impl CSpacePool {
    pub const fn new() -> CSpacePool {
        CSpacePool {
            obj: CSpaceObj { hdr: ObjHeader { refs: 0 }, num_slots: CS_SLOTS },
            slots: [const { CapSlot::empty() }; CS_SLOTS as usize],
        }
    }
}

#[repr(C)]
pub struct ChannelPool {
    pub ch: Channel,
    pub slots: [MsgSlot; 2 * CHAN_DEPTH as usize],
}

impl ChannelPool {
    pub const fn new() -> ChannelPool {
        ChannelPool {
            ch: empty_channel(),
            slots: [const { empty_msgslot() }; 2 * CHAN_DEPTH as usize],
        }
    }
}

// ── the world ────────────────────────────────────────────────────────────

pub struct World {
    pub cspaces: [CSpacePool; NCSPACES],
    pub chan: ChannelPool,
    pub tcbs: [Tcb; NTHREADS],
    pub notifs: [NotifObj; NNOTIFS],
    pub timers: [TimerObj; NTIMERS],
    pub aspaces: [AspaceObj; NASPACES],
    pub env: GhostEnv,
}

impl World {
    /// A blank canvas: every object present but unreferenced. `Tcb::empty`
    /// hardcodes `refs = 1` (the creator-cap assumption); since no cap points
    /// anywhere yet, normalize every refcount to its census (here, zero) so
    /// the world starts refcount-sound. Builders place caps and call
    /// [`super::wf::recompute_refs`]; the real ops maintain refs thereafter.
    pub fn new() -> World {
        let mut w = World {
            cspaces: [const { CSpacePool::new() }; NCSPACES],
            chan: ChannelPool::new(),
            tcbs: [const { Tcb::empty() }; NTHREADS],
            notifs: [const { empty_notif() }; NNOTIFS],
            timers: [const { empty_timer() }; NTIMERS],
            aspaces: [const { empty_aspace() }; NASPACES],
            env: GhostEnv::new(),
        };
        unsafe { super::wf::recompute_refs(&mut w) };
        w
    }

    // object pointers (provenance-carrying)
    pub fn cspace(&mut self, i: usize) -> *mut CSpaceObj {
        ptr::addr_of_mut!(self.cspaces[i].obj)
    }
    pub fn channel(&mut self) -> *mut Channel {
        ptr::addr_of_mut!(self.chan.ch)
    }
    pub fn tcb(&mut self, i: usize) -> *mut Tcb {
        ptr::addr_of_mut!(self.tcbs[i])
    }
    pub fn notif(&mut self, i: usize) -> *mut NotifObj {
        ptr::addr_of_mut!(self.notifs[i])
    }
    pub fn timer(&mut self, i: usize) -> *mut TimerObj {
        ptr::addr_of_mut!(self.timers[i])
    }
    pub fn aspace(&mut self, i: usize) -> *mut AspaceObj {
        ptr::addr_of_mut!(self.aspaces[i])
    }

    /// Slot `i` of cspace `cs`, through the real `CSpaceObj::slot` indexing —
    /// the same pointer the kernel computes.
    pub fn cspace_slot(&mut self, cs: usize, i: u32) -> *mut CapSlot {
        unsafe { CSpaceObj::slot(self.cspace(cs), i) }
    }

    /// A channel ring cap slot: ring ∈ {0,1}, message index `i`, cap `c`.
    pub fn ring_cap(&mut self, ring: usize, i: u32, c: usize) -> *mut CapSlot {
        let ch = self.channel();
        unsafe { ptr::addr_of_mut!((*Channel::slot(ch, ring, i)).caps[c]) }
    }

    /// A TCB binding slot (`which` ∈ {BIND_EXIT, BIND_FAULT}).
    pub fn bind_slot(&mut self, t: usize, which: usize) -> *mut CapSlot {
        let tcb = self.tcb(t);
        unsafe { ptr::addr_of_mut!((*tcb).bind_slots[which]) }
    }

    /// The full slot universe (plan §4.1): every cspace slot, every channel
    /// ring cap slot, every TCB binding slot — what `cdt_wf` and the
    /// refcount census range over.
    pub fn collect_slots(&mut self) -> [*mut CapSlot; TOTAL_SLOTS] {
        let mut out = [ptr::null_mut(); TOTAL_SLOTS];
        let mut k = 0;
        for cs in 0..NCSPACES {
            for i in 0..CS_SLOTS {
                out[k] = self.cspace_slot(cs, i);
                k += 1;
            }
        }
        for ring in 0..2 {
            for i in 0..CHAN_DEPTH {
                for c in 0..MSG_CAPS {
                    out[k] = self.ring_cap(ring, i, c);
                    k += 1;
                }
            }
        }
        for t in 0..NTHREADS {
            for b in 0..2 {
                out[k] = self.bind_slot(t, b);
                k += 1;
            }
        }
        debug_assert_eq!(k, TOTAL_SLOTS);
        out
    }

    /// Layout sanity (plan §4.1): the inline-array trick must place each
    /// header's slot 0 exactly where `*::slot` computes it. Call once in a
    /// builder/test; a mismatch means a `repr(C)` or size assumption broke.
    pub fn assert_layout(&mut self) {
        let cs = self.cspace(0);
        assert_eq!(
            self.cspace_slot(0, 0),
            ptr::addr_of_mut!(self.cspaces[0].slots[0]),
            "CSpaceObj::slot must address the inline slot array"
        );
        let _ = cs;
        assert_eq!(
            self.ring_cap(0, 0, 0),
            ptr::addr_of_mut!(self.chan.slots[0].caps[0]),
            "Channel::slot must address the inline ring array"
        );
    }
}

impl Default for World {
    fn default() -> World {
        World::new()
    }
}
