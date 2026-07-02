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

//! Embedded EL0 test program — an end-to-end exercise of the syscall
//! surface.
//!
//! Linked into the `.user_text` section at the EL0-accessible window
//! (mmu.rs), entered as the initial thread. Acts as a stand-in for init
//! on the test path. The scenario:
//!
//!   1. Thread 1 retypes untyped into notifications, a channel pair, a
//!      second cspace, and a TCB; wires channel events to notification
//!      bits; builds thread 2's cspace explicitly (rev2§5.1) and starts it.
//!   2. T1 sends a message carrying a derived (signal-only) cap; T2 is
//!      woken by the readable→notification binding, receives, and proves
//!      the cap works by signaling through it.
//!   3. T1 queues a second message with another derived cap in flight,
//!      then revokes the parent: the revoke must destroy T2's received
//!      cap AND the queued in-flight cap AND the on-exit binding cap
//!      bound into T2's TCB before start (rev2§2.2, rev2§5.1).
//!   4. T2 verifies both deaths (signal fails; the queued message arrives
//!      with no caps) and reports the verdict over the channel.
//!   5. T1 checks attenuation held, exercises a timer object, then reaps
//!      T2: the post-revoke rebound on-exit binding fires at T2's
//!      thread_exit(42), and read_report returns exited(42).
//!   6. T1 builds a throwaway channel from a carved sub-untyped, binds
//!      both ends' peer-closed events to a separately-funded
//!      notification, and revokes the sub-untyped: whole-object teardown
//!      fires every peer-closed binding before reclamation and the
//!      notification survives (rev2§3.3). Prints "M1 PASS".
//!
//! Constraints: everything reachable from EL0 must live in `.user_text`
//! (helpers force-inlined, no panicking ops, no core::fmt, no implicit
//! memcpy/memset — the compiler-builtins copies live in kernel text).

#![allow(dead_code)]

use core::arch::asm;
use core::mem::MaybeUninit;

pub const SLOT_NONE: u32 = u32::MAX;

const RIGHT_READ: u64 = 1;
const RIGHT_WRITE: u64 = 2;

// T1 (root cspace) slot map. Slots 0..6 are kernel-bestowed boot caps
// (untypeds, thread, device frames, init's aspace) even on the m1-test
// path; the scaffold's own allocations start above them.
const UNTYPED: u64 = 0;
const SELF_TCB: u64 = 1;
// Boot slot 2 is a second untyped (DRAM above the EL0 window). The rev2§3.3
// teardown test funds its notification here so the notification's funder
// is distinct from the channel's — revoking the channel must not reach it.
const UNTYPED2: u64 = 2;
const N1: u64 = 6;
const N2: u64 = 7;
const CHAN_A: u64 = 8;
const CHAN_B: u64 = 9;
const CSPACE2: u64 = 10;
const TCB2: u64 = 11;
const N2_COPY: u64 = 12;
const SEND1: u64 = 13;
const SEND2: u64 = 14;
const TIMER: u64 = 15;
const EXIT_BIND1: u64 = 16;
const EXIT_BIND2: u64 = 17;
const TCB2_WEAK: u64 = 18;
// rev2§3.3 channel whole-object teardown — a self-contained scenario on
// fresh slots, independent of the channel above.
const UA: u64 = 19; // sub-untyped funding the channel ("untyped A")
const PC_NOTIF: u64 = 20; // peer-closed notification, funded from UNTYPED2
const PC_CHAN_A: u64 = 21;
const PC_CHAN_B: u64 = 22;
// The PL011 IRQ-handler cap, a kernel-bestowed boot cap (main.rs
// slot 24, beside its MMIO frame at 23 — above the scaffold's own slots so
// the retypes never collide). N_IRQ is the notification the device IRQ
// signals; carved fresh so its bits don't alias N1's.
const PL011_IRQ: u64 = 24;
const N_IRQ: u64 = 25;

// T2 (cspace2) slot map.
const T2_CHAN: u64 = 0;
const T2_NOTIF: u64 = 1;
const T2_GOT: u64 = 2;
const T2_GOT2: u64 = 3;

// N1 bits.
const BIT_CAP_PROOF: u64 = 1 << 1;
const BIT_A_READABLE: u64 = 1 << 3;
const BIT_TIMER: u64 = 1 << 5;
const BIT_SELF_TEST: u64 = 1 << 6;
const BIT_CHILD_EXIT: u64 = 1 << 7;

