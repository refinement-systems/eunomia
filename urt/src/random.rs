//! Per-process entropy DRBG (std-port 3.4): the backend for std's `fill_bytes`
//! / `hashmap_random_keys`, and the source a parent draws each child's sub-seed
//! from.
//!
//! Seeded once at bootstrap from the `NAME_RANDOM_SEED` grant (a 256-bit value
//! the parent drew from *its* DRBG; the seed-tree root, `init`, mixes a
//! documented-predictable MVP value from RTC + `CNTVCT`). The generator is
//! **xoshiro256\*\*** — a fast, non-cryptographic PRNG. This is deliberate and
//! disclosed: the QEMU `virt` machine offers no good entropy source (rev2§2.6),
//! so randomness *quality* is MVP-only and is **not** a verification property
//! (only this module's per-word byte serialization — `u64_to_le` against
//! `le_bytes::u64_le` — and the seed *decode* in `loader::startup` are verified).
//! The real source
//! (`RNDR`/virtio-rng) is a later backend swap that changes only the seed bytes'
//! origin, not this DRBG or the per-child-reseed contract.
//!
//! Two invariants hold regardless of source:
//!   - `fill_bytes` never hands back the raw seed — every output is an advanced
//!     xoshiro state word, so a finite seed can never repeat/exhaust silently.
//!   - a parent draws a **fresh sub-seed per child** ([`fresh_seed`]) from its own
//!     stream — the classic fork-without-reseed trap avoided (rev2§5.1: "a parent
//!     draws a fresh seed for every child so siblings never share a stream").
//!
//! No-seed behavior: `fill_bytes` with no seed attached **loudly aborts** (the
//! `urt::time::now_utc_ns` "time page not attached" posture) — a mis-provisioned
//! binary fails visibly, never silently predictable.
//!
//! Concurrency: the process-global generator is guarded by the same
//! Loom-certified [`crate::lock::SpinLock`] the heap uses — mutual exclusion over
//! DRBG state, no wait/wake, so no new interleaving model is needed.
use crate::lock::SpinLock;
use core::cell::UnsafeCell;
// The `verus!{}` island below (`u64_to_le`) needs the prelude; in a plain build
// the macro erases its proof code and the import is otherwise unused (the crate-
// root pattern in `lib.rs`).
#[allow(unused_imports)]
use vstd::prelude::*;

/// A non-all-zero fallback state: xoshiro256\*\* sticks at zero if its whole state
/// is zero, so an all-zero seed is replaced by these fixed splitmix64 constants.
/// The seed origin is documented-predictable MVP anyway, so the guard changes
/// nothing security-relevant — it only keeps the generator from degenerating.
const NONZERO_FALLBACK: [u64; 4] = [
    0x9E37_79B9_7F4A_7C15,
    0xBF58_476D_1CE4_E5B9,
    0x94D0_49BB_1331_11EB,
    0x2545_F491_4F6C_DD1D,
];

#[inline]
fn rotl(x: u64, k: u32) -> u64 {
    x.rotate_left(k)
}

verus! {

/// The 8-byte little-endian image of one generator word — the verified
/// serialization under [`Drbg::fill`]. Proven to equal `le_bytes::u64_le(w)`
/// (the shared little-endian byte-image spec, cited by full path), so the byte
/// *layout* is mechanized while the xoshiro transition and randomness quality
/// stay off the proof surface (rev2§5.1). Write-direction, so no `by (bit_vector)`
/// lemma is needed: `u64_le` is defined in shift-extraction form, and the array
/// built from `(w >> 8k) as u8` matches it by extensional `=~=` (the `cas`
/// `push_u64_le` / `ipc` `Header::encode` byte-image pattern).
fn u64_to_le(w: u64) -> (r: [u8; 8])
    ensures
        r@ == le_bytes::u64_le(w),
{
    broadcast use vstd::array::group_array_axioms;

    let r: [u8; 8] = [
        w as u8,
        (w >> 8) as u8,
        (w >> 16) as u8,
        (w >> 24) as u8,
        (w >> 32) as u8,
        (w >> 40) as u8,
        (w >> 48) as u8,
        (w >> 56) as u8,
    ];
    assert(r@ =~= le_bytes::u64_le(w));
    r
}

} // verus!
/// The xoshiro256\*\* generator state (Blackman & Vigna). Deterministic given a
/// seed; the whole of `urt::random`'s logic lives here so it is directly
/// unit-testable without touching the process-global instance.
#[derive(Clone)]
struct Drbg {
    s: [u64; 4],
}

