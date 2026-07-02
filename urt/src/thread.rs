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

//! In-process thread spawn/join/yield/sleep (rev2§5.3) — the substrate the std
//! `sys/thread/eunomia` arm drives through the `eunomia_sys::thread` bridge.
//!
//! A std thread must share the spawning process's address space (its heap,
//! globals, and code), so it is started with `thread_start_as` (op 18) bound to
//! the process's *own* aspace and cspace — **not** `thread_start` (op 13), whose
//! fresh TCB runs in the kernel identity map where the process heap is unmapped.
//! `thread_start_as` carries the closure pointer in the seventh arg register (x6),
//! which lands in the new thread's initial `x0`; the std trampoline
//! reads it there.
//!
//! **Provisioning (rev2§2.3, scoped/opt-in).** Only a thread-capable process is
//! [`configure`]d — with caps to its own aspace (WRITE, to map thread stacks), its
//! own cspace (to name it in `thread_start_as`), a thread-untyped to retype the
//! per-thread objects from, and a free cspace-slot range. An unconfigured process
//! (the least-authority default) gets an error from [`spawn`].
//!
//! **Reuse pool.** Each of the [`MAX_THREADS`] slots lazily carves a persistent
//! per-thread sub-untyped from the thread-untyped on first use (its own CDT
//! subtree, so a join's revoke+reset reclaims it without touching siblings). Slot
//! `i`'s stack lives at a fixed VA below the main stack, one guard page apart, all
//! inside the single 2 MiB L3 table the loader already populated for the main
//! stack — so mapping a stack never needs a page-table top-up.
//!
//! **Disciplines mirrored from `urt::spawn` (the process-spawn twin).**
//! *bind-before-start* (the on-exit/on-fault notifications are bound to the TCB
//! before it runs, so a thread that exits immediately still raises its bit) and
//! *read-report-before-revoke* (the report lives in the TCB the revoke destroys).
//! Unlike `SpawnRec`, a thread shares the process's donation rather than owning
//! one, so teardown is a per-slot revoke, not a whole-process one.
//!
//! Trusted shell, no `verus!{}`: syscall marshalling over the verified `ipc::sys`
//! and the verified `slots::SlotAlloc`; its host-reachable invariant (the stack-VA
//! / slot allocator) is host-tested, the syscall path witnessed by the QEMU smoke.

use crate::lock::SpinLock;
use crate::slots::SlotAlloc;
use crate::thread_layout::{slot_of_sp, stack_region, PAGE, SLOTS_PER_THREAD, STACK_PAGES};
use core::cell::UnsafeCell;
use ipc::sys;

/// Max concurrent in-process threads. **Budget** (host-tested in
/// [`crate::thread_layout`]): slot `i`'s stack sits below the main stack within the
/// single 2 MiB (512-page) L3 table the loader already populated for it —
/// `MAX_THREADS*(STACK_PAGES+GUARD) + STACK_PAGES = 16*17 + 16 = 288 < 512` pages —
/// so mapping one reuses that table (no `aspace_topup`, which a separately carved
/// untyped could not satisfy). Disclosed MVP bound, tunable in lockstep with
/// `thread_layout::MAX_THREADS`.
pub const MAX_THREADS: usize = crate::thread_layout::MAX_THREADS;

/// Bytes carved per thread from the process thread-untyped: one TCB + `STACK_PAGES`
/// stack frames + two notifications (the join/termination notif and the
/// futex park-notif), with slack. Committed on a slot's first use and reused
/// across that slot's spawn/join cycles.
const PER_THREAD_BYTES: u64 = 128 * 1024;

/// Total bytes a thread-capable process's thread-untyped must hold — one
/// [`PER_THREAD_BYTES`] carve per [`MAX_THREADS`] slot, plus one more block for the
/// **main** thread's own futex park-notif (the main thread is not a
/// pool slot, so it carves its park-notif from the shared thread-untyped). The
/// producer (the spawner) sizes the untyped it grants from this constant, the
/// shared convention.
pub const THREAD_UNTYPED_BYTES: u64 = PER_THREAD_BYTES * (MAX_THREADS as u64 + 1);

/// Fixed priority for spawned threads (MVP). Must be `<=` the process's own
/// priority — the rev2§5.4 ceiling stamped on the TCB cap at retype — or `spawn`
/// is refused (`ERR_PERM`). Threads time-slice among themselves (round-robin at
/// this level) and run when the main thread blocks (e.g. at `join`); passing the
/// process's actual priority for same-level time-slicing with the main thread is a
/// follow-up.
const THREAD_PRIO: u64 = 1;

