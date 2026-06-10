//! Program loader (spec §5): ELF64 parsing (host-testable) and
//! spawn-with-explicit-cspace (target-only, over ipc::sys).
//!
//! The loader maps programs fully — no demand paging, fixed-size stacks
//! with unmapped guard regions below them (§5.3: every fault is a bug).

#![cfg_attr(not(any(feature = "std", test)), no_std)]

pub mod elf;

#[cfg(all(target_arch = "aarch64", target_os = "none"))]
pub mod spawn;
