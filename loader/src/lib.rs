//! Program loader (rev2§5): ELF64 parsing (host-testable) and
//! spawn-with-explicit-cspace (target-only, over ipc::sys).
//!
//! The loader maps programs fully — no demand paging, fixed-size stacks
//! with unmapped guard regions below them (rev2§5.3: every fault is a bug).

#![cfg_attr(not(any(feature = "std", test)), no_std)]

pub mod elf;
pub mod startup;

#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "none", target_os = "eunomia")
))]
pub mod spawn;