/// Notification bits a thread's termination raises (its own private notif, so the
/// bit values are local). Both mean "done"; the report distinguishes exit/fault.
const EXIT_BIT: u64 = 1 << 0;
const FAULT_BIT: u64 = 1 << 1;

/// An opaque join handle: the pool slot the spawned thread occupies. `join`
/// consumes it. Encoded across the seam as `slot as u64`.
pub struct JoinHandle {
    slot: usize,
}

impl JoinHandle {
    /// The pool slot, for the `u64` bridge encoding.
    pub fn index(&self) -> usize {
        self.slot
    }
    /// Reconstruct from the bridge encoding.
    pub fn from_index(slot: usize) -> JoinHandle {
        JoinHandle { slot }
    }
}

/// One reuse-pool slot's persistent kernel resources.
#[derive(Clone, Copy)]
struct Slot {
    /// A thread currently occupies this slot (spawned, not yet joined).
    in_use: bool,
    /// The persistent per-slot resources (`sub_ut` + the working cspace slots) are
    /// carved. `false` until first use.
    ready: bool,
    sub_ut: u32,
    tcb: u32,
    frame: u32,
    notif: u32,
    scratch: u32,
    /// The per-thread futex park-notif: a notification this slot's
    /// thread blocks on in `sys::futex` and a waker signals. Retyped from `sub_ut`
    /// at each spawn (so a join's revoke reclaims it), valid only while `in_use`.
    park_notif: u32,
}

impl Slot {
    const fn empty() -> Slot {
        Slot {
            in_use: false,
            ready: false,
            sub_ut: 0,
            tcb: 0,
            frame: 0,
            notif: 0,
            scratch: 0,
            park_notif: 0,
        }
    }
}

struct Inner {
    configured: bool,
    self_aspace: u32,
    self_cspace: u32,
    self_untyped: u32,
    /// Allocator for the per-slot working cspace slots. `None` until `configure`;
    /// `WORDS = 2` covers up to 128 slots (`SLOTS_PER_THREAD*MAX_THREADS + 1 = 97`).
    slots: Option<SlotAlloc<2>>,
    pool: [Slot; MAX_THREADS],
    /// The main thread's futex park-notif slot, carved lazily on its
    /// first `sys::futex` wait from `self_untyped` + one working slot. `0` until
    /// carved. The main thread has no pool slot, so it keeps its park-notif here.
    main_park: u32,
}

/// Process-global thread state, guarded by its own spinlock (distinct from the
/// heap's — no lock-ordering hazard, `spawn`/`join` never allocate while holding
/// it). `configure` runs once at bootstrap before any spawn.
struct State {
    lock: SpinLock,
    inner: UnsafeCell<Inner>,
}

// SAFETY: every access to `inner` is under `lock` (mutual exclusion, Loom-certified
// in `lock.rs`); the `UnsafeCell` interior is never reached by two threads at once.
unsafe impl Sync for State {}

static STATE: State = State {
    lock: SpinLock::new(),
    inner: UnsafeCell::new(Inner {
        configured: false,
        self_aspace: 0,
        self_cspace: 0,
        self_untyped: 0,
        slots: None,
        pool: [Slot::empty(); MAX_THREADS],
        main_park: 0,
    }),
};

/// Provision this process for in-process threading (called once by
/// `eunomia_sys::bootstrap` when the self-cap grants are present). Idempotent.
pub fn configure(
    self_aspace: u32,
    self_cspace: u32,
    self_untyped: u32,
    slot_base: u32,
    slot_len: u32,
) {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    let inner = unsafe { &mut *STATE.inner.get() };
    if inner.configured {
        return;
    }
    inner.self_aspace = self_aspace;
    inner.self_cspace = self_cspace;
    inner.self_untyped = self_untyped;
    // `SlotAlloc<2>` holds at most `2*64 = 128` slots; clamp to re-establish its
    // `cap <= WORDS*64` precondition at the seam (the §11 inverse-leak guard — the
    // `requires` erases in exec builds). `SLOTS_PER_THREAD*MAX_THREADS + 1 = 97 <=
    // 128`, so a well-provisioned range is never actually shortened.
    let cap = (slot_len as usize).min(2 * 64);
    inner.slots = Some(SlotAlloc::new(slot_base, cap));
    inner.configured = true;
}

/// Whether this process was configured for threading (the std arm maps `false` to
/// `Unsupported` before ever reaching a syscall).
pub fn is_configured() -> bool {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    unsafe { (*STATE.inner.get()).configured }
}