const T2_EXIT_STATUS: u64 = 42;
// N2 bits.
const BIT_B_READABLE: u64 = 1 << 0;
const BIT_GO: u64 = 1 << 2;
// PC_NOTIF bits (rev2§3.3 teardown test) — a separate object, so low bits
// are free again. One bit per endpoint's peer-closed binding.
const BIT_PC_A: u64 = 1 << 0;
const BIT_PC_B: u64 = 1 << 1;
// N_IRQ bit — a dedicated notification, so the low bit is free.
const BIT_IRQ: u64 = 1 << 0;

// Per-thread TLS base markers for the TPIDR_EL0 save/restore check. Distinct
// and nonzero (0 is the zeroed-frame default, so nonzero also catches a
// stuck-at-zero restore). Each thread writes its own, and after a context
// switch during which the other thread set a different value re-reads it: a
// mismatch means the kernel failed to save/restore TPIDR_EL0 (rev2§6.1(d)).
const T1_TLS: u64 = 0x1111_1111_1111_1111;
const T2_TLS: u64 = 0x2222_2222_2222_2222;

#[inline(always)]
unsafe fn sys(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: u64;
    asm!(
        "svc #0",
        inout("x0") a0 => ret,
        inout("x1") a1 => _,
        in("x2") a2,
        in("x3") a3,
        in("x4") a4,
        in("x5") a5,
        in("x7") nr,
        options(nostack),
    );
    ret as i64
}