impl Drbg {
    /// Install a 256-bit seed as the generator state (all-zero → [`NONZERO_FALLBACK`]).
    fn from_seed(seed: [u64; 4]) -> Self {
        let s = if seed == [0, 0, 0, 0] {
            NONZERO_FALLBACK
        } else {
            seed
        };
        Drbg { s }
    }

    /// One xoshiro256\*\* output word; advances the state. The result is computed
    /// from the *current* state and returned *before* the state rolls forward, so
    /// the first draw after a seed is already a scramble of it — never the raw
    /// seed bytes.
    fn next_u64(&mut self) -> u64 {
        let result = rotl(self.s[1].wrapping_mul(5), 7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = rotl(self.s[3], 45);
        result
    }

    /// Fill `out` with generator output (little-endian word order); the trailing
    /// partial word is truncated, so any length is served.
    fn fill(&mut self, out: &mut [u8]) {
        let mut chunks = out.chunks_exact_mut(8);
        for c in &mut chunks {
            c.copy_from_slice(&u64_to_le(self.next_u64()));
        }
        let rem = chunks.into_remainder();
        if !rem.is_empty() {
            let w = u64_to_le(self.next_u64());
            let n = rem.len();
            rem.copy_from_slice(&w[..n]);
        }
    }

    /// Draw a fresh 256-bit sub-seed (four output words) — a child's seed.
    fn fresh_seed(&mut self) -> [u64; 4] {
        [
            self.next_u64(),
            self.next_u64(),
            self.next_u64(),
            self.next_u64(),
        ]
    }
}

/// splitmix64 (the same finalizer that builds `cas`'s FastCDC gear table): widen
/// a single 64-bit value into a full 256-bit seed by taking four successive
/// outputs. The seed-tree root ([`init`]) uses it to expand its few hardware
/// words (RTC seconds, `CNTVCT`, `CNTFRQ`) into a `[u64;4]` for [`seed`].
pub fn expand_seed(scalar: u64) -> [u64; 4] {
    let mut x = scalar;
    let mut out = [0u64; 4];
    for w in out.iter_mut() {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        *w = z ^ (z >> 31);
    }
    out
}

/// The process-global generator. `None` until seeded (all-zero = `None` +
/// unlocked keeps the static in `.bss`, the loader zeroes it with the RW
/// segment — no runtime init). Every access holds `lock`.
struct Rng {
    lock: SpinLock,
    drbg: UnsafeCell<Option<Drbg>>,
}

// SAFETY: `drbg` is only ever reached while `lock` is held (the `urt::Heap`
// posture over its `UnsafeCell` free list — mutual exclusion by the spinlock).
unsafe impl Sync for Rng {}

impl Rng {
    const fn new() -> Self {
        Rng {
            lock: SpinLock::new(),
            drbg: UnsafeCell::new(None),
        }
    }
}

static STATE: Rng = Rng::new();

/// Loud abort when `fill_bytes` runs with no seed attached — factored out so the
/// no-seed policy is unit-testable ([`fill_locked`]) without the global.
#[cold]
fn no_seed_abort() -> ! {
    panic!(
        "urt::random: no entropy seed attached (NAME_RANDOM_SEED grant missing); \
         a HashMap / fill_bytes needs a per-process seed (rev2§5.1)"
    )
}

/// Fill `out` from `slot`, aborting loudly if unseeded. Pure over its argument so
/// both the happy and the no-seed paths test without the process-global `STATE`.
fn fill_locked(slot: &mut Option<Drbg>, out: &mut [u8]) {
    match slot.as_mut() {
        Some(d) => d.fill(out),
        None => no_seed_abort(),
    }
}

/// Seed (or re-seed) the process DRBG. Called once at bootstrap from the
/// `NAME_RANDOM_SEED` grant; the caller zeroes its own seed copy afterwards.
pub fn seed(seed: [u64; 4]) {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    unsafe { *STATE.drbg.get() = Some(Drbg::from_seed(seed)) };
}

/// Whether a seed has been attached (the `NAME_RANDOM_SEED` grant was present).
pub fn is_seeded() -> bool {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    unsafe { (*STATE.drbg.get()).is_some() }
}

/// Fill `out` with random bytes. **Loudly aborts** if no seed is attached — the
/// std `fill_bytes`/`hashmap_random_keys` seam and its infallible contract.
pub fn fill_bytes(out: &mut [u8]) {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    fill_locked(unsafe { &mut *STATE.drbg.get() }, out);
}

/// Draw a fresh 256-bit sub-seed for a child. **Loudly aborts** if unseeded (a
/// parent that spawns must itself be seeded — the fork-without-reseed guard).
pub fn fresh_seed() -> [u64; 4] {
    let _g = STATE.lock.lock();
    // SAFETY: exclusive under `lock`.
    match unsafe { (*STATE.drbg.get()).as_mut() } {
        Some(d) => d.fresh_seed(),
        None => no_seed_abort(),
    }
}

#[cfg(all(test, not(loom), not(shuttle)))]
mod tests {
    use super::*;