/// Destroy a slot's per-thread objects (TCB, stack frame → unmapped, notif) and
/// reset its sub-untyped for reuse — the persistent `sub_ut` + cspace slots stay.
fn recycle(s: &Slot) {
    sys::cap_revoke_all(s.sub_ut);
    let _ = sys::untyped_reset(s.sub_ut);
}

/// Spawn an in-process thread that enters `entry` (a naked/plain `extern "C"`
/// trampoline) with `arg` in `x0`. `stack_size` is capped at the fixed
/// `STACK_PAGES*PAGE` (a larger request is refused — MVP). Returns the join handle
/// or a negative syscall error.
pub fn spawn(entry: usize, stack_size: usize, arg: u64) -> Result<JoinHandle, i64> {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`; no allocation happens while it is held.
    let inner = unsafe { &mut *STATE.inner.get() };
    if !inner.configured {
        return Err(sys::ERR_STATE);
    }
    if stack_size as u64 > STACK_PAGES * PAGE {
        return Err(sys::ERR_ARG);
    }

    // Find a free pool slot.
    let mut slot_i = None;
    for i in 0..MAX_THREADS {
        if !inner.pool[i].in_use {
            slot_i = Some(i);
            break;
        }
    }
    let i = slot_i.ok_or(sys::ERR_NOMEM)?;

    // Lazily commit the slot's persistent resources on first use.
    if !inner.pool[i].ready {
        let base = {
            let sa = inner.slots.as_mut().ok_or(sys::ERR_STATE)?;
            sa.alloc_range(SLOTS_PER_THREAD).ok_or(sys::ERR_NOMEM)?
        };
        // Carve this slot's persistent sub-untyped (its own CDT subtree).
        let r = sys::retype(
            inner.self_untyped,
            sys::OBJ_UNTYPED,
            PER_THREAD_BYTES,
            base,
            0,
        );
        if r < 0 {
            inner
                .slots
                .as_mut()
                .unwrap()
                .free_range(base, SLOTS_PER_THREAD);
            return Err(r);
        }
        inner.pool[i] = Slot {
            in_use: false,
            ready: true,
            sub_ut: base,
            tcb: base + 1,
            frame: base + 2,
            notif: base + 3,
            scratch: base + 4,
            park_notif: base + 5,
        };
    }

    let s = inner.pool[i];

    // Retype the per-thread objects from the (fresh or reset) sub-untyped.
    let r = sys::retype(s.sub_ut, sys::OBJ_THREAD, 0, s.tcb, 0);
    if r < 0 {
        recycle(&s);
        return Err(r);
    }
    let r = sys::retype(s.sub_ut, sys::OBJ_FRAME, STACK_PAGES, s.frame, 0);
    if r < 0 {
        recycle(&s);
        return Err(r);
    }
    let r = sys::retype(s.sub_ut, sys::OBJ_NOTIF, 0, s.notif, 0);
    if r < 0 {
        recycle(&s);
        return Err(r);
    }
    // The per-thread futex park-notif, from the same sub-untyped so a
    // join's revoke reclaims it. Full rights, so any thread sharing the cspace both
    // signals (WRITE) and waits (READ) on it.
    let r = sys::retype(s.sub_ut, sys::OBJ_NOTIF, 0, s.park_notif, 0);
    if r < 0 {
        recycle(&s);
        return Err(r);
    }

    // Map the stack into the shared aspace; the page below stays unmapped (guard).
    let (top, bottom) = stack_region(i);
    let r = sys::map(inner.self_aspace, s.frame, bottom, sys::PERM_W);
    if r < 0 {
        recycle(&s);
        return Err(r);
    }

    // Bind on-exit / on-fault to the thread's notif BEFORE starting it (the
    // rev2§5.1 lost-wakeup discipline). `cap_copy` stages a duplicate that
    // `thread_bind` moves into the TCB, leaving the parent's `notif` intact.
    for (which, bit) in [(sys::BIND_EXIT, EXIT_BIT), (sys::BIND_FAULT, FAULT_BIT)] {
        let r = sys::cap_copy(s.notif, s.scratch, sys::RIGHTS_ALL);
        if r < 0 {
            recycle(&s);
            return Err(r);
        }
        let r = sys::thread_bind(s.tcb, which, s.scratch, bit);
        if r < 0 {
            sys::cap_delete(s.scratch);
            recycle(&s);
            return Err(r);
        }
    }

    // Start: shares the process aspace/cspace, closure pointer in x0 (step 1).
    let r = sys::thread_start_as(
        s.tcb,
        inner.self_cspace,
        inner.self_aspace,
        entry as u64,
        top,
        THREAD_PRIO,
        arg,
    );
    if r < 0 {
        recycle(&s);
        return Err(r);
    }

    inner.pool[i].in_use = true;
    Ok(JoinHandle { slot: i })
}

/// Block until the thread in `h`'s slot terminates, then reclaim its objects. The
/// lock is **not** held across the blocking wait.
pub fn join(h: JoinHandle) -> Result<(), i64> {
    if h.slot >= MAX_THREADS {
        return Err(sys::ERR_ARG);
    }
    // Snapshot the slot under the lock, then release it for the blocking wait.
    let s = {
        let _g = STATE.lock.lock();
        // SAFETY: exclusive under `lock`.
        let inner = unsafe { &mut *STATE.inner.get() };
        if !inner.pool[h.slot].in_use {
            return Err(sys::ERR_STATE);
        }
        inner.pool[h.slot]
    };

    // Block until the thread raises its exit or fault bit.
    let w = sys::notif_wait(s.notif);
    if w < 0 {
        return Err(w);
    }

    // Read the terminal report strictly before the revoke destroys the TCB it
    // lives in (the rev2§5.1 ordering `urt::spawn::reap` also enforces).
    let (state, _v1, _v2) = sys::read_report(s.tcb);
    debug_assert!(
        state >= 0,
        "read_report failed in join — revoke ran before read (ordering?)"
    );

    // Reclaim this slot's objects (unmaps the stack) and reset its sub-untyped.
    recycle(&s);

    // Mark the slot free for reuse (its persistent resources stay carved).
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    let inner = unsafe { &mut *STATE.inner.get() };
    inner.pool[h.slot].in_use = false;
    Ok(())
}

/// This calling thread's own futex park-notif cspace slot: the
/// notification it blocks on in `sys::futex` and a waker signals. Identifies the
/// caller by its stack pointer (the inverse `thread_layout::slot_of_sp`): a pool
/// thread returns its slot's `park_notif` (carved at spawn); the **main** thread
/// (no pool slot) lazily carves its own from `self_untyped` on first use. Only ever
/// called by a thread *for itself*, so the SP self-identification is exact.
/// `Err` if the process is unconfigured, the slot allocator/untyped is exhausted,
/// or the SP maps to a slot not currently running — the futex arm degrades to a
/// yield-poll rather than pushing a bogus slot into a syscall (the §11 guard).
pub fn current_park_notif() -> Result<u32, i64> {
    let sp = read_sp();
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    let inner = unsafe { &mut *STATE.inner.get() };
    if !inner.configured {
        return Err(sys::ERR_STATE);
    }
    match slot_of_sp(sp) {
        Some(i) => {
            let s = inner.pool[i];
            if s.ready && s.in_use {
                Ok(s.park_notif)
            } else {
                Err(sys::ERR_STATE)
            }
        }
        None => {
            // Main thread: carve its park-notif once, from the shared thread-untyped
            // + one working slot (the `WORKING_SLOTS` `+ 1`).
            if inner.main_park != 0 {
                return Ok(inner.main_park);
            }
            let slot = inner
                .slots
                .as_mut()
                .ok_or(sys::ERR_STATE)?
                .alloc()
                .ok_or(sys::ERR_NOMEM)?;
            let r = sys::retype(inner.self_untyped, sys::OBJ_NOTIF, 0, slot, 0);
            if r < 0 {
                inner.slots.as_mut().unwrap().free(slot);
                return Err(r);
            }
            inner.main_park = slot;
            Ok(slot)
        }
    }
}

/// This thread's current stack pointer, for [`current_park_notif`]'s self-lookup.
#[inline(always)]
fn read_sp() -> u64 {
    let sp: u64;
    // SAFETY: reading SP is unconditionally valid at EL0; `nomem`/`nostack` — a pure
    // register read with no memory effect.
    unsafe {
        core::arch::asm!("mov {sp}, sp", sp = out(reg) sp, options(nomem, nostack, preserves_flags));
    }
    sp
}

/// Cooperative yield (op 2).
pub fn yield_now() {
    sys::yield_now();
}

/// Sleep at least `nanos` — the MVP yield-poll (rev2§5.4): spin on the grant-free
/// monotonic counter, yielding between reads. Busy but correct; the power-efficient
/// `TimerArm`+`NotifWait` blocking sleep is a follow-up (it needs a per-thread timer
/// cap the process does not hold today).
pub fn sleep(nanos: u64) {
    if nanos == 0 {
        return;
    }
    let start = crate::time::now_mono_ns();
    let deadline = start.saturating_add(nanos as i64);
    while crate::time::now_mono_ns() < deadline {
        sys::yield_now();
    }
}
