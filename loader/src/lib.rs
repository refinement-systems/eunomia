// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! Program loader (rev2§5): ELF64 parsing (host-testable) and
//! spawn-with-explicit-cspace (target-only, over ipc::sys).
//!
//! The loader maps programs fully — no demand paging, fixed-size stacks
//! with unmapped guard regions below them (rev2§5.3: every fault is a bug).

#![cfg_attr(not(any(feature = "std", test)), no_std)]

pub mod elf;
pub mod startup;

#[cfg(bare_metal)]
pub mod spawn;
