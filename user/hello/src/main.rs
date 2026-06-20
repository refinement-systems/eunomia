//! The first program ever spawned by another Eunomia process. Its whole
//! world arrives via the startup convention (rev1§5.1): a bootstrap channel
//! cap in cspace slot 0 with the startup block as the first queued
//! message. It reads the block, replies, exits.

#![no_std]
#![no_main]

use ipc::sys;

const BOOT_CHAN: u32 = 0;

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start() -> ! {
    sys::debug_write(b"[hello] child alive in its own aspace\n");

    let mut buf = [0u8; 256];
    // The startup block was queued before we started, so the first recv
    // succeeds; the loop is plain defensiveness.
    let len = loop {
        let (len, _) = sys::chan_recv(BOOT_CHAN, buf.as_mut_ptr(), None);
        if len >= 0 {
            break len as usize;
        }
        sys::yield_now();
    };

    if &buf[..len] == b"startup:hello" {
        sys::chan_send(BOOT_CHAN, b"hello-ok", None);
    } else {
        sys::chan_send(BOOT_CHAN, b"hello-BAD", None);
    }
    sys::exit()
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    sys::debug_write(b"[hello] PANIC\n");
    sys::thread_exit(sys::STATUS_PANIC)
}
