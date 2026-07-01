//! Process bootstrap (rev2§5.1): receive the slot-0 startup block and stash it.
//!
//! The PAL `_start` calls [`init`] once, before `main`. It receives the first
//! message on the bootstrap channel (`grant::BOOTSTRAP_CHANNEL`), runs it through the
//! **verified** `loader::startup::decode` (total over arbitrary bytes, rev2§2.7), and
//! stashes the decoded [`Startup`] for the std `sys/args`/`sys/env` arms and later
//! grant lookups. This is the seam-crate home of the `recv_blocking`-then-`decode`
//! pattern the no_std binaries (`user/hello`, `user/storaged`) open-code in their own
//! `_start`.
//!
//! Trust posture: the untrusted byte boundary is `loader::startup::decode` (verified
//! separately, 1.2); the `chan_recv` is the trusted `svc` shell ([`crate::syscall`]);
//! everything here is plain single-threaded bookkeeping — the same posture the
//! trusted-base ledger already grants [`crate::grant`]. No `verus!{}` obligation.

use crate::grant::BOOTSTRAP_CHANNEL;
use crate::syscall;
use core::ptr::{addr_of, addr_of_mut};
use loader::startup::{self, Startup, MAX_BLOCK};

// The bootstrap block is at most one channel message (`MAX_BLOCK == 256`, rev2§5.1).
// A `'static` buffer so the borrowed argv/env slices in the stashed `Startup` outlive
// `_start` and stay valid for the whole process.
static mut BOOT_BUF: [u8; MAX_BLOCK] = [0; MAX_BLOCK];
static mut BOOT: Option<Startup<'static>> = None;
static mut READY: bool = false;

/// Receive and decode the slot-0 startup block. Idempotent; the PAL `_start` calls
/// this once before `main`.
pub fn init() {
    // SAFETY: single-threaded process bring-up; `READY` makes it init-once.
    if unsafe { *addr_of!(READY) } {
        return;
    }
    let len = recv_bootstrap();
    // SAFETY: `len <= MAX_BLOCK`; runs once, before any reader.
    unsafe { commit(len) };
    // std-port 2.4: attach the pre-mapped grant pages (today only the time page) so
    // the std `sys/time` arm can read them. Must follow `commit` — it reads `startup()`.
    attach_grants();
    // std-port 3.2: if this process was granted the threading self-caps (scoped,
    // opt-in), configure the in-process thread pool. Absent ⇒ threads are
    // `Unsupported` (the least-authority default). Also reads `startup()`.
    configure_threads();
}

/// Attach the kernel-mapped pages the startup block grants. Today only the rev2§2.6
/// time page (`NAME_TIME`): a process granted it can read `SystemTime`; one without
/// it gets urt's loud panic at first `now_utc_ns` (mis-wired, not degraded). Reading
/// `Instant` needs no grant — it reads the counter directly. Target-gated: `urt` is
/// only a dependency on the userspace cross-build (matching [`crate::pal`]).
#[cfg(any(target_os = "eunomia", target_os = "none"))]
fn attach_grants() {
    if let Some(va) = startup().and_then(|s| crate::grant::time_va(s)) {
        // SAFETY: `va` is the base of the read-only `TimePage` granted under
        // `NAME_TIME` (rev2§2.6); it stays mapped for the process lifetime.
        unsafe { urt::time::attach(va as usize) };
    }
}

#[cfg(not(any(target_os = "eunomia", target_os = "none")))]
fn attach_grants() {}

/// Configure the `urt` in-process thread pool from the std-port 3.2 threading
/// self-cap grants (self-aspace/self-cspace/thread-untyped + the free-slot-range
/// base). Present only for a thread-capable process; absent leaves threads
/// unconfigured, so the std `sys/thread` arm refuses `spawn` cleanly. Target-gated
/// like [`attach_grants`] (`urt` is a userspace-cross-build-only dependency).
#[cfg(any(target_os = "eunomia", target_os = "none"))]
fn configure_threads() {
    if let Some((aspace, cspace, untyped, slot_base)) =
        startup().and_then(|s| crate::grant::thread_caps(s))
    {
        urt::thread::configure(
            aspace,
            cspace,
            untyped,
            slot_base,
            urt::thread_layout::WORKING_SLOTS,
        );
    }
}

#[cfg(not(any(target_os = "eunomia", target_os = "none")))]
fn configure_threads() {}

