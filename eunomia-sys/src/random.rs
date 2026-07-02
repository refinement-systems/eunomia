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

//! The std-PAL entropy bridge: thin delegation to `urt::random`,
//! the per-process DRBG (xoshiro256\*\* over the `NAME_RANDOM_SEED` grant). Holds
//! no logic of its own — the `pal` `__eunomia_fill_bytes` shim is a one-line call
//! into here, and here is a one-line call into `urt`. Gated to the
//! eunomia/bare-metal targets (where `urt::random` is the seeded generator), like
//! [`crate::futex`].

#![cfg(bare_metal)]

/// Fill `out` with random bytes for std's `fill_bytes`/`hashmap_random_keys`.
/// Loudly aborts if no `NAME_RANDOM_SEED` grant was attached (the `urt::random`
/// no-seed posture) — a mis-provisioned binary fails visibly.
pub fn fill_bytes(out: &mut [u8]) {
    urt::random::fill_bytes(out)
}
