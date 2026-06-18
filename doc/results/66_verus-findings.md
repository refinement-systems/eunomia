# Verus findings 46 — Phase 8c: `cas::store` replay bound (`replay_bound`)

Plan: `doc/plans/3_verus-rewrite.md` (§4.8) and
`doc/plans/3_verus-rewrite_phase8-detail.md` (§8c). Prior increment: `65` (phase
8b — `advance_head`, the contiguous-flushed-prefix head advance). Phase 8 is the
master plan's one *complement-to-TLA+* target: extract the **pure
recovery-decision core** from `cas::store` and prove it implements the commit
protocol faithfully ∀ inputs, closing the model-to-code gap that the
`CommitProtocol` TLA+ spec (design) and the crash-injection proptest (sampled
bytes) leave open. It is sub-phased by extraction risk 8a→8d; **8c is the phase's
single hardest piece — the replay-bound decision** over the *variable-length* WAL
byte buffer (totality + termination ∀ arbitrary bytes), sequenced after 8a banked
the `store.rs` `verus!{}` block and 8b banked the sequence-reasoning idiom.

**This phase retires nothing.** Kani was fully retired in 7f, and the commit
protocol never had a Kani harness (always too `Vec`/`std`-heavy — CBMC OOM'd).
Verus here is **purely additive** (master plan §4.8): it neither replaces TLA+
(the design gate) nor the proptest (the differential seam) — it closes the gap
*between* them. Both stay.

`cargo verus verify -p cas --no-default-features`: **53 verified, 0 errors** (was
49 in 8b; the +4 covers `decode_frame`, the `wal_content_ok` seam, `replay_bound`,
and the `content_ok_spec` ghost). `cargo test -p cas`: green — the crash-injection
proptests `crash_recovery_preserves_acked_state` / `crash_mid_gc_loses_no_data`,
the `wal_replay_scan` / `mount_recovery` fuzz corpora, and **all ten** `mnt1_*` /
`ovl1_*` forgery regressions (incl. `mnt1_forged_wal_seq_max_rejected`, the
seq-exhaustion gate this phase touched — see §3) run the **rewritten** `mount`
replay loop. `cargo test --workspace --exclude kernel`: green. `cd kernel && cargo
build` (rebuilds the user binaries) + the resulting `user/storaged` aarch64
cross-build: green — the `store.rs` `verus!{}` block still erases into storaged's
no_std build (the standing phase-7 vstd-with-`alloc` risk, re-cleared). `bash
scripts/boot-test.sh`: **BOOT TEST PASS** — the boot-critical recovery path
end-to-end.

---

## 1. The decision, and the protocol fact it realizes

The recovery path is `Store::mount` (mount = crash recovery, §4.5). From the
committed head it reads contiguous, checksummed, seq-continuous WAL records until
the first torn or seq-discontinuous one — anything beyond is an unacked torn tail
(never acked, §4.5). This is the TLA+ `Recover` action. 8c lifts the **bound** of
that walk — *how many* records replay and the byte offset just past them — into a
verified parser core, leaving the overlay apply + the content-level OVL-1 extent
gate as the plain-Rust applier. The 7g `decode_raw` move: a verified parser, a
plain-Rust applier.

**`replay_bound(wal: &[u8], wal_head, wal_next_seq) -> ReplaySpan`** — the verified
form of the `while let Some((rseq, op, rlen)) = WalOp::decode_record(&wal[off..])`
loop. `ReplaySpan { count, end_off }`. `requires wal_head <= wal@.len()` (justified
upstream: `validate_geometry` (7f) ensures `wal_head <= wal_len == wal.len()`; the
plain-Rust `mount` call site trusts it, the 8a/8b precedent). `ensures`:

```
r.end_off <= wal@.len()
```

That single postcondition is the headline: **totality** — no panic / no OOB ∀
bytes — which makes the old `store.rs` `off += rlen` trust-comment ("bounded:
decode_record only matched because off + rlen <= wal.len()") a *theorem*. Combined
with the `decreases wal@.len() - off` measure, replay over an arbitrary/forged WAL
buffer is also proven to **terminate** — a real anti-DoS property (a torn or
adversarial WAL cannot make boot loop forever).

---

## 2. The extraction — a verified `Hash`-free framing parser + the content seam

The 7f/7g/8a/8b split applied to a variable-length parser: the **framing** parse
is verified; the blake3 checksum and the `WalOp` payload decode stay the content
seam.

**`decode_frame(wal, off) -> Option<RecFrame>`** is the `Hash`-free verified
analogue of `WalOp::decode_record`'s header parse (`disk.rs`): `wal.len() - off >=
WAL_HEADER`, per-byte `WAL_MAGIC` compare (`b"WREC" == [0x57,0x52,0x45,0x43]`, the
7f `magic_ok` recipe), read `len` (u32 @ `off+12`) and `seq` (u64 @ `off+4`),
`checked_add` the record length and bounds-check it. The load-bearing `ensures`:

```
r matches Some(f) ==> WAL_HEADER <= f.rlen && off + f.rlen <= wal@.len()
```

The `WAL_HEADER <= rlen` half (record length ≥ a nonzero constant) is what makes
the replay loop's `off += rlen` strictly advance — the termination argument; the
`off + rlen <= wal.len()` half is the in-bounds argument. It **indexes
`wal[off + k]`** (the `disk.rs` byte-reader recipe, reusing `read_u32_le` /
`read_u64_le` — widened to `pub(crate)`) rather than range-slicing, so the proof
stays first-order.

**`wal_content_ok(wal, off, rlen) -> bool`** (`#[verifier::external_body]`) is the
content-layer acceptance `decode_record` makes after framing — the blake3 payload
checksum **and** that the payload decodes to a `WalOp`. Both are out of
verification scope (blake3 is interpreted hashing — the 7f `checksum_ok` seam; the
`WalOp` payload is `Vec`-building content, TLA+'s abstracted record value), so the
seam simply delegates to `WalOp::decode_record(&wal[off..off+rlen]).is_some()` — the
real oracle, on the exact-`rlen` record slice. It carries an `ensures r ==
content_ok_spec(...)` over an uninterpreted ghost `content_ok_spec`, so the
maximal-run characterization (8d) can name "this record is content-valid" without
looking inside the hash or the content decode (the standard trusted-fn-with-spec
idiom). Totality needs no collision-freedom — exactly the boundary 7f drew.

**`mount`'s rewrite — the plain-Rust applier.** The `while let Some(..)` loop
becomes: call `replay_bound` for the span, then re-walk exactly `span.count`
records, re-calling `decode_record` (plain Rust → the full `WalOp`) for the OVL-1
extent gate + `apply_to_overlay` + the `RecMeta` push:

```rust
let span = replay_bound(&wal, sb.wal_head, sb.wal_next_seq);
let mut off = sb.wal_head; let mut seq = sb.wal_next_seq;
for _ in 0..span.count {
    let (_rseq, op, rlen) = WalOp::decode_record(&wal[off as usize..])
        .expect("replay_bound accepted this record");   // 8a unwrap discipline
    if let WalOp::Write { offset, data, .. } = &op { /* OVL-1 — needs decoded content */ }
    store.apply_to_overlay(&op);
    store.wal_records.push_back(RecMeta { seq, off, ref_name: op.ref_name().to_vec(), flushed: false });
    off += rlen as u64;
    seq = seq.checked_add(1).ok_or(StoreError::Corrupt("wal sequence exhausted"))?;
}
store.wal_tail = off; store.wal_seq = seq;
```

The `.expect` is justified by `replay_bound`'s contract — the 8a precedent
(`ra.unwrap()` justified by `pick_survivor`'s `SlotA ==> valid_a`): `replay_bound`
accepted each of the first `span.count` records, so `decode_record` returns `Some`
here. `wal_content_ok`'s exact-`rlen` slice and `mount`'s open `&wal[off..]` slice
read the *same* header bytes (`decode_frame` already bounded `off+rlen`), so they
agree — the `.expect` cannot fire. **OVL-1** and the **seq-exhaustion** `checked_add`
stay byte-identical in the applier (OVL-1 needs the decoded `Write` content; both
gates are content-level), so a `Write` that passes framing/content/seq but fails
OVL-1 still aborts `mount` at that record, exactly as before.

---

## 3. Notes — the seq-exhaustion gate (the one faithfulness corner)

The §4.4 seq-exhaustion forgery gate (`mnt1_forged_wal_seq_max_rejected`) plants a
*validly sealed* record at `seq == u64::MAX` with `wal_next_seq == u64::MAX`, and
`mount` must reject loudly (`Corrupt("wal sequence exhausted")`), not silently drop
it as an unacked tail. The first cut had `replay_bound` *stop before* the boundary
record (to keep its `seq += 1` overflow-free), which dropped it — the regression
test caught it immediately (the master plan §9 "gated by tests, not proofs"
discipline working as intended). The fix: `replay_bound` **counts** the boundary
record (`count += 1; off += frame.rlen`) *before* the `seq == u64::MAX` stop, then
breaks (it just can't advance the sequence past `u64::MAX`). So `mount`'s re-walk
replays it and its `checked_add` fires the gate — original behaviour restored. The
`end_seq`/seq-accounting postcondition was dropped with this change (it would
overflow in exactly this case, and `mount` recomputes `wal_seq` itself via its own
`checked_add`, so the span needn't carry it) — `ReplaySpan` is now `{ count,
end_off }`. The 2^64-record corner remains otherwise vacuous; the gate's behaviour
is the property the test pins.

- **Clean termination + totality; the hard half deferred.** The loop is the
  no-`break`-condition prefix scan (8b / 7g idiom) with `decreases wal@.len() -
  off` and the `count <= off` invariant (each record consumes ≥ 1 byte ⇒
  `count += 1` is overflow-free). The proof discharged at **53 verified, 0 errors**
  with no `by (bit_vector)` and no lemma — the framing reads reuse `disk.rs`'s
  verified `read_u32_le` / `read_u64_le`, and `WAL_HEADER` moved **into** a
  `verus!{}` block (the 7f rule: a `const` outside the macro is opaque to it) so
  `decode_frame` sees its concrete value (= 48).
- **Module location, continued.** 8a/8b parked the block in `store.rs`; 8c adds to
  it (it needs the WAL byte buffer + `WAL_HEADER`, both reachable). No `recovery.rs`
  split was warranted — the surrounding `Store` machinery is untouched.
- **No CI / pinning change.** `cargo verus verify -p cas --no-default-features`
  already runs in the `verus` job (since 7f) with no per-proof filter, so the new
  obligations auto-gate. Verus stays pinned at `0.2026.06.07.cd03505`.

---

## 4. What stays the other tiers' / later sub-phases' job

- **The tight maximal-run equality** — `count ==` the recursive *maximal*
  contiguous seq-run — is **deliberately deferred to 8d** (plan §8c: "per-piece
  contracts before the composed theorem", the §4.8 parallel of phase 6's deferred
  system clause and 8b's deferred head-monotonicity). Stating it needs spec-level
  byte decoding of `seq`/`len` (a `run_len` spec fn over a spec `frame` +
  `content_ok_spec`, with a `by (bit_vector)` bridge from the shift-form LE
  readers — vstd's `spec_u32_from_le_bytes` is a *closed* spec over a 4-byte
  subslice, so it needs the bridge regardless). That is the genuinely hard part the
  plan flagged. `replay_bound`'s loop *does* implement the maximal run faithfully
  (framing + seq-continuity + the content seam per record); only the *closed-form
  equality* is deferred. Faithfulness in the interim is covered exactly where the
  plan places it: the crash-injection proptest (the two halves composed against
  real tree content) and the `wal_replay_scan` / `mount_recovery` fuzz corpora.
- **The content-coverage half of `AckedWritesRecoverable`** — "flushed ⇒ effects
  in the committed root", the last-write-wins semantics TLA+ abstracts to version
  numbers — remains the `CommitProtocol` design gate; the crash-injection proptest
  stays the differential seam (master plan §4.8).

The TLA+↔code correspondence 8c lands is **`Recover → replay_bound`** (adding to
8a's `LiveSlot → pick_survivor` / `Crash`-safety `commit_target` and 8b's
`CommitPrepare.newHead → advance_head`). Phase 8d composes `advance_head ∘
replay_bound` into the gap-freedom round-trip (the code-level shadow of
`AckedWritesRecoverable`) and lands the maximal-run equality the composition needs.
No spec / `CLAUDE.md` / `0_kani-rewrite.md` edits here — those are phase 9 (the
documentation-only closeout).
