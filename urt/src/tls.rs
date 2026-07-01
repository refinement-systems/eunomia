//! Process-global thread-local **key table** (std-port 3.5): the verified key
//! allocator behind std's key-based TLS (`vendor/rust`'s `sys/thread_local/os.rs`).
//!
//! A TLS *key* is an index into every thread's `TPIDR_EL0` block (the per-thread
//! pointer slots `eunomia_sys::tls` manages). Both std's `local_pointer!` (the
//! current-thread handle/id, `dtor = None`) and every `thread_local!` variable
//! draw a key here — one shared allocator, **replacing std-port 3.2's raw
//! `NEXT_SLOT` counter**. Each key also carries the destructor std registered for
//! it, so the seam's thread-exit runner can drop live `thread_local!` values.
//!
//! **Verified by Verus.** The allocator is a thin [`KeyTable`] over the verified
//! [`crate::slots::SlotAlloc`]: [`KeyTable::create`] hands out an in-range key that
//! was free and is now live (so successive keys are distinct — the key-uniqueness
//! property the TLS layer relies on, proven not assumed), exhaustion is exact
//! (`None` ⟺ all `TLS_KEYS` keys live), and [`KeyTable::destroy`] frees a live key
//! (a double-destroy is a contract-checked impossibility). The per-key destructor
//! registry (`[Option<Dtor>; TLS_KEYS]`) is plain sibling bookkeeping — a table of
//! opaque function pointers is not a verifiable property — guarded by the same
//! [`crate::lock::SpinLock`] `urt::Heap`/`urt::random` use (mutual exclusion, no
//! wait/wake, so no new interleaving model; Miri is the data-race oracle).
//!
//! `get`/`set` (the per-thread value read/write) are *not* here — each thread
//! touches only its own `TPIDR` block, so they need no lock and live in the seam
//! (`eunomia_sys::tls`) with the `mrs`/`msr` asm.
use crate::lock::SpinLock;
use crate::slots::SlotAlloc;
use core::cell::UnsafeCell;
use vstd::prelude::*;

/// A TLS destructor: std's `os::destroy_value::<T>`, run on a live value at thread
/// exit. Registered per key at [`create`]; snapshotted by [`collect_dtors`].
pub type Dtor = unsafe extern "C" fn(*mut u8);

verus! {

/// TLS keys per process — the width of every thread's `TPIDR` block. **MUST match
/// `eunomia_sys::tls::TLS_SLOTS`** (the seam indexes `[0, TLS_SLOTS)` into a block
/// of exactly this many pointer slots). Comfortably above std's handful of
/// `local_pointer!` sites plus the `thread_local!`s a program declares. Declared
/// inside `verus!{}` so the `KeyTable` contracts can speak of it; still an ordinary
/// `const` for the plain state below.
pub const TLS_KEYS: usize = 64;

/// The verified key allocator: a `SlotAlloc<1>` over the fixed window
/// `[0, TLS_KEYS)`. `WORDS = 1` (one bitmap word) exactly covers `TLS_KEYS = 64`.
pub struct KeyTable {
    slots: SlotAlloc<1>,
}

impl KeyTable {
    /// Well-formedness: the underlying allocator is well-formed over the fixed
    /// window `[0, TLS_KEYS)`. `new` establishes it and every op preserves it.
    /// `closed` — the body reads the private `slots` field.
    pub closed spec fn wf(self) -> bool {
        &&& self.slots.wf()
        &&& self.slots.spec_base() == 0
        &&& self.slots.spec_cap() == TLS_KEYS as int
    }

    /// Key `key` is **live** (allocated) ⇔ its slot is not free. Meaningful for
    /// `0 <= key < TLS_KEYS`. `closed`, as [`KeyTable::wf`].
    pub closed spec fn is_live(self, key: int) -> bool {
        !self.slots.is_free_spec(key)
    }

    /// A fresh table: every key free (none live).
    pub fn new() -> (t: KeyTable)
        ensures
            t.wf(),
            forall|k: int| 0 <= k < TLS_KEYS ==> !t.is_live(k),
    {
        KeyTable { slots: SlotAlloc::new(0, TLS_KEYS) }
    }

    /// Allocate one key, or `None` when all `TLS_KEYS` are live. A returned key was
    /// free and is now live, in `[0, TLS_KEYS)`, distinct from every other live key
    /// (the frame clause) — the key-uniqueness the TLS layer stands on.
    pub fn create(&mut self) -> (r: Option<u32>)
        requires
            old(self).wf(),
        ensures
            final(self).wf(),
            match r {
                Some(k) => {
                    &&& 0 <= k < TLS_KEYS
                    &&& !old(self).is_live(k as int)
                    &&& final(self).is_live(k as int)
                    &&& forall|j: int|
                        0 <= j < TLS_KEYS && j != k as int ==> final(self).is_live(j) == old(
                            self,
                        ).is_live(j)
                },
                None => forall|j: int| 0 <= j < TLS_KEYS ==> old(self).is_live(j),
            },
    {
        self.slots.alloc()
    }

    /// Free a live key (std destroys a redundant key it just created in the
    /// `LazyKey` race). The `is_live` precondition makes a double-destroy a
    /// contract-checked impossibility (it would hand a live key out twice).
    pub fn destroy(&mut self, key: u32)
        requires
            old(self).wf(),
            0 <= key < TLS_KEYS,
            old(self).is_live(key as int),
        ensures
            final(self).wf(),
            !final(self).is_live(key as int),
            forall|j: int|
                0 <= j < TLS_KEYS && j != key as int ==> final(self).is_live(j) == old(
                    self,
                ).is_live(j),
    {
        self.slots.free(key)
    }
}

} // verus!
/// Process-global key state: the verified [`KeyTable`] plus its per-key destructor
/// registry, both guarded by one spinlock. `table` starts `None` and self-inits on
/// first [`create`] (a bootstrap-free lazy static; all-zero + unlocked keeps it in
/// `.bss`, the loader zeroes it with the RW segment) — no caps or configure step,
/// unlike `urt::thread`.
struct State {
    lock: SpinLock,
    inner: UnsafeCell<Inner>,
}