    #[test]
    fn deterministic_stream_from_a_fixed_seed() {
        let a = Drbg::from_seed([1, 2, 3, 4]).fresh_seed();
        let b = Drbg::from_seed([1, 2, 3, 4]).fresh_seed();
        assert_eq!(a, b, "same seed must give the same stream");
    }

    #[test]
    fn distinct_seeds_diverge() {
        let a = Drbg::from_seed([1, 2, 3, 4]).fresh_seed();
        let b = Drbg::from_seed([1, 2, 3, 5]).fresh_seed();
        assert_ne!(a, b, "different seeds must give different streams");
    }

    #[test]
    fn never_returns_the_raw_seed() {
        // The first draw must be a scramble of the seed, not the seed bytes.
        let seed = [11u64, 22, 33, 44];
        let first = Drbg::from_seed(seed).fresh_seed();
        assert_ne!(first, seed, "fill_bytes leaked the raw seed");
    }

    #[test]
    fn sub_seeds_are_all_distinct() {
        // A parent drawing several children's seeds gives each a different one.
        let mut d = Drbg::from_seed([9, 8, 7, 6]);
        let s1 = d.fresh_seed();
        let s2 = d.fresh_seed();
        let s3 = d.fresh_seed();
        assert_ne!(s1, s2);
        assert_ne!(s2, s3);
        assert_ne!(s1, s3);
    }

    #[test]
    fn all_zero_seed_does_not_degenerate() {
        // An all-zero seed would freeze xoshiro at zero; the guard prevents it.
        let mut d = Drbg::from_seed([0, 0, 0, 0]);
        assert_ne!(d.next_u64(), 0);
        assert_ne!(d.fresh_seed(), [0, 0, 0, 0]);
    }

    #[test]
    fn fill_serves_any_length() {
        // Non-multiple-of-8 lengths are filled (trailing partial word truncated)
        // and are not left as zeros.
        let mut d = Drbg::from_seed([5, 5, 5, 5]);
        for len in [0usize, 1, 7, 8, 9, 15, 16, 17, 100] {
            let mut buf = vec![0u8; len];
            d.fill(&mut buf);
            if len >= 8 {
                assert!(buf.iter().any(|&b| b != 0), "len {len} left all-zero");
            }
        }
    }

    #[test]
    fn expand_widens_a_scalar_without_collision() {
        let e = expand_seed(0);
        assert_ne!(e, [0, 0, 0, 0]);
        // Distinct scalars give distinct expansions (a weak but useful check).
        assert_ne!(expand_seed(1), expand_seed(2));
    }

    #[test]
    fn fill_locked_happy_path_fills() {
        let mut slot = Some(Drbg::from_seed([1, 1, 1, 1]));
        let mut buf = [0u8; 16];
        fill_locked(&mut slot, &mut buf);
        assert!(buf.iter().any(|&b| b != 0));
    }

    #[test]
    #[should_panic(expected = "no entropy seed attached")]
    fn fill_locked_aborts_when_unseeded() {
        let mut slot: Option<Drbg> = None;
        let mut buf = [0u8; 8];
        fill_locked(&mut slot, &mut buf);
    }

    #[test]
    fn global_seed_then_fill() {
        // The sole global-touching test (the bootstrap-stash precedent): it only
        // ever *seeds*, so it is order-independent (the no-seed abort is covered
        // by `fill_locked_aborts_when_unseeded`, which never touches STATE).
        seed([0xDEAD_BEEF, 0xCAFE_F00D, 1, 2]);
        assert!(is_seeded());
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        fill_bytes(&mut a);
        fill_bytes(&mut b);
        assert_ne!(a, b, "consecutive fills must advance the stream");
    }
}
