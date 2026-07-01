//! `eunomia-sys` — the PAL↔kernel seam for the Rust std port (rev2§3.7,
//! rev2§6.1(d)).
//!
//! Three surfaces, with deliberately different trust postures:
//!
//! - [`encode`] — the **verified** core: [`encode::encode`], the inverse of the
//!   kernel's `kcore::sysabi::decode`, turning a typed [`encode::Call`] into the
//!   register file `{nr, a0..a5}`, proven over every `Call` to place each argument in
//!   its register and *refuse* (return `Err`) any out-of-range field the kernel would
//!   reject — so the PAL cannot construct a shape-rejectable syscall (the §11
//!   inverse-leak rule made machine-checked).
//! - [`syscall`] — the trusted shell: the raw `svc #0` register marshalling (the
//!   irreducible inline asm, rev2§6.1(d)) and the typed wrappers the PAL calls, each
//!   running its arguments through the verified [`encode::encode`] before the `svc`.
//! - [`grant`] — a thin named-grant resolver over the `loader::startup` decoder
//!   (rev2§5.1). No new decode logic: the untrusted byte boundary is
//!   `loader::startup::decode` (verified separately); this only reads named grants out
//!   of an already-decoded block.
//! - [`path`] — the second **verified** surface (std-port 4.2): [`path::resolve`]
//!   turns raw `/`-separated `OsStr` bytes into a `.`/`..`-resolved, root-confined
//!   tree-component list, total over all bytes and proven to emit only components
//!   storaged's `validate_name` accepts (so no accepted path can escape the
//!   handle's subtree). Host-buildable, so the `-p eunomia-sys` gate checks it; the
//!   target-gated [`fs`] client calls it.
//!
//! Agreement between this crate's local ABI twin and the kernel's real decoder is
//! pinned by a host round-trip test (`decode(encode(call)) == Ok(call)`), not by a
//! Verus proof — `decode`'s `ensures` are shape-only, so a functional inverse is not
//! provable against it; the test is the cross-check oracle (the `loader::startup`
//! round-trip-proptest tier).
//!
//! Two further surfaces serve the std PAL, both plain Rust (no `verus!{}`) over the
//! verified core:
//!
//! - [`bootstrap`] — receives the slot-0 startup block in `_start` and stashes the
//!   decoded [`grant::Startup`] for the `sys/args`/`sys/env` arms; bookkeeping over
//!   the verified `loader::startup::decode` and the trusted `chan_recv` shell.
//! - [`io_error`] — the proptested map from the syscall ABI `ERR_*` codes to a
//!   std-agnostic error [`io_error::Kind`] the PAL translates into `io::ErrorKind`.
//! - [`pal`] — the `#[no_mangle]` `extern "Rust"` shims the vendored std PAL links
//!   against (the `__rust_alloc` pattern); std cannot depend on this crate directly
//!   because its verified deps pull `vstd` (not sysroot-buildable), so it reaches the
//!   three surfaces above through these one-line delegations.
#![cfg_attr(not(test), no_std)]

// The per-thread TLS block (std-port 3.2) is heap-allocated over the process-global
// allocator; pull `alloc` on the target build (where `tls` is compiled).
#[cfg(any(target_os = "eunomia", target_os = "none"))]
extern crate alloc;

pub mod bootstrap;
pub mod encode;
// The storaged fs client (std-port 4.1); target-gated internally like `pal`
// (it links `storage-server`/`ipc`, target-only deps).
pub mod fs;
// The `sys::futex` bridge (std-port 3.3); target-gated internally like `pal`.
pub mod futex;
pub mod grant;
// Internal: the compile-time `System`-heap reservation size for the `pal` arm.
mod heap;
pub mod io_error;
pub mod pal;
// The **verified** path resolver for the fs client (std-port 4.2): raw
// `/`-separated `OsStr` bytes → a `.`/`..`-resolved, root-confined tree-component
// list. Host-buildable (NOT target-gated) so the `-p eunomia-sys` verus gate
// checks it; the target-gated `fs` arm calls it.
pub mod path;
// The entropy DRBG bridge (std-port 3.4); target-gated internally like `pal`.
pub mod random;
// Internal: the bring-up debug-log stdio chunker for the `pal`/`sys/stdio` arm.
mod stdio;
pub mod syscall;
// The in-process thread bridge (std-port 3.2); target-gated internally like `pal`.
pub mod thread;
// The per-thread TLS block (std-port 3.2); target-gated internally like `pal`.
pub mod tls;