struct Inner {
    /// `None` until the first `create`; then the live [`KeyTable`]. The
    /// well-formedness `create`/`destroy` require is re-established by construction:
    /// `KeyTable::new` gives `wf()`, and both ops preserve it (verified), and the
    /// table is built no other way — the §11 inverse-leak guard for the erased
    /// `requires` (the `urt::thread` `slots: Option<SlotAlloc>` precedent).
    table: Option<KeyTable>,
    /// `dtors[k]` = the destructor std registered for key `k` (`None` = free key or
    /// a `local_pointer!` with no destructor). Snapshotted by [`collect_dtors`].
    dtors: [Option<Dtor>; TLS_KEYS],
}

// SAFETY: `inner` is only ever reached while `lock` is held (the `urt::Heap` /
// `urt::random` posture — mutual exclusion by the spinlock over the `UnsafeCell`).
unsafe impl Sync for State {}

static STATE: State = State {
    lock: SpinLock::new(),
    inner: UnsafeCell::new(Inner {
        table: None,
        dtors: [None; TLS_KEYS],
    }),
};

/// Allocate a TLS key and register its destructor. Returns `None` when all
/// `TLS_KEYS` keys are live (the seam maps that to std's "out of TLS keys" abort).
pub fn create(dtor: Option<Dtor>) -> Option<u32> {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    let inner = unsafe { &mut *STATE.inner.get() };
    if inner.table.is_none() {
        inner.table = Some(KeyTable::new());
    }
    // The borrow of `table` ends before we touch the disjoint `dtors` field.
    let r = inner.table.as_mut().unwrap().create();
    if let Some(k) = r {
        inner.dtors[k as usize] = dtor;
    }
    r
}

/// Free a TLS key and clear its destructor. A no-op if the table is uninitialized
/// or `key` is out of range (the inverse-leak runtime guard re-establishing
/// `KeyTable::destroy`'s `key < TLS_KEYS` precondition at the plain boundary).
pub fn destroy(key: u32) {
    if (key as usize) >= TLS_KEYS {
        return;
    }
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    let inner = unsafe { &mut *STATE.inner.get() };
    if let Some(t) = inner.table.as_mut() {
        t.destroy(key);
        inner.dtors[key as usize] = None;
    }
}

/// Snapshot the per-key destructor registry for the thread-exit runner. The runner
/// reads each thread's `TPIDR` values *outside* the lock (a destructor may re-enter
/// [`create`] when it initializes another `thread_local!`), so it copies the
/// registry out here and releases the lock before running anything.
pub fn collect_dtors(out: &mut [Option<Dtor>; TLS_KEYS]) {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    let inner = unsafe { &*STATE.inner.get() };
    *out = inner.dtors;
}

#[cfg(all(test, not(loom), not(shuttle)))]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // ── The verified allocator, tested directly (pure, no process-global) ──

    #[test]
    fn keytable_hands_out_distinct_in_range_keys_then_exhausts() {
        let mut t = KeyTable::new();
        let mut seen = HashSet::new();
        for _ in 0..TLS_KEYS {
            let k = t.create().expect("a free key while under capacity");
            assert!((k as usize) < TLS_KEYS, "key {k} out of window");
            assert!(seen.insert(k), "duplicate key {k}");
        }
        assert!(t.create().is_none(), "create past TLS_KEYS must be None");
    }

    #[test]
    fn keytable_reuses_a_destroyed_key() {
        let mut t = KeyTable::new();
        for _ in 0..TLS_KEYS {
            t.create().unwrap();
        }
        // Full: free one, the next create must hand that index back.
        t.destroy(10);
        assert_eq!(t.create(), Some(10), "a freed key must be reusable");
    }

    #[test]
    fn keytable_sequential_keys_start_at_zero() {
        // `SlotAlloc::alloc` scans from 0, so the first keys are 0, 1, 2… — the
        // stable numbering `local_pointer!` (CURRENT/ID) and the first
        // `thread_local!` rely on.
        let mut t = KeyTable::new();
        assert_eq!(t.create(), Some(0));
        assert_eq!(t.create(), Some(1));
        assert_eq!(t.create(), Some(2));
    }

    // ── The process-global wrapper (robust to parallel interference: asserts only
    //    about the key it owns, and always frees it) ──

    #[test]
    fn global_create_registers_its_dtor_then_frees() {
        unsafe extern "C" fn sentinel(_: *mut u8) {}
        let k = create(Some(sentinel)).expect("a global key");
        let mut buf = [None; TLS_KEYS];
        collect_dtors(&mut buf);
        assert!(buf[k as usize].is_some(), "registered dtor must be visible");
        destroy(k);
        // After destroy the slot is reusable; another create may or may not return
        // `k` depending on concurrent tests, so we only assert it succeeds.
        let k2 = create(None).expect("reuse after destroy");
        destroy(k2);
    }
}
