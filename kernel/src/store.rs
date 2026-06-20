//! `KernelStore`: the production [`kcore::store::Store`] resolver — the one
//! sanctioned handle→pointer boundary. A production `ObjId`/`SlotId` *is* the
//! object's/slot's live address; every accessor casts it back and reads/writes
//! the field. Zero-sized, so the kcore object machinery monomorphizes against it
//! with no indirection.
//!
//! This is the trusted base, exactly as the TLB/MMIO asm is: kcore is verified
//! against the *array-backed* `Store` and runs against this address-backed one.
//! The `unsafe` int→pointer casts live here, never in kcore.
//!
//! Concurrency invariant (unchanged): single-core, non-preemptible (IRQs masked
//! at EL1), so whoever runs kernel code has exclusive access to every object.

use core::ptr;
use kcore::aspace::AspaceObj;
use kcore::channel::{Channel, MSG_PAYLOAD};
use kcore::cspace::{CSpaceObj, CapSlot, ObjHeader};
use kcore::id::{ObjId, SlotId};
use kcore::notification::NotifObj;
use kcore::store::{Binding, Store};
use kcore::thread::{Report, Tcb, ThreadState};
use kcore::timer::TimerObj;

/// The production object store: handles are live addresses.
pub struct KernelStore;

// Handle ⇄ address helpers (the trusted boundary). `ObjId(0)`/`SlotId(0)` never
// occur as live handles (an object is never at address 0), so 0 ⇄ None.
#[inline]
fn obj_ptr<T>(o: ObjId) -> *mut T {
    o.0 as *mut T
}
#[inline]
fn slot_ptr(s: SlotId) -> *mut CapSlot {
    s.0 as *mut CapSlot
}
#[inline]
fn opt_obj(p: *mut impl Sized) -> Option<ObjId> {
    if p.is_null() {
        None
    } else {
        Some(ObjId(p as u64))
    }
}
#[inline]
fn obj_or_null<T>(o: Option<ObjId>) -> *mut T {
    match o {
        Some(h) => h.0 as *mut T,
        None => ptr::null_mut(),
    }
}

impl Store for KernelStore {
    // ── cap slots ─────────────────────────────────────────────────────────
    fn slot(&self, s: SlotId) -> CapSlot {
        unsafe { *slot_ptr(s) }
    }
    fn set_slot(&mut self, s: SlotId, v: CapSlot) {
        unsafe { *slot_ptr(s) = v }
    }

    // ── object refcounts (ObjHeader is at offset 0 of every object) ───────
    fn obj_refs(&self, o: ObjId) -> u32 {
        unsafe { (*obj_ptr::<ObjHeader>(o)).refs }
    }
    fn set_obj_refs(&mut self, o: ObjId, r: u32) {
        unsafe { (*obj_ptr::<ObjHeader>(o)).refs = r }
    }

    // ── cspace ────────────────────────────────────────────────────────────
    fn cspace_num_slots(&self, cs: ObjId) -> u32 {
        unsafe { (*obj_ptr::<CSpaceObj>(cs)).num_slots }
    }
    fn cspace_slot(&self, cs: ObjId, i: u32) -> SlotId {
        unsafe { SlotId(CSpaceObj::slot(obj_ptr::<CSpaceObj>(cs), i) as u64) }
    }

