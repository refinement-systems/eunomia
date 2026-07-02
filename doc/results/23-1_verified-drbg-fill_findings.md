# Findings 23-1 — verified DRBG byte serialization (C1.3, review finding 9)

Task **C1.3** (the optional insert) of `doc/plans/3_plan-std-correction.md`,
acting on the optional half of finding 9 of the independent review
(`doc/results/22_std-port-review.md`): lift the one genuinely-provable piece of the
per-process DRBG — `Drbg::fill`'s little-endian *word serialization* — onto the
existing `le-bytes` spec, while the xoshiro transition and every randomness-quality
property stay trusted (Global decision 2).

**Headline:** `urt/src/random.rs` gains one verified `verus!{}` helper,
`u64_to_le(w) -> [u8; 8]`, proven `r@ == le_bytes::u64_le(w)`; `Drbg::fill` calls
it in place of the unspecced `to_le_bytes()`. urt's verified count rises **29 →
30**, the single new obligation (`urt::random::u64_to_le`) costs **rlimit 13775**,
and **every pre-existing obligation is byte-identical**. No new `external_body`, no
new trusted seam (tally stays 14). Adopted — the proof cost is clean.

## Scope — what is now on the proof surface, what stays off

rev2§5.1 puts DRBG *quality* off the proof surface (documented-predictable MVP
seed). Global decision 2 identifies the one provable piece: turning each generated
`u64` into its 8 little-endian bytes. That byte *layout* is now mechanized against
the shared `le_bytes::u64_le` spec (`le-bytes/src/lib.rs:38-49`). Explicitly **not**
verified, by design: the xoshiro256** transition (`next_u64`), the seed guard, the
chunking loop that walks `out`, the `copy_from_slice` placement, and the
trailing-partial-word truncation — all remain host-tested plain Rust.

## Decision — a per-word serializer citing the shared spec, no seam growth

`Drbg::fill` used `self.next_u64().to_le_bytes()` + `copy_from_slice` over
`chunks_exact_mut(8)`; Verus specs none of `to_le_bytes`/`copy_from_slice`/
`chunks_exact_mut`. The verified equivalent is a small write-direction helper:

```rust
verus! {
fn u64_to_le(w: u64) -> (r: [u8; 8])
    ensures r@ == le_bytes::u64_le(w),
{
    broadcast use vstd::array::group_array_axioms;
    let r: [u8; 8] = [ w as u8, (w >> 8) as u8, /* … */ (w >> 56) as u8 ];
    assert(r@ =~= le_bytes::u64_le(w));
    r
}
} // verus!
```

- **Write direction needs no `by (bit_vector)` lemma.** `le_bytes::u64_le` is
  `open` and defined in shift-extraction form, so the array built from `(w >> 8k)
  as u8` matches it by extensional `=~=` directly — cheaper than the *reader*
  (`read_u64_le`), which needs `lemma_u64_le_bytes` to bridge its bit-construction
  form back to the spec. This is exactly the cas `push_u64_le`
  (`cas/src/prolly.rs:806-819`) / ipc `Header::encode` (`ipc/src/header.rs:90-110`)
  byte-image pattern; urt cites `le_bytes::u64_le` by full path, the established
  reuse of the shared `open` spec.
- **`urt` gains the `le-bytes` path-dep.** no_std + no-alloc with the same pinned
  vstd, so it rides into the userspace cross-build unchanged and stays no_std even
  when urt links std under `cargo test` (the cas/loader precedent).
- **The only `fill` edit** swaps `to_le_bytes()` for `u64_to_le(...)`; the chunking
  loop is untouched. A Verus exec fn erases to a normal fn, so plain-Rust `fill` and
  the `#[cfg(test)]` DRBG tests call it unchanged.

### Rejected alternatives

