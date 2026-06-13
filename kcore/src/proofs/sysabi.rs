//! Syscall-decode harnesses (plan §4.6). `decode` is pure `u64` reasoning
//! (no pointers, no large arrays), so these run over fully nondeterministic
//! register files and are among the cheapest in the suite (like
//! `check_carve_no_overflow`).

#![cfg(kani)]

use super::bounds::CS_SLOTS;
use super::world::CSpacePool;
use crate::channel::MSG_PAYLOAD;
use crate::cspace::CSpaceObj;
use crate::sysabi::{decode, Sys, SysError, NUM_PRIOS};
use crate::untyped::ObjType;
use core::ptr;

/// Six independent nondet argument registers (`[kani::any(); 6]` would tie all
/// six to one symbolic value).
fn nondet_args() -> [u64; 6] {
    let mut a = [0u64; 6];
    let mut i = 0;
    while i < 6 {
        a[i] = kani::any();
        i += 1;
    }
    a
}

/// `check_decode_total` (plan §4.6, spec §3.7): `decode` never panics or
/// overflows for **any** `(nr, args)`; a known `nr` (0..=23) never reports
/// `UnknownCall`, and any other `nr` always does — an unknown opcode is an
/// error, never a crash. (Panic/overflow freedom is what Kani checks by
/// driving the call; the assertions pin the unknown-opcode contract.)
#[kani::proof]
fn check_decode_total() {
    let nr: u64 = kani::any();
    let a = nondet_args();
    let r = decode(nr, a);
    if nr <= 23 {
        assert!(r != Err(SysError::UnknownCall));
    } else {
        assert!(r == Err(SysError::UnknownCall));
    }
}

/// `check_validate_lengths` (plan §4.6): every value `decode` validates holds
/// on the `Ok` path — message length `<= MSG_PAYLOAD` (so `channel::send`'s
/// `as u16` is lossless), `event <= 2`, `which <= 1`, `prio < NUM_PRIOS`, and
/// a retype's `ty` is a real `ObjType`. Plus the two totality facts the
/// validation rests on: `ObjType::from_u64` is `Some` iff the code is in range,
/// and `CSpaceObj::slot` bounds the index (`null` iff `i >= num_slots`) — the
/// "slot index < cspace size before use" guard behind `cur_slot`.
#[kani::proof]
fn check_validate_lengths() {
    let nr: u64 = kani::any();
    let a = nondet_args();
    if let Ok(sys) = decode(nr, a) {
        match sys {
            // The truncation guard: send's `data.len() as u16` is lossless.
            Sys::ChanSend { len, .. } => assert!(len <= MSG_PAYLOAD as u64),
            Sys::ChanBind { event, .. } => assert!(event <= 2),
            Sys::ThreadBind { which, .. } => assert!(which <= 1),
            Sys::ThreadStart { prio, .. } | Sys::ThreadStartAs { prio, .. } => {
                assert!((prio as usize) < NUM_PRIOS)
            }
            // `ty` is an `ObjType` by construction; just confirm it round-trips.
            Sys::Retype { ty, .. } => assert!(ObjType::from_u64(ty as u64) == Some(ty)),
            _ => {}
        }
    }

    // ObjType::from_u64 totality: Some iff the code names one of the 8 types.
    let v: u64 = kani::any();
    assert!(ObjType::from_u64(v).is_some() == (v < 8));

    // CSpaceObj::slot bounds the index — the guard behind cur_slot's `as u32`
    // (the "slot index < cspace size before use" half of §4.6).
    let mut pool = CSpacePool::new();
    let i: u32 = kani::any();
    unsafe {
        let cs = ptr::addr_of_mut!(pool.obj);
        let slot = CSpaceObj::slot(cs, i);
        assert!(slot.is_null() == (i >= CS_SLOTS));
    }
}
