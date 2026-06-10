//! Program loader — reads ELF images from snapshot handles, constructs
//! address spaces, and spawns processes with an explicit initial cspace
//! (spec §5, M3).
//!
//! M3 work items:
//!   - Parse ELF64 LE for aarch64-unknown-none
//!   - Create address-space object, map PT_LOAD segments from snapshot data
//!   - Map guard page below stack
//!   - Build child cspace with startup block (spec §5.1)
//!   - Issue spawn syscall → process cap returned to caller

fn main() {
    todo!("M3: ELF loader main loop")
}