    // ── channel ───────────────────────────────────────────────────────────
    fn chan_depth(&self, ch: ObjId) -> u32 {
        unsafe { (*obj_ptr::<Channel>(ch)).depth }
    }
    fn chan_end_caps(&self, ch: ObjId, end: usize) -> u32 {
        unsafe { (*obj_ptr::<Channel>(ch)).end_caps[end] }
    }
    fn set_chan_end_caps(&mut self, ch: ObjId, end: usize, v: u32) {
        unsafe { (*obj_ptr::<Channel>(ch)).end_caps[end] = v }
    }
    fn chan_head(&self, ch: ObjId, ring: usize) -> u32 {
        unsafe { (*obj_ptr::<Channel>(ch)).head[ring] }
    }
    fn set_chan_head(&mut self, ch: ObjId, ring: usize, v: u32) {
        unsafe { (*obj_ptr::<Channel>(ch)).head[ring] = v }
    }
    fn chan_count(&self, ch: ObjId, ring: usize) -> u32 {
        unsafe { (*obj_ptr::<Channel>(ch)).count[ring] }
    }
    fn set_chan_count(&mut self, ch: ObjId, ring: usize, v: u32) {
        unsafe { (*obj_ptr::<Channel>(ch)).count[ring] = v }
    }
    fn chan_binding(&self, ch: ObjId, end: usize, ev: usize) -> Binding {
        unsafe { (*obj_ptr::<Channel>(ch)).bindings[end][ev] }
    }
    fn set_chan_binding(&mut self, ch: ObjId, end: usize, ev: usize, b: Binding) {
        unsafe { (*obj_ptr::<Channel>(ch)).bindings[end][ev] = b }
    }
    fn chan_ring_cap(&self, ch: ObjId, ring: usize, i: u32, c: usize) -> SlotId {
        unsafe {
            let msg = Channel::slot(obj_ptr::<Channel>(ch), ring, i);
            SlotId(ptr::addr_of_mut!((*msg).caps[c]) as u64)
        }
    }
    fn chan_msg_len(&self, ch: ObjId, ring: usize, i: u32) -> u16 {
        unsafe { (*Channel::slot(obj_ptr::<Channel>(ch), ring, i)).len }
    }
    fn set_chan_msg_len(&mut self, ch: ObjId, ring: usize, i: u32, v: u16) {
        unsafe { (*Channel::slot(obj_ptr::<Channel>(ch), ring, i)).len = v }
    }
    fn chan_msg_write(&mut self, ch: ObjId, ring: usize, i: u32, data: &[u8]) {
        unsafe {
            let msg = Channel::slot(obj_ptr::<Channel>(ch), ring, i);
            let n = data.len().min(MSG_PAYLOAD);
            ptr::copy_nonoverlapping(
                data.as_ptr(),
                ptr::addr_of_mut!((*msg).payload).cast::<u8>(),
                n,
            );
        }
    }
    fn chan_msg_read(&self, ch: ObjId, ring: usize, i: u32, len: usize, buf: &mut [u8]) {
        unsafe {
            let msg = Channel::slot(obj_ptr::<Channel>(ch), ring, i);
            let n = len.min(MSG_PAYLOAD).min(buf.len());
            ptr::copy_nonoverlapping(
                ptr::addr_of!((*msg).payload).cast::<u8>(),
                buf.as_mut_ptr(),
                n,
            );
        }
    }

