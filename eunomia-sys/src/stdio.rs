//! Bring-up stdio over the kernel debug-log (std-port 2.3).
//!
//! The std PAL's `sys/stdio` arm routes stdout/stderr (and panic last-words) here, and
//! this issues the `DebugWrite` syscall — rev2§7's EL0 debug-print scaffold. That is a
//! disclosed, *temporary* deviation from the rev2§2 capability model (rev2§2.7): an
//! ambient kernel-diagnostic path, default-on for dev images, that no capability gates.
//! Phase 5.1 moves stdout/stdin onto the userspace console channel; only panic
//! last-words stay on this path (rev2§7 C-M9, "kept ... for kernel-internal panic
//! reporting").
//!
//! The kernel rejects a `DebugWrite` whose length exceeds [`DEBUG_WRITE_MAX`] outright
//! — `ERR_FAULT`, writing nothing (`kernel/src/syscall.rs`, the `Sys::DebugWrite` arm)
//! — so [`write`] splits a buffer into chunks no larger than the cap before issuing the
//! syscall. That split re-establishes the kernel's length precondition at the seam (the
//! inverse-leak rule, host-tested below); the PAL arm is then a one-line delegate.

/// The maximum byte length the kernel `DebugWrite` (opcode 1) accepts in one call: it
/// returns `ERR_FAULT` (writing nothing) for anything longer. Kept in lockstep with the
/// `len > 1024` guard in `kernel/src/syscall.rs`'s `Sys::DebugWrite` arm; the
/// `cap_matches_kernel` test pins the value.
///
/// `allow(dead_code)`: consumed by [`write`] (target-gated) and the test module, so a
/// host non-test build sees it unused.
#[allow(dead_code)]
pub const DEBUG_WRITE_MAX: usize = 1024;

/// Split `buf` into the ≤[`DEBUG_WRITE_MAX`]-byte chunks [`write`] issues — the one
/// splitter both the syscall loop and the test exercise, so the test validates the real
/// boundary. An empty `buf` yields no chunks.
///
/// `allow(dead_code)`: as [`DEBUG_WRITE_MAX`].
#[allow(dead_code)]
fn chunks(buf: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    // `.max(1)` keeps this total even if the cap were ever mis-set to 0 (`slice::chunks`
    // panics on a 0 chunk size). DEBUG_WRITE_MAX is 1024, so today it is a no-op guard.
    buf.chunks(DEBUG_WRITE_MAX.max(1))
}

/// Write `buf` to the kernel debug-log in ≤[`DEBUG_WRITE_MAX`]-byte chunks (rev2§7).
/// Best-effort and infallible: the syscall has no backpressure and is a silent no-op
/// when the kernel lacks the `debug-log` feature, so the full length is always reported
/// written (std's `write_all` then never loops). Target-gated because
/// [`crate::syscall::debug_write`] issues the `svc` shell, whose host build is a stub.
#[cfg(any(target_os = "eunomia", target_os = "none"))]
pub fn write(buf: &[u8]) -> usize {
    for chunk in chunks(buf) {
        crate::syscall::debug_write(chunk);
    }
    buf.len()
}

#[cfg(test)]
mod tests {
    use super::{chunks, DEBUG_WRITE_MAX};

    #[test]
    fn cap_matches_kernel() {
        // Lockstep with the `len > 1024` guard in `kernel/src/syscall.rs`'s
        // `Sys::DebugWrite` arm. If the kernel cap ever changes, change both.
        assert_eq!(DEBUG_WRITE_MAX, 1024);
    }

    #[test]
    fn chunks_never_exceed_cap_and_reassemble() {
        // Exhaustive over every length spanning several cap multiples and their
        // boundaries (0, the cap, cap+1, ...): every chunk is non-empty and within the
        // cap, the chunks concatenate back to the input, and the count is the ceiling of
        // len / cap.
        for len in 0..=(3 * DEBUG_WRITE_MAX + 5) {
            let input: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let mut reassembled = Vec::with_capacity(len);
            let mut count = 0usize;
            for chunk in chunks(&input) {
                assert!(
                    chunk.len() <= DEBUG_WRITE_MAX,
                    "chunk over cap at len {len}"
                );
                assert!(!chunk.is_empty(), "empty chunk at len {len}");
                reassembled.extend_from_slice(chunk);
                count += 1;
            }
            assert_eq!(reassembled, input, "reassembly mismatch at len {len}");
            assert_eq!(
                count,
                len.div_ceil(DEBUG_WRITE_MAX),
                "chunk count at len {len}"
            );
        }
    }
}
