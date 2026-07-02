// SPDX-License-Identifier: 0BSD
//! Pure address-space geometry for in-process thread stacks.
//!
//! Split out of `thread` (which is bare-metal-only, since it issues syscalls) so
//! the stack-VA arithmetic — the one host-reachable invariant of the thread
//! primitive — is host-tested: every slot's stack is below the main stack, guard-
//! separated from its neighbours, and the whole pool fits the single 2 MiB L3 table
//! the loader already populated for the main stack (so a `map` never needs a page-
//! table top-up, which a separately-carved untyped could not satisfy — rev2§2.5).

/// Top of the main thread's stack (`loader::spawn::STACK_TOP`). Thread stacks grow
/// downward from below it.
pub const STACK_TOP: u64 = 0x9000_0000;
pub const PAGE: u64 = 4096;
/// Pages per stack (`loader::spawn::STACK_PAGES`), main and thread alike.
pub const STACK_PAGES: u64 = 16;
/// One unmapped guard page below each stack (rev2§5.3).
pub const GUARD_PAGES: u64 = 1;
/// Per-thread VA stride: a stack plus its guard page.
pub const STRIDE: u64 = (STACK_PAGES + GUARD_PAGES) * PAGE;

/// Max concurrent in-process threads (see [`crate::thread::MAX_THREADS`]).
pub const MAX_THREADS: usize = 16;

/// Working cspace slots per thread slot: {sub-untyped, TCB, stack frame, notif,
/// scratch, park-notif} (carved once per slot, reused across its spawn/join
/// cycles). The park-notif (base+5) is the per-thread notification a `sys::futex`
/// waiter blocks on and a waker signals.
pub const SLOTS_PER_THREAD: u32 = 6;

/// Total free cspace slots the thread pool needs, the convention shared by the
/// producer (which reserves the range and sizes the child cspace) and
/// `thread::configure`. `6 * 16 + 1 = 97 <= 128`, the `SlotAlloc<2>` cap; the
/// trailing `+ 1` is the main thread's own futex park-notif slot,
/// which is not one of the pool slots.
pub const WORKING_SLOTS: u32 = SLOTS_PER_THREAD * (MAX_THREADS as u32) + 1;

/// The `(top, bottom)` VA of slot `slot`'s stack: `top` is the initial SP (16-byte
/// aligned, exclusive), `bottom` the lowest mapped byte; `[bottom - PAGE, bottom)`
/// is the guard. Slot 0 sits one stride below the main stack.
pub const fn stack_region(slot: usize) -> (u64, u64) {
    let top = STACK_TOP - (slot as u64 + 1) * STRIDE;
    let bottom = top - STACK_PAGES * PAGE;
    (top, bottom)
}

/// The pool slot whose stack contains `sp`, or `None` for the main thread (whose
/// stack sits above `STACK_TOP`, outside every pool region). The inverse of
/// [`stack_region`]: a running thread reads its own `sp` to find which slot it
/// occupies — hence which per-thread futex park-notif is its own —
/// without threading a slot index through the spawn trampoline. Well-defined
/// because the regions are disjoint and guard-separated (the tests below), so at
/// most one contains `sp`.
pub fn slot_of_sp(sp: u64) -> Option<usize> {
    let mut i = 0;
    while i < MAX_THREADS {
        let (top, bottom) = stack_region(i);
        // A live SP lies in `[bottom, top]`: it starts at `top` (exclusive of the
        // guard above) and descends toward `bottom` as the stack grows.
        if bottom <= sp && sp <= top {
            return Some(i);
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lowest 2 MiB region boundary at or below `STACK_TOP` — the L3 table the
    /// main stack's mapping populates. Every thread stack must live within it.
    const L3_2MIB: u64 = 0x0020_0000;
    const REGION_BASE: u64 = STACK_TOP - L3_2MIB; // STACK_TOP is 2 MiB-aligned

    #[test]
    fn stacks_are_below_the_main_stack_with_a_guard() {
        // Main stack occupies [STACK_TOP - STACK_PAGES*PAGE, STACK_TOP).
        let main_bottom = STACK_TOP - STACK_PAGES * PAGE;
        let (top0, _bot0) = stack_region(0);
        // Slot 0's top is strictly below the main stack's bottom...
        assert!(top0 < main_bottom, "slot 0 stack overlaps the main stack");
        // ...with exactly one guard page between them.
        assert_eq!(main_bottom - top0, GUARD_PAGES * PAGE);
    }

    #[test]
    fn slots_are_disjoint_and_guard_separated() {
        for i in 0..MAX_THREADS - 1 {
            let (_top_i, bot_i) = stack_region(i);
            let (top_j, _bot_j) = stack_region(i + 1);
            // The next (lower) slot's top is one guard page below this slot's bottom.
            assert!(top_j < bot_i, "slots {i} and {} overlap", i + 1);
            assert_eq!(bot_i - top_j, GUARD_PAGES * PAGE);
        }
    }

    #[test]
    fn whole_pool_fits_one_l3_table() {
        // The lowest mapped byte of the last slot must stay within the L3 region,
        // so no thread stack forces a new page-table (the budget rule).
        let (_top, bottom_last) = stack_region(MAX_THREADS - 1);
        assert!(
            bottom_last >= REGION_BASE,
            "thread stacks spill past the main stack's 2 MiB L3 table \
             ({bottom_last:#x} < {REGION_BASE:#x}) — a map would need a top-up"
        );
        // And the top slot stays below STACK_TOP (never aliases the main stack).
        let (top0, _) = stack_region(0);
        assert!(top0 < STACK_TOP);
    }

    #[test]
    fn tops_are_16_byte_aligned() {
        // AArch64 requires SP 16-aligned at a function entry; page-aligned tops are.
        for i in 0..MAX_THREADS {
            let (top, _) = stack_region(i);
            assert_eq!(top % 16, 0, "slot {i} SP not 16-aligned");
        }
    }

    #[test]
    fn slot_of_sp_inverts_stack_region() {
        // A SP anywhere inside slot `i`'s stack (top, midpoint, one byte above the
        // bottom) maps back to `i` — the inverse the running-thread lookup relies on.
        for i in 0..MAX_THREADS {
            let (top, bottom) = stack_region(i);
            assert_eq!(slot_of_sp(top), Some(i), "slot {i} top misroutes");
            assert_eq!(slot_of_sp(bottom), Some(i), "slot {i} bottom misroutes");
            assert_eq!(slot_of_sp(bottom + (top - bottom) / 2), Some(i));
        }
    }

    #[test]
    fn slot_of_sp_rejects_the_main_stack_and_guards() {
        // The main thread's SP (its stack is [STACK_TOP - STACK_PAGES*PAGE,
        // STACK_TOP), above every pool region) maps to None — the main-thread case.
        let main_bottom = STACK_TOP - STACK_PAGES * PAGE;
        assert_eq!(slot_of_sp(main_bottom), None);
        assert_eq!(slot_of_sp(STACK_TOP - 16), None);
        // A guard page between two slots belongs to neither: slot i's bottom sits
        // one guard page above slot i+1's top, and the byte just below i's bottom
        // is the guard — outside [bottom, top] of both.
        let (_top0, bot0) = stack_region(0);
        assert_eq!(slot_of_sp(bot0 - 1), None, "guard byte routed to a slot");
    }
}
