# Verus findings 44 — Phase 8a: `cas::store` survivor selection (`pick_survivor` + `commit_target`)

Plan: `doc/plans/3_verus-rewrite.md` (§4.8) and
`doc/plans/3_verus-rewrite_phase8-detail.md` (§8a). Prior increment: `63`
(phase 7g — `cas::tlv`, the last §4.7 chokepoint). Phase 8 is the master plan's one
*complement-to-TLA+* target: extract the **pure recovery-decision core** from
`cas::store` and prove it implements the commit protocol faithfully ∀ inputs,
closing the model-to-code gap that the `CommitProtocol` TLA+ spec (design) and the
crash-injection proptest (sampled bytes) leave open. It is sub-phased by extraction
risk (8a→8d); **8a is the smallest, cleanest decision** — the survivor-selection /
commit-target scalar logic — sequenced first to **bank the extraction workflow**
before any sequence (8b `advance_head`) or variable-length-buffer (8c
`replay_bound`) proof rests on it.

**This phase retires nothing.** Kani was fully retired in 7f, and the commit
protocol never had a Kani harness (always too `Vec`/`std`-heavy — CBMC OOM'd, as
`cas::tlv` did). Verus here is **purely additive** (master plan §4.8): it neither
replaces TLA+ (the design gate) nor the proptest (the differential seam) — it
closes the gap *between* them. Both stay.

`cargo verus verify -p cas --no-default-features`: **47 verified, 0 errors** (was
45 in 7g — the +2 is `pick_survivor` + `commit_target`; `live_slot` is a `spec
fn`). `cargo test -p cas`: green — the crash-injection proptests
`crash_recovery_preserves_acked_state` and `crash_mid_gc_loses_no_data` plus the
canonical-form / `mount_recovery` suites all run the **rewritten** `mount`/`commit`
(the `verus!{}` block erases). `cargo test --workspace --exclude kernel`: green.
`cd kernel && cargo build` and a clean `user/storaged` aarch64 rebuild: green — a
**third** `cas` module now carries a `verus!{}` block (after `disk.rs` 7f and
`prolly.rs` 7g) and vstd-with-`alloc` still erases into storaged's no_std
cross-build (the standing phase-7 risk, re-cleared).

---

## 1. The two decisions, and the protocol facts they realize

The recovery path is `Store::mount` (mount = crash recovery, §4.5); its dual is the
write path `Store::commit`. 8a lifts the two **pure scalar decisions** out of them
into a verified core, leaving the surrounding I/O (the `BlockDev` reads/writes, the
two fsync barriers, the chunk store, the prolly tree, `apply_to_overlay`) as
plain-Rust callers — the 7f/7g split (a `Hash`-free verified core, thin delegators).

**`pick_survivor(gen_a, valid_a, gen_b, valid_b) -> Survivor`** — the verified form
of the `match decoded { … }` arms in `mount`. The TLA+ `LiveSlot` / `OlderIsA`: the
valid slot of higher generation. `ensures`, ∀ `(gen, valid)`:

```
(!valid_a && !valid_b) ==> r is Neither
(valid_a && !valid_b)  ==> r is SlotA
(!valid_a && valid_b)  ==> r is SlotB
(valid_a && valid_b)   ==> ((r is SlotA) <==> gen_a >= gen_b)
(r is SlotA) ==> valid_a        (r is SlotB) ==> valid_b
```

It is **total** and **faithful to mount's `>=` tie-break** (slot A wins a tie). The
last two clauses — *a chosen slot is always a valid one* — are what justify the
plain-Rust `unwrap`s at the call site (below). Under distinct generations (every
honest commit bumps `generation` and writes the *other* slot — the TLA+
`GenerationsDistinct`, so two valid slots never share a generation) the `>=` is a
strict `>`, making the choice deterministic; the proof states the general `>=` form
and leaves distinctness as the protocol-level fact the design tier owns.

**`commit_target(sb_in_b) -> Slot`** — the verified form of the A/B target line in
`commit`. Always the **non-live** slot:

```
(r is A) <==> sb_in_b          r != live_slot(sb_in_b)
```

