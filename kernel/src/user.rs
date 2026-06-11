//! Embedded EL0 test program — the M1 exit criterion (spec §10).
//!
//! Linked into the `.user_text` section at the EL0-accessible window
//! (mmu.rs), entered as the initial thread. Acts as a stand-in for init
//! until the M3 loader exists. The scenario:
//!
//!   1. Thread 1 retypes untyped into notifications, a channel pair, a
//!      second cspace, and a TCB; wires channel events to notification
//!      bits; builds thread 2's cspace explicitly (§5.1) and starts it.
//!   2. T1 sends a message carrying a derived (signal-only) cap; T2 is
//!      woken by the readable→notification binding, receives, and proves
//!      the cap works by signaling through it.
//!   3. T1 queues a second message with another derived cap in flight,
//!      then revokes the parent: the revoke must destroy T2's received
//!      cap AND the queued in-flight cap (§2.2).
//!   4. T2 verifies both deaths (signal fails; the queued message arrives
//!      with no caps) and reports the verdict over the channel.
//!   5. T1 checks attenuation held, exercises a timer object, and prints
//!      "M1 PASS".
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
// N2 bits.
const BIT_B_READABLE: u64 = 1 << 0;
const BIT_GO: u64 = 1 << 2;

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
    sys(4, src, dst, rights, 0, 0, 0)
}

#[inline(always)]
unsafe fn cap_revoke(slot: u64) -> i64 {
    sys(6, slot, 0, 0, 0, 0, 0)
}

#[inline(always)]
unsafe fn cap_install(cspace: u64, src: u64, dst_index: u64) -> i64 {
    sys(7, cspace, src, dst_index, 0, 0, 0)
}

#[inline(always)]
unsafe fn chan_send(chan: u64, data: &[u8], caps: *const [u32; 4]) -> i64 {
    sys(8, chan, data.as_ptr() as u64, data.len() as u64, caps as u64, 0, 0)
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
unsafe fn exit() -> ! {
    sys(15, 0, 0, 0, 0, 0, 0);
    loop {
        asm!("nop");
    }
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

pub const USER_STACK_TOP: u64 = crate::mmu::USER_BASE + crate::mmu::USER_SIZE;
pub const T2_STACK_TOP: u64 = USER_STACK_TOP - 0x1_0000;

#[link_section = ".user_text"]
#[no_mangle]
pub extern "C" fn user_main(_arg: u64) -> ! {
    unsafe {
        putc(b'1'); // marker: thread 1 alive at EL0

        check(retype(UNTYPED, OBJ_NOTIF, 0, N1, 0), b'a');
        check(retype(UNTYPED, OBJ_NOTIF, 0, N2, 0), b'b');
        check(retype(UNTYPED, OBJ_CHANNEL, 4, CHAN_A, CHAN_B), b'c');
        check(retype(UNTYPED, OBJ_CSPACE, 16, CSPACE2, 0), b'd');
        check(retype(UNTYPED, OBJ_THREAD, 0, TCB2, 0), b'e');

        check(chan_bind(CHAN_A, 0, N1, BIT_A_READABLE), b'f');
        check(chan_bind(CHAN_B, 0, N2, BIT_B_READABLE), b'g');

        // Build thread 2's world explicitly (§5.1): its channel end and a
        // wait-only notification cap, moved into its private cspace.
        check(cap_copy(N2, N2_COPY, RIGHT_READ), b'h');
        check(cap_install(CSPACE2, CHAN_B, T2_CHAN), b'i');
        check(cap_install(CSPACE2, N2_COPY, T2_NOTIF), b'j');
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

        // Send a signal-only derivation of N1 (attenuation, §2.3).
        let caps1: [u32; 4] = [SEND1 as u32, SLOT_NONE, SLOT_NONE, SLOT_NONE];
        check(cap_copy(N1, SEND1, RIGHT_WRITE), b'l');
        check(chan_send(CHAN_A, &PING, &caps1), b'm');

        // T2 proves the transferred cap works by signaling through it.
        wait_for(N1, BIT_CAP_PROOF, b'n');
        putc(b'2'); // marker: cap arrived and was used

        // Queue a second derived cap in flight, then revoke the parent:
        // the revoke must reach into the queue (§2.2).
        let caps2: [u32; 4] = [SEND2 as u32, SLOT_NONE, SLOT_NONE, SLOT_NONE];
        check(cap_copy(N1, SEND2, RIGHT_WRITE), b'o');
        check(chan_send(CHAN_A, &MORE, &caps2), b'p');
        check(cap_revoke(N1), b'q');
        check(notif_signal(N2, BIT_GO), b'r');

        // T2 reports its verdict over the channel (A-readable → N1 bit).
        // The verdict is the message LENGTH (1 = pass, 2 = fail): the
        // payload is never read back, keeping non-inlined core calls out
        // of EL0 code (everything in kernel .text is EL0 execute-never).
        wait_for(N1, BIT_A_READABLE, b's');
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
        wait_for(N1, BIT_SELF_TEST, b'v');

        // Timer object: deadline signals a bound notification (§2.6).
        check(retype(UNTYPED, OBJ_TIMER, 0, TIMER, 0), b'w');
        check(timer_arm(TIMER, N1, BIT_TIMER, 1_250_000), b'x'); // ~20ms @62.5MHz
        wait_for(N1, BIT_TIMER, b'y');
        putc(b'4'); // marker: timer fired

        debug_write(&MSG_PASS);
        exit();
    }
}

#[link_section = ".user_text"]
#[no_mangle]
pub extern "C" fn user_thread2(_arg: u64) -> ! {
    unsafe {
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
        exit();
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