    // ── notification ──────────────────────────────────────────────────────
    fn notif_word(&self, n: ObjId) -> u64 {
        unsafe { (*obj_ptr::<NotifObj>(n)).word }
    }
    fn set_notif_word(&mut self, n: ObjId, v: u64) {
        unsafe { (*obj_ptr::<NotifObj>(n)).word = v }
    }
    fn notif_wait_head(&self, n: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<NotifObj>(n)).wait_head }
    }
    fn set_notif_wait_head(&mut self, n: ObjId, t: Option<ObjId>) {
        unsafe { (*obj_ptr::<NotifObj>(n)).wait_head = t }
    }
    fn notif_wait_tail(&self, n: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<NotifObj>(n)).wait_tail }
    }
    fn set_notif_wait_tail(&mut self, n: ObjId, t: Option<ObjId>) {
        unsafe { (*obj_ptr::<NotifObj>(n)).wait_tail = t }
    }

    // ── thread ────────────────────────────────────────────────────────────
    fn tcb_state(&self, t: ObjId) -> ThreadState {
        unsafe { (*obj_ptr::<Tcb>(t)).state }
    }
    fn set_tcb_state(&mut self, t: ObjId, s: ThreadState) {
        unsafe { (*obj_ptr::<Tcb>(t)).state = s }
    }
    fn tcb_qnext(&self, t: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<Tcb>(t)).qnext }
    }
    fn set_tcb_qnext(&mut self, t: ObjId, q: Option<ObjId>) {
        unsafe { (*obj_ptr::<Tcb>(t)).qnext = q }
    }
    fn tcb_wait_notif(&self, t: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<Tcb>(t)).wait_notif }
    }
    fn set_tcb_wait_notif(&mut self, t: ObjId, n: Option<ObjId>) {
        unsafe { (*obj_ptr::<Tcb>(t)).wait_notif = n }
    }
    fn tcb_report(&self, t: ObjId) -> Report {
        unsafe { (*obj_ptr::<Tcb>(t)).report }
    }
    fn set_tcb_report(&mut self, t: ObjId, r: Report) {
        unsafe { (*obj_ptr::<Tcb>(t)).report = r }
    }
    fn tcb_priority(&self, t: ObjId) -> u8 {
        unsafe { (*obj_ptr::<Tcb>(t)).priority }
    }
    fn set_tcb_priority(&mut self, t: ObjId, p: u8) {
        unsafe { (*obj_ptr::<Tcb>(t)).priority = p }
    }
    fn tcb_bind_slot(&self, t: ObjId, which: usize) -> SlotId {
        unsafe { SlotId(ptr::addr_of_mut!((*obj_ptr::<Tcb>(t)).bind_slots[which]) as u64) }
    }
    fn tcb_bind_bits(&self, t: ObjId, which: usize) -> u64 {
        unsafe { (*obj_ptr::<Tcb>(t)).bind_bits[which] }
    }
    fn set_tcb_bind_bits(&mut self, t: ObjId, which: usize, b: u64) {
        unsafe { (*obj_ptr::<Tcb>(t)).bind_bits[which] = b }
    }
    fn tcb_cspace(&self, t: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<Tcb>(t)).cspace }
    }
    fn set_tcb_cspace(&mut self, t: ObjId, cs: Option<ObjId>) {
        unsafe { (*obj_ptr::<Tcb>(t)).cspace = cs }
    }
    fn tcb_aspace(&self, t: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<Tcb>(t)).aspace }
    }
    fn set_tcb_aspace(&mut self, t: ObjId, a: Option<ObjId>) {
        unsafe { (*obj_ptr::<Tcb>(t)).aspace = a }
    }
    fn set_tcb_retval(&mut self, t: ObjId, v: u64) {
        unsafe { (*obj_ptr::<Tcb>(t)).frame.x[0] = v }
    }

    // ── timer ─────────────────────────────────────────────────────────────
    fn timer_armed(&self, t: ObjId) -> bool {
        unsafe { (*obj_ptr::<TimerObj>(t)).armed }
    }
    fn set_timer_armed(&mut self, t: ObjId, v: bool) {
        unsafe { (*obj_ptr::<TimerObj>(t)).armed = v }
    }
    fn timer_deadline(&self, t: ObjId) -> u64 {
        unsafe { (*obj_ptr::<TimerObj>(t)).deadline }
    }
    fn set_timer_deadline(&mut self, t: ObjId, v: u64) {
        unsafe { (*obj_ptr::<TimerObj>(t)).deadline = v }
    }
    fn timer_notif(&self, t: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<TimerObj>(t)).notif }
    }
    fn set_timer_notif(&mut self, t: ObjId, n: Option<ObjId>) {
        unsafe { (*obj_ptr::<TimerObj>(t)).notif = n }
    }
    fn timer_bits(&self, t: ObjId) -> u64 {
        unsafe { (*obj_ptr::<TimerObj>(t)).bits }
    }
    fn set_timer_bits(&mut self, t: ObjId, v: u64) {
        unsafe { (*obj_ptr::<TimerObj>(t)).bits = v }
    }
    fn timer_next(&self, t: ObjId) -> Option<ObjId> {
        unsafe { (*obj_ptr::<TimerObj>(t)).next }
    }
    fn set_timer_next(&mut self, t: ObjId, n: Option<ObjId>) {
        unsafe { (*obj_ptr::<TimerObj>(t)).next = n }
    }

    // ── hardware / scheduler seam ─────────────────────────────────────────
    fn make_runnable(&mut self, t: ObjId) {
        unsafe { crate::thread::enqueue(obj_ptr::<Tcb>(t)) }
    }
    fn unqueue_ready(&mut self, t: ObjId) {
        unsafe { crate::thread::unqueue_ready(obj_ptr::<Tcb>(t)) }
    }
    fn aspace_unmap(&mut self, a: ObjId, va: u64, pages: u64) {
        unsafe { crate::aspace::unmap(obj_ptr::<AspaceObj>(a), va, pages) }
    }
    fn aspace_map(
        &mut self,
        a: ObjId,
        pa: u64,
        va: u64,
        pages: u64,
        perms: u64,
    ) -> Result<(), crate::aspace::MapError> {
        // The trusted page-table join (rev1§6.1(c)): the verified cap-side record is
        // `kcore::cspace::map_frame`, which drives this seam exactly as `delete` drives
        // `aspace_unmap`.
        unsafe { crate::aspace::map(obj_ptr::<AspaceObj>(a), pa, va, pages, perms) }
    }
    fn aspace_destroy(&mut self, a: ObjId) {
        unsafe { crate::aspace::destroy_aspace(obj_ptr::<AspaceObj>(a)) }
    }
    fn tlb_invalidate_page(&mut self, asid: u16, va: u64) {
        // TLBI VAE1: [63:48] ASID, [43:0] VA[55:12].
        let arg = ((asid as u64) << 48) | ((va >> 12) & 0xFFF_FFFF_FFFF);
        unsafe { core::arch::asm!("tlbi vae1, {v}", v = in(reg) arg) };
    }
    fn barrier_after_map(&mut self) {
        unsafe { core::arch::asm!("dsb ishst") };
    }
    fn barrier_after_unmap(&mut self) {
        unsafe { core::arch::asm!("dsb ish", "isb") };
    }
    fn timer_armed_head(&self) -> Option<ObjId> {
        unsafe { opt_obj(crate::timer::armed_head()) }
    }
    fn set_timer_armed_head(&mut self, h: Option<ObjId>) {
        unsafe { crate::timer::set_armed_head(obj_or_null::<TimerObj>(h)) }
    }
    // ── ready queue (B8C): the per-level head/tail + bitmap, realized over the
    //    `READY`/`READY_BITMAP` kernel statics. The verified `kcore::ready` ops run
    //    against these by-handle accessors (the trusted ObjId↔`*mut Tcb` link seam).
    fn ready_head(&self, level: usize) -> Option<ObjId> {
        unsafe { crate::thread::ready_head_at(level) }
    }
    fn set_ready_head(&mut self, level: usize, h: Option<ObjId>) {
        unsafe { crate::thread::set_ready_head_at(level, h) }
    }
    fn ready_tail(&self, level: usize) -> Option<ObjId> {
        unsafe { crate::thread::ready_tail_at(level) }
    }
    fn set_ready_tail(&mut self, level: usize, t: Option<ObjId>) {
        unsafe { crate::thread::set_ready_tail_at(level, t) }
    }
    fn ready_bitmap(&self) -> u32 {
        unsafe { crate::thread::ready_bitmap_get() }
    }
    fn set_ready_bitmap(&mut self, b: u32) {
        unsafe { crate::thread::ready_bitmap_set(b) }
    }
}
