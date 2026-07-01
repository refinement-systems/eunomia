//! The std-PAL entropy bridge (std-port 3.4): thin delegation to `urt::random`,
//! the per-process DRBG (xoshiro256\*\* over the `NAME_RANDOM_SEED` grant). Holds
//! no logic of its own — the `pal` `__eunomia_fill_bytes` shim is a one-line call
//! into here, and here is a one-line call into `urt`. Gated to the
//! eunomia/bare-metal targets (where `urt::random` is the seeded generator), like
//! [`crate::futex`].

#![cfg(any(target_os = "eunomia", target_os = "none"))]

/// Fill `out` with random bytes for std's `fill_bytes`/`hashmap_random_keys`.
/// Loudly aborts if no `NAME_RANDOM_SEED` grant was attached (the `urt::random`
/// no-seed posture) — a mis-provisioned binary fails visibly.
pub fn fill_bytes(out: &mut [u8]) {
    urt::random::fill_bytes(out)
}
