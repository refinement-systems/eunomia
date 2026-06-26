# 15 — Verus findings: `le-bytes` consumer migration (Phase 2.3)

Date: 2026-06-26. Crates: `cas`, `loader`, `le-bytes`, `ipc`. This is a temporary
intermediate record per CLAUDE.md; it is not referenced from comments, specs, or
guidelines.

## Purpose

Phase 2.3 of `doc/plans/0_verus-improvements.md` migrates the read-direction little-endian
byte machinery from each consumer's local copy to the shared `le-bytes` crate (created in
2.1, alloc-cost measured in 2.2 — finding 14). Before this phase, `cas/src/prolly.rs` and
`loader/src/elf.rs` each carried a byte-identical copy of all six obligations
(`lemma_u{16,32,64}_le_bytes` + `read_u{16,32,64}_le`); the extraction exists precisely to
remove that drift hazard. This report records the two non-mechanical decisions the
migration forced.

## What changed

- `cas` and `loader` each gained a `le-bytes = { path = "../le-bytes" }` dependency, deleted
  their local `u{16,32,64}_le` specs / `lemma_u{16,32,64}_le_bytes` proofs / `read_u{16,32,64}_le`
  readers, and now cite the shared crate's items by **full path** (`le_bytes::u16_le`,
  `le_bytes::read_u16_le`, …) from inside their spec/proof/exec sites — never a top-level
  `use`, per `doc/guidelines/verus.md` §6/§12, so a plain `cargo build` still erases the
  ghost helpers.
- `cas` keeps the items the shared crate intentionally does **not** carry: `read_arr32`
  (cas-only 32-byte digest reader) and the `push_u{16,32,64}_le` writers (whose `ensures`
  now cite `le_bytes::u*_le`, sound because the shared specs are `open`).
- `loader` had no surviving `u*_le`/lemma citations (its only references lived inside the
  deleted readers), so only the 12 `read_u*_le` exec call sites in `parse` were requalified.

## Finding A — ipc's both-direction codec helpers are deliberately *not* migrated

`ipc/src/le_bytes.rs` (`lemma_u{16,32}_le_reassemble` / `lemma_u{16,32}_le_split_bytes`) is
the **both-direction** codec-bijection form: each width states *reassemble-from-split* and
*split-from-reassembled*, the two facts the header/session encode↔decode round-trip proofs
need. The shared `le-bytes` crate is scoped (Phase 2.1 / the plan's "le-bytes scope guard")
to the **read-direction encode-shape only**. Folding ipc's helpers in would either drop one
of the two directions (a §10 coverage loss) or widen the shared crate past its guard. So
ipc is **left unchanged**: no `le-bytes` dependency, no code moved, its helpers stay in
ipc's own `le_bytes` **module**. This is a recorded decision, not an oversight — a future
*third* read-direction copy should be merged into `le-bytes`; the both-direction family
should not. (Note the two distinct things now both named `le_bytes`: the shared crate vs.
ipc's internal module — the ledger's ipc row is clarified accordingly.)

## Finding B — relocation accounting: the raw verified-count sum drops by 6 (dedup), with zero coverage loss

The plan's "relocation nets to zero" rule is about **coverage**, not the raw sum of
per-crate `verification results::` headlines. Verus reports a per-crate own-count (a cold
`-p loader` run prints loader's `12` *and*, on separate lines, its transitively re-verified
deps `ipc 71` / `le-bytes 6` — the headline is not cumulative). Because cas and loader each
previously verified their *own* copy of the same six obligations (12 instances total across
the gate), and those copies now dedup into one shared `le-bytes` copy (6 instances), the
raw sum of headlines drops by 6:

| crate      | before | after | Δ  | note |
|------------|-------:|------:|---:|------|
| `le-bytes` |      — |     6 | +6 | new standalone gate (3 readers + 3 lemmas; the `u*_le` specs are `open` and carry no obligation) |
| `cas`      |     77 |    71 | −6 | local readers/lemmas deleted; cites `le-bytes` |
| `loader`   |     18 |    12 | −6 | local readers/lemmas deleted; cites `le-bytes` |
| `ipc`      |     71 |    71 |  0 | untouched (Finding A) |

This is the intended dedup, **not** a weakening: every read-direction fact is still proven —
once, in `le-bytes` — and cited by both consumers. No spec was weakened, no obligation
dropped, no `ensures` loosened, no input coverage narrowed. The decrement below the prior
cas/loader baselines is sanctioned by this relocation task (the plan's "decrement the
consumer rows" step); the `≥ baseline` regression rule then applies to the updated rows.

## Verification (all cold; `cargo clean -p <crate>` first, a present `verification results::` line = real run)

Verus `0.2026.06.07.cd03505`, toolchain `1.95.0` (the pinned binary).

- `cargo verus verify -p cas --no-default-features` → `le-bytes: 6 verified, 0 errors`
  (alloc prelude) and `cas: 71 verified, 0 errors`.
- `cargo verus verify -p loader --no-default-features` → `le-bytes: 6` (no-alloc),
  `ipc: 71`, `loader: 12` — all `0 errors`.
- `cargo build -p cas -p loader` clean (full-path citations, no top-level `use` of the ghost
  helpers ⇒ they erase).
- `cargo test -p cas` → 133 + 9 + 10 passed, 0 failed (the prolly decode/encode proptests
  exercise the migrated `le_bytes::read_u*_le` / `u*_le` sites).

`le-bytes/src/lib.rs` is byte-identical to its 2.1/2.2 state (no edit), so its six
obligations' `rlimit` under both preludes is exactly as finding 14 measured — no new
`rlimit` is introduced and none is needed.