/// Block until the bootstrap message arrives (it is queued before the child starts,
/// so the first `chan_recv` succeeds; the loop is plain defensiveness — the
/// `user/storaged` `recv_blocking` shape). Returns the message length, capped at the
/// buffer size.
fn recv_bootstrap() -> usize {
    let ptr = addr_of_mut!(BOOT_BUF) as *mut u8;
    loop {
        let (n, _) = syscall::chan_recv(BOOTSTRAP_CHANNEL, ptr, None);
        if n >= 0 {
            return (n as usize).min(MAX_BLOCK);
        }
        syscall::yield_now();
    }
}

/// Decode `BOOT_BUF[..len]` and stash the result. `decode` is total, so a malformed
/// block leaves `BOOT == None` (argv/env then read empty) rather than aborting.
///
/// # Safety
/// Must run once, before any [`startup`]/[`argv`]/[`env`] reader; `len <= MAX_BLOCK`.
unsafe fn commit(len: usize) {
    // A `'static` view of the buffer — `BOOT_BUF` lives for the whole program, so the
    // argv/env subranges `decode` borrows out of it are `'static` too.
    // SAFETY: `BOOT_BUF` is a live `[u8; MAX_BLOCK]`; the read is bounded by `len`.
    let block: &'static [u8] =
        unsafe { core::slice::from_raw_parts(addr_of!(BOOT_BUF) as *const u8, MAX_BLOCK) };
    let decoded = startup::decode(&block[..len.min(MAX_BLOCK)]);
    // SAFETY: single-threaded, init-once.
    unsafe {
        *addr_of_mut!(BOOT) = decoded;
        *addr_of_mut!(READY) = true;
    }
}

/// The decoded startup block, or `None` if it has not been received yet or was
/// malformed. Grant lookups (`grant::*`) take `&Startup`, so later PAL arms (stdio
/// slots, the time-page region, the storage root handle) resolve through this.
pub fn startup() -> Option<&'static Startup<'static>> {
    // SAFETY: `BOOT` is written once by `commit` before `main`, read-only after; the
    // raw-pointer deref carries an unconstrained lifetime, valid as `'static` because
    // `BOOT` is a `static`.
    let slot: &'static Option<Startup<'static>> = unsafe { &*addr_of!(BOOT) };
    slot.as_ref()
}

/// The process arguments as raw byte-strings (rev2§5.1: argv is bytes, not UTF-8).
/// Empty before [`init`] or for a malformed block.
pub fn argv() -> &'static [&'static [u8]] {
    match startup() {
        Some(s) => &s.argv[..s.nargv],
        None => &[],
    }
}

/// The process environment as raw `KEY=VALUE` byte-strings (POSIX `environ`
/// convention). Empty before [`init`] or for a malformed block.
pub fn env() -> &'static [&'static [u8]] {
    match startup() {
        Some(s) => &s.env[..s.nenv],
        None => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Write `block` into the bootstrap buffer and run the decode/stash path, the
    // half of `init` that does not issue the `svc` (the real `chan_recv` is the
    // trusted shell, exercised in QEMU). Lets the host test drive `commit` + the
    // accessors over a real EUS1 block.
    unsafe fn test_load(block: &[u8]) {
        let n = block.len().min(MAX_BLOCK);
        // SAFETY: single-threaded test; `n <= MAX_BLOCK`.
        unsafe {
            let buf = &mut *addr_of_mut!(BOOT_BUF);
            buf[..n].copy_from_slice(&block[..n]);
            commit(n);
        }
    }

    // One test only — it mutates the process-global stash, so keep it the sole
    // toucher (asserts the empty pre-load state, then the loaded state).
    #[test]
    fn stashes_decoded_argv_and_env() {
        assert!(startup().is_none());
        assert_eq!(argv(), &[] as &[&[u8]]);
        assert_eq!(env(), &[] as &[&[u8]]);

        let mut s = Startup::new();
        s.push_argv(b"prog").unwrap();
        s.push_argv(b"--flag").unwrap();
        s.push_env(b"KEY=VALUE").unwrap();
        let mut out = [0u8; MAX_BLOCK];
        let n = startup::encode(&s, &mut out).unwrap();

        // SAFETY: this is the only test mutating the global stash.
        unsafe { test_load(&out[..n]) };

        assert!(startup().is_some());
        assert_eq!(argv(), &[&b"prog"[..], &b"--flag"[..]]);
        assert_eq!(env(), &[&b"KEY=VALUE"[..]]);
    }
}
