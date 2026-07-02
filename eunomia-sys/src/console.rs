//! Userspace console stdio.
//!
//! The std PAL's `sys/stdio` arm routes `stdout`/`stdin`/`stderr` here, and this
//! marshals them over the `user/console` channel — the rev2§5.1 capability-routed
//! terminal, replacing the bring-up debug-log path (`crate::stdio`) for
//! all interactive I/O. Panic last-words still ride the debug-log (rev2§7 C-M9): panic
//! reporting must not depend on the console, which may be the very thing that wedged.
//!
//! The console channel is a raw byte pipe (no framing beyond the message boundary): a
//! write is one or more `ChanSend`s of ≤[`CONSOLE_MSG_MAX`] bytes; a read is one
//! `ChanRecv` of the keystroke bytes the console driver forwarded. Unlike the storaged
//! `fs` client there is no versioned connect handshake to run.
//!
//! Trust posture: a **trusted marshalling shell** (the `sys/stdio` posture) over the
//! verified `ipc` channel syscalls — no new logic beyond chunking a write to the
//! kernel's `MSG_PAYLOAD` cap and carrying a read remainder across calls, both
//! host-tested below. The channel is kernel-serialized, so there is no userspace
//! concurrency obligation (unlike the futex path).
//!
//! Backpressure/empty-ring are **yield-polled** — a std process is granted no wake
//! notification for a reactor, and yield-poll is the shipped precedent (the shell's
//! `out()`/`Stdin::getc` and [`crate::fs`]). Power-efficient reactor/timer blocking is
//! a disclosed deferred upgrade.

use crate::grant::Startup;
#[cfg(bare_metal)]
use crate::syscall;
use crate::syscall::SLOT_NONE;

/// The maximum bytes one console message carries: the kernel's channel `MSG_PAYLOAD`
/// (`kcore::channel::MSG_PAYLOAD == 256`, also `ipc::MAX_PAYLOAD`). A write is split
/// into chunks no larger than this; a read receives at most this many bytes. The
/// `cap_matches_kernel` test pins this twin against the real const through the kcore
/// dev-dep — `kcore`/`ipc` are userspace-cross-build-only deps, so this crate keeps a
/// host-visible `usize` twin (the `crate::stdio::DEBUG_WRITE_MAX` posture).
///
/// `allow(dead_code)`: consumed by the target-gated write/read paths and the test
/// module, so a host non-test build sees it unused.
#[allow(dead_code)]
pub const CONSOLE_MSG_MAX: usize = 256;

// ---------------------------------------------------------------------------
// Pure, host-tested helpers — the only non-delegation logic in this arm.
// ---------------------------------------------------------------------------

/// Resolve the three console-channel cspace slots from the decoded startup block,
/// applying the rev2§5.1 stderr fallback: `NAME_STDERR` → else the `stdout` channel →
/// else `SLOT_NONE` (the write path then falls back to the debug-log; the read path
/// reports EOF). Pure over the block (no globals), so it is host-tested without
/// touching the process-global channel state.
///
/// `allow(dead_code)`: called by the target-gated [`attach`] and the test module.
#[allow(dead_code)]
fn resolve(s: &Startup) -> (u32, u32, u32) {
    let stdout = crate::grant::stdout_slot(s);
    (
        crate::grant::stdin_slot(s).unwrap_or(SLOT_NONE),
        stdout.unwrap_or(SLOT_NONE),
        crate::grant::stderr_slot(s).or(stdout).unwrap_or(SLOT_NONE),
    )
}

/// Split `buf` into the ≤[`CONSOLE_MSG_MAX`]-byte chunks a write issues as one
/// `ChanSend` each. An empty `buf` yields no chunks. The one splitter both the syscall
/// loop and the test exercise (the `crate::stdio::chunks` posture).
///
/// `allow(dead_code)`: consumed by the target-gated write path and the test module.
#[allow(dead_code)]
fn chunks(buf: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    // `.max(1)` keeps this total even if the cap were ever mis-set to 0 (`slice::chunks`
    // panics on a 0 chunk size). CONSOLE_MSG_MAX is 256, so today it is a no-op guard.
    buf.chunks(CONSOLE_MSG_MAX.max(1))
}

/// The unread tail of a console message whose length exceeded a `read` caller's buffer.
/// `pos..len` of `buf` are the carried bytes, delivered by the next `read` before any
/// further `ChanRecv`. A `read` never loses input to a short caller buffer.
///
/// Soundness of the process-global instance ([`CARRY`]): std serializes every `stdin`
/// read behind its stdin lock, so there is a single accessor at a time.
///
/// `allow(dead_code)`: exercised by the target-gated read path and the test module.
#[allow(dead_code)]
struct Carry {
    buf: [u8; CONSOLE_MSG_MAX],
    pos: usize,
    len: usize,
}

#[allow(dead_code)]
impl Carry {
    const fn new() -> Carry {
        Carry {
            buf: [0; CONSOLE_MSG_MAX],
            pos: 0,
            len: 0,
        }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.len
    }