- **Lift the entire `fill` chunking loop into `verus!{}`** (whole-buffer
  concatenation postcondition). This is *feasible* — bare `&mut [u8]` element writes
  verify in this Verus (`out[i] = v` gives `out@ == old(out)@.update(i, v)`;
  precedent `kcore/src/aspace.rs::alloc_table`, loop shape
  `freelist/src/lib.rs:196-212`) — but it forces `next_u64` to become
  `#[verifier::external_body]`, **adding an `external_body` trusted-seam
  annotation** where the ledger's entropy routing note today states the DRBG "adds
  no `external_body`, no new seam, the tally stays 14". Growing the trusted base
  cuts against the whole point of this correction effort; it also costs more (ghost
  word-seq + recursive concat spec + tail-truncation) for a postcondition **no
  caller consumes** (randomness is off-surface). Rejected on seam growth, not
  feasibility.
- **Put the writer in `le-bytes`** as a `write_u64_le` primitive. Deferred: the plan
  lists "verified write-direction `le-bytes`" as separate deferred work; cas keeps
  its write helpers crate-local citing the shared `open` spec, the pattern followed
  here. Worth extracting only when a second write-direction encoder needs it.

## Gate — commands and result lines

- **Verify (cold, authoritative):** `cargo clean -p urt && cargo verus verify -p
  urt` → `urt` **30 verified, 0 errors** (was 29; the `open` `u64_le` spec adds no
  obligation). The `verification results::` line is present (not stale cache). The
  run re-verifies urt's gated deps transitively — `le-bytes` **6/0**, `ipc` **71/0**,
  `freelist` **30/0** — all unchanged.
- **Proof cost (`scripts/verus-baseline.sh urt`, cold, before vs after):** the
  before-tree rlimit multiset is preserved exactly; the after-tree adds **one**
  obligation, `urt::random::u64_to_le` at **rlimit 13775** (ceiling 1,000,000; for
  scale `slots::alloc_range` is 469,496, the urt TLS `create` was 8,391). Every
  pre-existing obligation's rlimit is byte-identical (empty symmetric difference on
  the non-`u64_to_le` values). Wall-clock is advisory only per `verus.md` §10 and
  moved within noise (differing thread counts). **Clean delta — adopted.**
- **Host tests:** `cargo test -p urt` → `46 passed; 0 failed`, including all 10
  `random::tests` (`fill_serves_any_length` over lengths `[0,1,7,8,9,15,16,17,100]`,
  `fill_locked_happy_path_fills`, the `fill_locked_aborts_when_unseeded`
  should-panic, `never_returns_the_raw_seed`, the determinism/distinctness checks) —
  the erased `u64_to_le` behaves identically to `to_le_bytes()`.
- **Miri (UB):** `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p urt
  random::tests` → `10 passed; 0 failed`, no UB on the changed path; the full urt
  sweep `… miri nextest run -p urt -j4` → `46 tests run: 46 passed, 0 skipped`.
- **Formatting:** `scripts/verusfmt.sh --check` and `cargo fmt --check` both clean
  (the `verus!{}` interior + the plain lines).

## Surface left trusted

The DRBG stays a host-tested plain-Rust boundary except the new byte-image
obligation. `next_u64` (xoshiro transition), the seed guard, the `fill` chunking
loop, `copy_from_slice`, and the tail truncation carry no Verus contract — they are
witnessed by `random::tests` (deterministic-stream, distinct-sub-seeds,
never-returns-raw-seed, all-zero-seed guard, no-seed abort) and the QEMU
`HashMap`-over-the-seed smoke, unchanged. No `external_body` was added, so the §11
tally stays 14.

## For C3 / C6 (count reconciliation, not done here)

Per the plan's ordering, C1.3 does not edit the ledger. The new numbers to consume:

- urt Baseline row (`doc/guidelines/verus_trusted-base.md:649`): **29 → 30**, the
  added obligation `urt::random::u64_to_le` (rlimit 13775).
- Entropy routing note (`:458-471`): C3.4 records the verified `fill` serialization
  and this count. Note its current "adds no `external_body`, no new seam, the tally
  stays 14" remains **true** under this design (the producer adds neither); C3.4
  only adds the verified-serialization line, it does not have to walk back the
  no-seam claim.
- C6.1's `verus-manifest.tsv` pins urt at **30**.

## Follow-ups

None blocking. "Verified write-direction `le-bytes`" (a shared `write_u64_le`)
remains the plan's deferred item — extract `u64_to_le` upward if a second
write-direction encoder appears.
