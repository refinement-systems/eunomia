//! init — the one process the kernel constructs (§1). Holds all initial
//! authority (boot untyped in slot 0) and is the root of every grant in
//! the running system.
//!
//! M3 demo: spawn the embedded hello ELF into its own address space with
//! an explicitly constructed cspace (§5.1) — a bootstrap channel in the
//! child's slot 0 carrying the startup block — and verify the reply.

#![no_std]
#![no_main]

use ipc::sys::{self, EV_READABLE, OBJ_CHANNEL, OBJ_NOTIF, RIGHTS_ALL};
use loader::spawn;

static HELLO_ELF: &[u8] = include_bytes!(env!("HELLO_ELF_PATH"));

// Kernel-bestowed slots (boot convention, main.rs).
const UNTYPED: u32 = 0;

// Our own allocations.
const CHAN_A: u32 = 2;
const CHAN_B: u32 = 3;
const NOTIF: u32 = 4;
const SPAWN_BASE: u32 = 8;

const BIT_REPLY: u64 = 1 << 0;

fn check(r: i64, what: &[u8]) {
    if r < 0 {
        sys::debug_write(b"[init] FAILED: ");
        sys::debug_write(what);
        sys::debug_write(b"\n");
        sys::exit();
    }
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    sys::debug_write(b"[init] up in its own address space\n");

    check(sys::retype(UNTYPED, OBJ_CHANNEL, 4, CHAN_A, CHAN_B), b"channel");
    check(sys::retype(UNTYPED, OBJ_NOTIF, 0, NOTIF, 0), b"notif");
    check(sys::chan_bind(CHAN_A, EV_READABLE, NOTIF, BIT_REPLY), b"bind");

    // Build the child: kernel objects, mapped image, fixed stack.
    let prepared = match spawn::prepare(HELLO_ELF, UNTYPED, SPAWN_BASE, 8) {
        Ok(p) => p,
        Err(_) => {
            sys::debug_write(b"[init] FAILED: prepare\n");
            sys::exit();
        }
    };

    // Explicit cspace construction (§5.1): bootstrap channel in slot 0,
    // startup block queued before the child runs.
    check(sys::chan_send(CHAN_A, b"startup:hello", None), b"startup block");
    check(sys::cap_install(prepared.cspace_slot, CHAN_B, 0), b"install chan");
    check(spawn::start(&prepared, 4).map_or(-1, |_| 0), b"start");

    // Notification-driven wait for the child's reply.
    loop {
        let w = sys::notif_wait(NOTIF);
        check(w, b"wait");
        if w as u64 & BIT_REPLY != 0 {
            break;
        }
    }
    let mut buf = [0u8; 256];
    let (len, _) = sys::chan_recv(CHAN_A, buf.as_mut_ptr(), None);
    check(len, b"recv");

    if &buf[..len as usize] == b"hello-ok" {
        sys::debug_write(b"M3 SPAWN PASS\n");
    } else {
        sys::debug_write(b"M3 SPAWN FAIL\n");
    }
    sys::exit()
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[init] PANIC\n");
    sys::exit()
}