    /// Deliver a freshly-received `msg` (≤[`CONSOLE_MSG_MAX`] bytes) to `out`: copy as
    /// much as fits, stash the remainder for the next [`drain`](Self::drain). Returns
    /// the bytes delivered to `out`. Precondition: `self` is empty.
    fn fill_from(&mut self, msg: &[u8], out: &mut [u8]) -> usize {
        let copied = msg.len().min(out.len());
        out[..copied].copy_from_slice(&msg[..copied]);
        let rest = &msg[copied..];
        self.len = rest.len();
        self.pos = 0;
        self.buf[..self.len].copy_from_slice(rest);
        copied
    }

    /// Deliver up to `out.len()` carried bytes into `out`; returns the count.
    fn drain(&mut self, out: &mut [u8]) -> usize {
        let n = (self.len - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        n
    }
}

// ---------------------------------------------------------------------------
// Process-global channel state + the syscall-issuing entry points (target only,
// matching `crate::stdio::write` / `crate::fs`: the host build stubs the `svc` shell).
// ---------------------------------------------------------------------------

#[cfg(bare_metal)]
use core::sync::atomic::{AtomicU32, Ordering};

/// The `stdin`/`stdout`/`stderr` console-channel cspace slots (rev2§5.1), or
/// `SLOT_NONE` when this process was granted no console (a non-interactive child):
/// its writes then fall back to the debug-log and its reads report EOF.
#[cfg(bare_metal)]
static STDIN_CHAN: AtomicU32 = AtomicU32::new(SLOT_NONE);
#[cfg(bare_metal)]
static STDOUT_CHAN: AtomicU32 = AtomicU32::new(SLOT_NONE);
#[cfg(bare_metal)]
static STDERR_CHAN: AtomicU32 = AtomicU32::new(SLOT_NONE);

/// The read-remainder carry ([`Carry`]); guarded by std's stdin lock (single accessor).
#[cfg(bare_metal)]
static mut CARRY: Carry = Carry::new();

/// Resolve the console grants and stash the channel slots, once, at bootstrap (called
/// from [`crate::bootstrap`] after the startup block is decoded, before `main`). A
/// process with no console grant leaves the slots `SLOT_NONE` — the least-authority
/// default (debug-log writes, EOF reads).
#[cfg(bare_metal)]
pub(crate) fn attach(s: &Startup) {
    let (stdin, stdout, stderr) = resolve(s);
    STDIN_CHAN.store(stdin, Ordering::Relaxed);
    STDOUT_CHAN.store(stdout, Ordering::Relaxed);
    STDERR_CHAN.store(stderr, Ordering::Relaxed);
}

/// Write `buf` to `chan` in ≤[`CONSOLE_MSG_MAX`]-byte messages, yield-polling on
/// backpressure (`ERR_FULL`) exactly as the shell's `out()` does. Best-effort and
/// infallible (the debug-log posture): a non-`ERR_FULL` failure drops the chunk rather
/// than loop, and the full length is always reported so std's `write_all` never loops.
/// A `SLOT_NONE` channel (no console granted) falls back to the debug-log.
#[cfg(bare_metal)]
fn write_chan(chan: u32, buf: &[u8]) -> usize {
    if chan == SLOT_NONE {
        return crate::stdio::write(buf);
    }
    for chunk in chunks(buf) {
        while syscall::chan_send(chan, chunk, None) == syscall::ERR_FULL {
            syscall::yield_now();
        }
    }
    buf.len()
}

/// The `sys/stdio` `Stdout` write body: the console `stdout` channel,
/// else the debug-log.
#[cfg(bare_metal)]
pub fn stdout_write(buf: &[u8]) -> usize {
    write_chan(STDOUT_CHAN.load(Ordering::Relaxed), buf)
}

/// The `sys/stdio` `Stderr` write body: the console `stderr` channel
/// (`NAME_STDERR` → else the `stdout` channel, resolved at [`attach`]), else the
/// debug-log. Kept distinct from stdout so diagnostics never enter a pipeline's data.
#[cfg(bare_metal)]
pub fn stderr_write(buf: &[u8]) -> usize {
    write_chan(STDERR_CHAN.load(Ordering::Relaxed), buf)
}

/// The `sys/stdio` `Stdin` read body: block until at least one byte
/// arrives on the console `stdin` channel and deliver up to `buf.len()` of it, carrying
/// any remainder for the next call. Returns `0` (EOF) when no console was granted — the
/// fallback for a child without a `stdin` grant. Serialized by std's stdin lock.
#[cfg(bare_metal)]
pub fn stdin_read(buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let chan = STDIN_CHAN.load(Ordering::Relaxed);
    if chan == SLOT_NONE {
        return 0; // No console granted — EOF.
    }
    // SAFETY: std serializes stdin reads behind its stdin lock, so `CARRY` has a single
    // accessor here. `addr_of_mut!` avoids a reference to the mutable static.
    let carry = unsafe { &mut *core::ptr::addr_of_mut!(CARRY) };
    if !carry.is_empty() {
        return carry.drain(buf);
    }
    let mut scratch = [0u8; CONSOLE_MSG_MAX];
    loop {
        // `chan_recv` requires a 256-byte buffer; `scratch` is exactly CONSOLE_MSG_MAX.
        let (n, _) = syscall::chan_recv(chan, scratch.as_mut_ptr(), None);
        if n > 0 {
            return carry.fill_from(&scratch[..n as usize], buf);
        }
        // ERR_EMPTY (ring drained): yield and retry — a blocking read.
        syscall::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::{chunks, resolve, Carry, CONSOLE_MSG_MAX};
    use crate::grant::{Grant, GrantKind, Startup, NAME_STDERR, NAME_STDIN, NAME_STDOUT};
    use crate::syscall::SLOT_NONE;

    /// Decode a block carrying the given console grants, then `resolve` it.
    fn resolve_grants(grants: &[(u8, u32)]) -> (u32, u32, u32) {
        let mut sb = Startup::new();
        for &(name, slot) in grants {
            sb.push_grant(Grant {
                name,
                kind: GrantKind::CapSlot(slot),
            })
            .expect("within grant budget");
        }
        let mut buf = [0u8; loader::startup::MAX_BLOCK];
        let n = loader::startup::encode(&sb, &mut buf).expect("within block budget");
        let s = loader::startup::decode(&buf[..n]).expect("round-trips");
        resolve(&s)
    }

    #[test]
    fn cap_matches_kernel() {
        // The local `usize` twin must equal the kernel channel `MSG_PAYLOAD` (the
        // per-message cap the console channel enforces), pinned through the kcore dev-dep
        // like `crate::stdio`'s `cap_matches_kernel`.
        assert_eq!(CONSOLE_MSG_MAX as u64, kcore::channel::MSG_PAYLOAD as u64);
    }

    #[test]
    fn resolve_all_three_distinct_slots() {
        // Each name resolves to its own granted slot.
        assert_eq!(
            resolve_grants(&[(NAME_STDIN, 7), (NAME_STDOUT, 8), (NAME_STDERR, 9)]),
            (7, 8, 9)
        );
    }

    #[test]
    fn resolve_stderr_falls_back_to_stdout_channel() {
        // No NAME_STDERR grant → stderr rides the stdout channel (the rev2§5.1 terminal
        // case: one console under both names), not SLOT_NONE.
        assert_eq!(
            resolve_grants(&[(NAME_STDIN, 6), (NAME_STDOUT, 6)]),
            (6, 6, 6)
        );
    }

    #[test]
    fn resolve_no_console_is_all_none() {
        // A child with no console grant: writes fall back to the debug-log, reads EOF.
        assert_eq!(resolve_grants(&[]), (SLOT_NONE, SLOT_NONE, SLOT_NONE));
    }

    #[test]
    fn chunks_never_exceed_cap_and_reassemble() {
        // Exhaustive over lengths spanning several cap multiples: every chunk is
        // non-empty and within the cap, they concatenate back, and the count is the
        // ceiling of len / cap (the `crate::stdio` chunker test at the console cap).
        for len in 0..=(3 * CONSOLE_MSG_MAX + 5) {
            let input: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut reassembled = Vec::with_capacity(len);
            let mut count = 0usize;
            for chunk in chunks(&input) {
                assert!(
                    chunk.len() <= CONSOLE_MSG_MAX,
                    "chunk over cap at len {len}"
                );
                assert!(!chunk.is_empty(), "empty chunk at len {len}");
                reassembled.extend_from_slice(chunk);
                count += 1;
            }
            assert_eq!(reassembled, input, "reassembly mismatch at len {len}");
            assert_eq!(
                count,
                len.div_ceil(CONSOLE_MSG_MAX),
                "chunk count at len {len}"
            );
        }
    }

    #[test]
    fn carry_delivers_prefix_then_remainder() {
        // A message longer than the caller's buffer: the first read gets a prefix, the
        // carried tail comes out across the next reads — no input lost.
        let mut carry = Carry::new();
        let msg: Vec<u8> = (0..10u8).collect();
        let mut out = [0u8; 4];
        assert_eq!(carry.fill_from(&msg, &mut out), 4);
        assert_eq!(out, [0, 1, 2, 3]);
        assert!(!carry.is_empty());
        assert_eq!(carry.drain(&mut out), 4);
        assert_eq!(out, [4, 5, 6, 7]);
        let mut tail = [0u8; 4];
        assert_eq!(carry.drain(&mut tail), 2);
        assert_eq!(&tail[..2], &[8, 9]);
        assert!(carry.is_empty());
    }

    #[test]
    fn carry_fits_leaves_nothing() {
        // A message that fits leaves the carry empty (the common case: buf ≥ one message).
        let mut carry = Carry::new();
        let msg = [1u8, 2, 3];
        let mut out = [0u8; 8];
        assert_eq!(carry.fill_from(&msg, &mut out), 3);
        assert_eq!(&out[..3], &[1, 2, 3]);
        assert!(carry.is_empty());
    }
}