#[inline(always)]
unsafe fn sys2(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> (i64, u64) {
    let ret: u64;
    let ret2: u64;
    asm!(
        "svc #0",
        inout("x0") a0 => ret,
        inout("x1") a1 => ret2,
        in("x2") a2,
        in("x3") a3,
        in("x7") nr,
        options(nostack),
    );
    (ret as i64, ret2)
}

/// Read the EL0 thread pointer (`TPIDR_EL0`), the TLS base. RW at EL0.
#[inline(always)]
unsafe fn get_tpidr() -> u64 {
    let v: u64;
    asm!("mrs {v}, tpidr_el0", v = out(reg) v, options(nomem, nostack));
    v
}

/// Write the EL0 thread pointer (`TPIDR_EL0`).
#[inline(always)]
unsafe fn set_tpidr(v: u64) {
    asm!("msr tpidr_el0, {v}", v = in(reg) v, options(nomem, nostack));
}

#[inline(always)]
unsafe fn putc(c: u8) {
    sys(0, c as u64, 0, 0, 0, 0, 0);
}

#[inline(always)]
unsafe fn debug_write(msg: &[u8]) {
    sys(1, msg.as_ptr() as u64, msg.len() as u64, 0, 0, 0, 0);
}

#[inline(always)]
unsafe fn retype(ut: u64, ty: u64, param: u64, dst: u64, dst2: u64) -> i64 {
    sys(3, ut, ty, param, dst, dst2, 0)
}

#[inline(always)]
unsafe fn cap_copy(src: u64, dst: u64, rights: u64) -> i64 {
    // a[3] = 0xFF: no rev2§5.4 priority-ceiling reduction (kcore `NO_PRIO_CEILING`).
    sys(4, src, dst, rights, 0xFF, 0, 0)
}

#[inline(always)]
unsafe fn cap_copy_prio(src: u64, dst: u64, rights: u64, prio_ceiling: u64) -> i64 {
    sys(4, src, dst, rights, prio_ceiling, 0, 0)
}

#[inline(always)]
unsafe fn cap_revoke(slot: u64) -> i64 {
    // CapRevoke runs one bounded quantum and returns ERR_AGAIN (-12) while
    // descendants remain (rev2§2.2 preemptible/restartable). Loop until the
    // subtree is empty so the `check`-ing call sites see only a terminal status.
    // (No yield needed: T2 is parked; the loop terminates as the subtree shrinks.)
    loop {
        let r = sys(6, slot, 0, 0, 0, 0, 0);
        if r != -12 {
            return r;
        }
    }
}

#[inline(always)]
unsafe fn cap_install(cspace: u64, src: u64, dst_index: u64) -> i64 {
    sys(7, cspace, src, dst_index, 0, 0, 0)
}

#[inline(always)]
unsafe fn chan_send(chan: u64, data: &[u8], caps: *const [u32; 4]) -> i64 {
    sys(
        8,
        chan,
        data.as_ptr() as u64,
        data.len() as u64,
        caps as u64,
        0,
        0,
    )
}

#[inline(always)]
unsafe fn chan_recv(chan: u64, buf: *mut u8, dests: *const [u32; 4]) -> (i64, u64) {
    sys2(9, chan, buf as u64, dests as u64, 0)
}

#[inline(always)]
unsafe fn chan_bind(chan: u64, event: u64, notif: u64, bits: u64) -> i64 {
    sys(10, chan, event, notif, bits, 0, 0)
}

#[inline(always)]
unsafe fn notif_signal(slot: u64, bits: u64) -> i64 {
    sys(11, slot, bits, 0, 0, 0, 0)
}

#[inline(always)]
unsafe fn notif_wait(slot: u64) -> i64 {
    sys(12, slot, 0, 0, 0, 0, 0)
}

#[inline(always)]
unsafe fn thread_start(tcb: u64, cspace: u64, entry: u64, sp: u64, prio: u64, arg: u64) -> i64 {
    sys(13, tcb, cspace, entry, sp, prio, arg)
}

#[inline(always)]
unsafe fn timer_arm(timer: u64, notif: u64, bits: u64, delta: u64) -> i64 {
    sys(14, timer, notif, bits, delta, 0, 0)
}

#[inline(always)]
unsafe fn irq_bind(irq: u64, notif: u64, bits: u64) -> i64 {
    sys(25, irq, notif, bits, 0, 0, 0)
}

#[inline(always)]
unsafe fn irq_ack(irq: u64) -> i64 {
    sys(26, irq, 0, 0, 0, 0, 0)
}

#[inline(always)]
unsafe fn thread_exit(status: u64) -> ! {
    sys(15, status, 0, 0, 0, 0, 0);
    loop {
        asm!("nop");
    }
}

#[inline(always)]
unsafe fn exit() -> ! {
    thread_exit(0)
}

#[inline(always)]
unsafe fn thread_bind(tcb: u64, which: u64, notif: u64, bits: u64) -> i64 {
    sys(21, tcb, which, notif, bits, 0, 0)
}

#[inline(always)]
unsafe fn read_report(tcb: u64) -> (i64, u64, u64) {
    let ret: u64;
    let r1: u64;
    let r2: u64;
    asm!(
        "svc #0",
        inout("x0") tcb => ret,
        out("x1") r1,
        out("x2") r2,
        in("x7") 22u64,
        options(nostack),
    );
    (ret as i64, r1, r2)
}

/// Abort the test with a tagged error marker: "E<tag>!".
#[inline(always)]
unsafe fn check(r: i64, tag: u8) {
    if r < 0 {
        putc(b'E');
        putc(tag);
        putc(b'!');
        putc(b'\n');
        exit();
    }
}

/// Wait until the accumulated notification word contains `bits`.
#[inline(always)]
unsafe fn wait_for(slot: u64, bits: u64, tag: u8) -> u64 {
    let mut acc: u64 = 0;
    loop {
        let w = notif_wait(slot);
        check(w, tag);
        acc |= w as u64;
        if acc & bits != 0 {
            return acc;
        }
    }
}

#[link_section = ".user_text"]
static PING: [u8; 4] = *b"ping";
#[link_section = ".user_text"]
static MORE: [u8; 4] = *b"more";
#[link_section = ".user_text"]
static MSG_PASS: [u8; 8] = *b"M1 PASS\n";
#[link_section = ".user_text"]
static MSG_FAIL: [u8; 8] = *b"M1 FAIL\n";
#[link_section = ".user_text"]
static VERDICT_OK: [u8; 1] = *b"K";
#[link_section = ".user_text"]
static VERDICT_FAIL: [u8; 2] = *b"FF";

const OBJ_CSPACE: u64 = 0;
const OBJ_THREAD: u64 = 1;
const OBJ_CHANNEL: u64 = 2;
const OBJ_NOTIF: u64 = 3;
const OBJ_TIMER: u64 = 4;
const OBJ_UNTYPED: u64 = 7;

// chan_bind event index for the peer-closed signal (rev2§3.3).
const EV_PEER_CLOSED: u64 = 2;

pub const USER_STACK_TOP: u64 = crate::mmu::USER_BASE + crate::mmu::USER_SIZE;
pub const T2_STACK_TOP: u64 = USER_STACK_TOP - 0x1_0000;

#[link_section = ".user_text"]
#[no_mangle]
pub extern "C" fn user_main(_arg: u64) -> ! {
    unsafe {
        putc(b'1'); // marker: thread 1 alive at EL0

        // EL0 TLS base; the kernel must preserve it across context switches
        // (rev2§6.1(d)). Checked after the first handoff to T2, below.
        set_tpidr(T1_TLS);

        check(retype(UNTYPED, OBJ_NOTIF, 0, N1, 0), b'a');
        check(retype(UNTYPED, OBJ_NOTIF, 0, N2, 0), b'b');
        check(retype(UNTYPED, OBJ_CHANNEL, 4, CHAN_A, CHAN_B), b'c');
        check(retype(UNTYPED, OBJ_CSPACE, 16, CSPACE2, 0), b'd');
        check(retype(UNTYPED, OBJ_THREAD, 0, TCB2, 0), b'e');

        check(chan_bind(CHAN_A, 0, N1, BIT_A_READABLE), b'f');
        check(chan_bind(CHAN_B, 0, N2, BIT_B_READABLE), b'g');

        // Build thread 2's world explicitly (rev2§5.1): its channel end and a
        // wait-only notification cap, moved into its private cspace.
        check(cap_copy(N2, N2_COPY, RIGHT_READ), b'h');
        check(cap_install(CSPACE2, CHAN_B, T2_CHAN), b'i');
        check(cap_install(CSPACE2, N2_COPY, T2_NOTIF), b'j');
        // The canonical parent move (rev2§5.1): bind on-exit before start.
        // This first binding is derived from N1, so the step-3 revoke
        // must reach through the TCB slot and clear it.
        check(cap_copy(N1, EXIT_BIND1, RIGHT_WRITE), b'h');
        check(thread_bind(TCB2, 0, EXIT_BIND1, BIT_CHILD_EXIT), b'i');
        check(
            thread_start(
                TCB2,
                CSPACE2,
                (user_thread2 as extern "C" fn(u64) -> !) as usize as u64,
                T2_STACK_TOP,
                4,
                0,
            ),
            b'k',
        );

        // Send a signal-only derivation of N1 (attenuation, rev2§2.3).
        let caps1: [u32; 4] = [SEND1 as u32, SLOT_NONE, SLOT_NONE, SLOT_NONE];
        check(cap_copy(N1, SEND1, RIGHT_WRITE), b'l');
        check(chan_send(CHAN_A, &PING, &caps1), b'm');

        // T2 proves the transferred cap works by signaling through it.
        wait_for(N1, BIT_CAP_PROOF, b'n');
        // TLS: T2 has run and set its own TPIDR_EL0 to T2_TLS in the interval
        // we were parked, so reading back our own value proves the kernel
        // saved/restored TPIDR_EL0 across the switch (rev2§6.1(d)).
        if get_tpidr() != T1_TLS {
            check(-1, b'T');
        }
        putc(b'2'); // marker: cap arrived and was used

        // Queue a second derived cap in flight, then revoke the parent:
        // the revoke must reach into the queue (rev2§2.2).
        let caps2: [u32; 4] = [SEND2 as u32, SLOT_NONE, SLOT_NONE, SLOT_NONE];
        check(cap_copy(N1, SEND2, RIGHT_WRITE), b'o');
        check(chan_send(CHAN_A, &MORE, &caps2), b'p');
        check(cap_revoke(N1), b'q');
        // The revoke just cleared T2's on-exit binding through the TCB
        // slot (it held an N1 descendant). Rebind with a fresh copy while
        // T2 is still parked waiting for GO, so its death notice fires.
        check(cap_copy(N1, EXIT_BIND2, RIGHT_WRITE), b'q');
        check(thread_bind(TCB2, 0, EXIT_BIND2, BIT_CHILD_EXIT), b'q');
        check(notif_signal(N2, BIT_GO), b'r');

        // T2 reports its verdict over the channel (A-readable → N1 bit).
        // The verdict is the message LENGTH (1 = pass, 2 = fail): the
        // payload is never read back, keeping non-inlined core calls out
        // of EL0 code (everything in kernel .text is EL0 execute-never).
        // T2's exit can land in any of the remaining waits' words, so
        // accumulate across them (a wait consumes the whole word).
        let mut seen = wait_for(N1, BIT_A_READABLE, b's');
        let mut buf = MaybeUninit::<[u8; 256]>::uninit();
        let no_dests: [u32; 4] = [SLOT_NONE; 4];
        let (len, mask) = chan_recv(CHAN_A, buf.as_mut_ptr() as *mut u8, &no_dests);
        check(len, b't');
        if len != 1 || mask != 0 {
            debug_write(&MSG_FAIL);
            exit();
        }
        putc(b'3'); // marker: revoke verified by t2

        // The revoked parent cap itself must still work (revoke deletes
        // descendants, not the cap).
        check(notif_signal(N1, BIT_SELF_TEST), b'u');
        seen |= wait_for(N1, BIT_SELF_TEST, b'v');

        // Timer object: deadline signals a bound notification (rev2§2.6).
        check(retype(UNTYPED, OBJ_TIMER, 0, TIMER, 0), b'w');
        check(timer_arm(TIMER, N1, BIT_TIMER, 1_250_000), b'x'); // ~20ms @62.5MHz
        seen |= wait_for(N1, BIT_TIMER, b'y');
        putc(b'4'); // marker: timer fired

        // Reap T2 (rev2§5.1): the rebound on-exit binding delivers the death
        // notice, and the report — recorded by the kernel, not claimed
        // by the child — must read exited(42).
        if seen & BIT_CHILD_EXIT == 0 {
            wait_for(N1, BIT_CHILD_EXIT, b'z');
        }
        let (state, status, _) = read_report(TCB2);
        if state != 1 || status != T2_EXIT_STATUS {
            debug_write(&MSG_FAIL);
            exit();
        }
        // Attenuation gates the report surface (rev2§2.3): a thread cap
        // copy without bind-reports/read-report can neither configure
        // the slots nor read the record. The copy also strictly lowers the
        // rev2§5.4 priority ceiling (`prio_ceiling = 3`, the rev2§2.3 supervision
        // grant) — the witness that the reducing `derive` ABI
        // (`cap_copy_prio`) runs end-to-end; the ceiling is
        // orthogonal to the rights gating checked just below.
        check(
            cap_copy_prio(TCB2, TCB2_WEAK, RIGHT_READ | RIGHT_WRITE, 3),
            b'!',
        );
        let r_bind = thread_bind(TCB2_WEAK, 0, SLOT_NONE as u64, 0);
        let (r_weak, _, _) = read_report(TCB2_WEAK);
        if r_bind >= 0 || r_weak >= 0 {
            debug_write(&MSG_FAIL);
            exit();
        }
        putc(b'5'); // marker: exit report delivered, read, and gated

        // ── Channel whole-object teardown (rev2§3.3) ──────────────────
        // Build a channel from a freshly carved sub-untyped UA and bind
        // BOTH endpoints' peer-closed events to one notification funded
        // from a SEPARATE untyped (UNTYPED2). Revoking UA tears the whole
        // channel down at once — the rev2§3.5 case where a session's funder
        // dies — so every endpoint's peer-closed binding must fire before
        // the channel's memory is reclaimed, and the separately-funded
        // notification must outlive the channel to receive both signals
        // ("teardown always signals", rev2§3.3). This is the runtime witness
        // for the CapRevocation TSpec's ChannelFireSafe property.
        check(retype(UNTYPED, OBJ_UNTYPED, 0x10000, UA, 0), b'H');
        check(retype(UNTYPED2, OBJ_NOTIF, 0, PC_NOTIF, 0), b'I');
        check(retype(UA, OBJ_CHANNEL, 4, PC_CHAN_A, PC_CHAN_B), b'J');
        check(
            chan_bind(PC_CHAN_A, EV_PEER_CLOSED, PC_NOTIF, BIT_PC_A),
            b'K',
        );
        check(
            chan_bind(PC_CHAN_B, EV_PEER_CLOSED, PC_NOTIF, BIT_PC_B),
            b'L',
        );
        check(cap_revoke(UA), b'M');
        // Both fires land in one word: T1 never blocked, so the bits
        // accumulate and the first wait returns the whole word.
        let pc = wait_for(PC_NOTIF, BIT_PC_A | BIT_PC_B, b'N');
        // The torn-down endpoint caps are now dead — a send errors out
        // (rev2§3.3 "afterward a dead endpoint cap yields error returns").
        let no_caps: [u32; 4] = [SLOT_NONE; 4];
        if pc & (BIT_PC_A | BIT_PC_B) != (BIT_PC_A | BIT_PC_B)
            || chan_send(PC_CHAN_A, &PING, &no_caps) >= 0
        {
            debug_write(&MSG_FAIL);
            exit();
        }
        putc(b'6'); // marker: whole-object teardown fired every peer-closed

        // ── Device IRQ → notification (rev2§1, rev2§3.6) ──────────────
        // init holds the PL011 IRQ-handler cap (boot grant, slot 24).
        // Bind it to a fresh notification and let the line fire: the
        // kernel signals that notification from `handle_el0_irq` — the
        // timer's delivery twin, but for a device SPI — masks the line,
        // and `irq_ack` re-enables it. There is no real device to assert
        // the PL011 line here (and the smoke harness has no stdin), so on
        // the m1-test path the kernel software-pends INTID 33 on bind and
        // on ack: two clean deliveries arrive through the real GIC +
        // exception path. The first proves bind → deliver → signal; the
        // second proves `irq_ack` re-enabled the masked line (the rev2§3.6
        // mask-on-deliver / unmask-on-ack cycle). Lost-wakeup-safe either
        // way: if the kick lands before the wait it is a poll-once, if
        // after, a genuine block-then-wake (the timer segment already
        // proved the latter for notifications).
        check(retype(UNTYPED, OBJ_NOTIF, 0, N_IRQ, 0), b'O');
        check(irq_bind(PL011_IRQ, N_IRQ, BIT_IRQ), b'P');
        wait_for(N_IRQ, BIT_IRQ, b'Q'); // delivery 1: bound IRQ signalled it
        check(irq_ack(PL011_IRQ), b'R'); // ack unmasks; the kick re-fires
        wait_for(N_IRQ, BIT_IRQ, b'S'); // delivery 2: ack re-enabled the line
        putc(b'7'); // marker: device IRQ delivered, acked, and re-fired

        putc(b'8'); // marker: TPIDR_EL0 (EL0 TLS) preserved across switches
        debug_write(&MSG_PASS);
        exit();
    }
}

#[link_section = ".user_text"]
#[no_mangle]
pub extern "C" fn user_thread2(_arg: u64) -> ! {
    unsafe {
        // EL0 TLS base, distinct from T1's; must survive context switches
        // (rev2§6.1(d)). Checked after the go-ahead handoff, below.
        set_tpidr(T2_TLS);
        // Woken by the B-readable binding; receive the cap.
        wait_for(T2_NOTIF, BIT_B_READABLE, b'A');
        let mut buf = MaybeUninit::<[u8; 256]>::uninit();
        let dests1: [u32; 4] = [T2_GOT as u32, SLOT_NONE, SLOT_NONE, SLOT_NONE];
        let (len, mask) = chan_recv(T2_CHAN, buf.as_mut_ptr() as *mut u8, &dests1);
        check(len, b'B');
        if mask != 1 {
            check(-1, b'C');
        }
        // Prove the transferred cap works: signal the parent's
        // notification through it.
        check(notif_signal(T2_GOT, BIT_CAP_PROOF), b'D');

        // Wait for the go-ahead (sent after the revoke).
        wait_for(T2_NOTIF, BIT_GO, b'E');
        // TLS: T1 ran with a different TPIDR_EL0 while we were parked; ours
        // must have been restored on the switch back to us (rev2§6.1(d)).
        if get_tpidr() != T2_TLS {
            check(-1, b'U');
        }

        // The received cap must now be dead…
        let r_dead = notif_signal(T2_GOT, BIT_CAP_PROOF);
        // …and the queued message must arrive with its cap slot emptied.
        let dests2: [u32; 4] = [T2_GOT2 as u32, SLOT_NONE, SLOT_NONE, SLOT_NONE];
        let (len2, mask2) = chan_recv(T2_CHAN, buf.as_mut_ptr() as *mut u8, &dests2);

        // Verdict = message length: 1 byte for pass, 2 for fail.
        let ok = r_dead < 0 && len2 >= 0 && mask2 == 0;
        let no_caps: [u32; 4] = [SLOT_NONE; 4];
        let r = if ok {
            chan_send(T2_CHAN, &VERDICT_OK, &no_caps)
        } else {
            chan_send(T2_CHAN, &VERDICT_FAIL, &no_caps)
        };
        check(r, b'G');
        thread_exit(T2_EXIT_STATUS);
    }
}

#[link_section = ".user_text"]
#[no_mangle]
pub extern "C" fn user_idle(_arg: u64) -> ! {
    loop {
        unsafe {
            asm!("wfi");
        }
    }
}