where `live_slot(sb_in_b)` is the ghost model of `Store::sb_in_b` (B iff
`sb_in_b`). So a crash mid-write damages only the slot being written and the last
committed slot survives — the code witness of the TLA+ `Crash` three-outcome safety
(`AtLeastOneValidSlot` preserved **by construction**). Total.

---

## 2. The extraction — in-block enums, plain-Rust delegators

`Survivor { SlotA, SlotB, Neither }` and `Slot { A, B }` are **in-block enums**: an
external enum can't be *constructed* inside `verus!{}` (the 7g `TlvErr`/7f `SbError`
rule), and these are new types anyway, so they simply live in the block. `mount`
and `commit` map them back to the existing control flow in plain Rust.

`mount`'s rewrite keeps the **version-error distinction** (`UnsupportedVersion` vs
`NoSuperblock`, §2.6) as plain Rust — it only shapes the refusal, not the choice —
and drives the choice off `pick_survivor`:

```rust
let (ra, rb) = (Superblock::decode_checked(&buf_a), Superblock::decode_checked(&buf_b));
let valid_a = ra.is_ok();  let valid_b = rb.is_ok();
let gen_a = ra.as_ref().map(|s| s.generation).unwrap_or(0);
let gen_b = rb.as_ref().map(|s| s.generation).unwrap_or(0);
let (sb, sb_in_b) = match pick_survivor(gen_a, valid_a, gen_b, valid_b) {
    Survivor::SlotA => (ra.unwrap(), false),   // SlotA ==> valid_a, so no panic
    Survivor::SlotB => (rb.unwrap(), true),
    Survivor::Neither => return Err(/* WrongVersion(v) ? UnsupportedVersion : NoSuperblock */),
};
```

The decode itself (`Superblock::decode_checked`) and its totality + geometry
validation are **already proven** (phase 7f). The behaviour is identical to the
prior code, arm for arm. `commit` swaps `if self.sb_in_b { SB_A_OFF } else { SB_B_OFF }`
for a `match commit_target(self.sb_in_b)`.

---

## 3. Notes

- **Clean port, no proof gotchas.** Both bodies are a direct `if`/`match` and the
  SMT discharge is immediate — this is the 7f-geometry analogue (pure scalar logic),
  exactly why 8a was chosen to go first. The datatype `!=` in `commit_target`'s
  `r != live_slot(sb_in_b)` ensures verified without help (Verus's built-in spec
  equality on a C-like enum); the `(r is A) <==> sb_in_b` form is the equivalent
  fallback if a future port hits a `!=` snag.
- **Module location (the §8a TBD): `store.rs`'s own new `verus!{}` block.** 8a's
  functions take only scalars — no `RecMeta` (private to `store.rs`) or
  `Superblock` — so the block sits next to its `mount`/`commit` callers, the
  lowest-churn choice. 8b/8c may revisit (e.g. a `recovery.rs`) once `advance_head`
  needs the `RecMeta` sequence; not 8a's concern.
- **No CI / pinning change.** `cargo verus verify -p cas --no-default-features`
  already runs in the `verus` job (since 7f) with no per-proof filter, so the two
  new obligations auto-gate. Verus stays pinned at `0.2026.06.07.cd03505`.

---

## 4. What stays the other tiers' job

The TLA+↔code correspondence 8a lands is `LiveSlot → pick_survivor` and the
`Crash`-safety witness `commit_target`. The **content-coverage half** of
`AckedWritesRecoverable` — "flushed ⇒ effects in the committed root", the
last-write-wins semantics TLA+ abstracts to version numbers — is **deliberately out
of scope** and remains the `CommitProtocol` design gate; the crash-injection
proptest stays the differential seam exercising the two halves composed against real
tree content (master plan §4.8). Phase 8b (`advance_head`) and 8c (`replay_bound`)
extend this core to the head-advance and replay-bound decisions; 8d composes them
into the gap-freedom round-trip. No spec / `CLAUDE.md` / `0_kani-rewrite.md` edits
here — those are phase 9 (documentation-only closeout).
