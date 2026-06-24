# B4 — channel post-loop lemmas (evaluation)

Task **B4** (ranks 10 + 15, Wave B) from `doc/plans/0_verus-optimization.md`: lift the
heavy post-loop reasoning carried inline by `kcore::channel::recv` (B4a, rank 10) and
`kcore::channel::send` (B4b, rank 15, `dep: B4a`) into tightly-keyed `proof fn`s
(`doc/guidelines/verus.md` §10: decomposition is the default fix; line 1074: one lemma
per post-loop conjunct). Both ops close by `assert(chan_wf(…)) by { … }` and
`assert(ring_fifo(…) =~= …) by { … }`; the `by {}` restricts only what *leaves* the
block — *inside*, the full pass-2 loop invariant (14 clauses in `recv`, 16 in `send`),
the `dests`/`caps` foralls, and the `slot_move` framing are all visible, so each
obligation is solved against the giant per-op query. This file records the per-attempt
evaluation under the plan's §2 protocol. Temporary intermediate report (per `CLAUDE.md`,
not citable from code/specs/guidelines).

- **Kind:** both sub-tasks are optimizations (decompose); clarity is a secondary axis.
  Optimization keep/drop bar (§2): keep only if the target fn **and** the crate SMT
  total measurably drop (rlimit drop decisive); clarity judged on the diff.
- **Host / build:** Darwin arm64, verus `0.2026.06.07.cd03505`, Rust 1.95.0.
- **Method:** cold runs (`cargo clean -p kcore` before each); `cargo verus verify -p
  kcore` for the gate, `--time-expanded --output-json` for timing. The deterministic
  `rlimit` field is the run-independent signal (§2); per-fn wall ms wobbles ±5–15 %, so
  the rlimit carries the claim. **Borderline crate totals were re-measured 3× and
  compared by median** (§2 noise rule) — this mattered here (see Measurement).
- **Baseline.** Developed on the post-B3 branch. `channel.rs` is untouched by B1/B2/B3
  (they edit `thread.rs`/`notification.rs`/`cspace.rs`), but `recv`/`send` *read* cspace
  specs (`chan_wf`, `ring_fifo`, `lemma_ring_msg_eq`, …) that B2/B3 perturbed, so a fresh
  cold pre-B4 baseline was taken rather than reusing the plan's §1 (pre-B1, 391-tree)
  numbers. Median of 3 cold runs: **397 verified, kcore SMT 63 310 ms / rlimit
  164 991 829**, `recv` **1 318 ms / rlimit 3 623 281**, `send` **524 ms / rlimit
  1 621 570**. (The plan's §1 cited `recv` 1 484 / `send` 579 off the pre-B1 tree; lower
  here, consistent with the shrunken post-B1/B2/B3 context — the run-independent rlimit
  is the anchor.)

## The change

Four new `proof fn`s in `channel.rs`, two per op, one per post-loop conjunct (the
`chan_wf` frame and the FIFO `Seq` step). Each is keyed on the cheap shape facts the
op already establishes (the head/count shift, ring-cap/msg-len/bindings-domain frames)
plus the two per-ring-slot facts the loop proved (the dequeued/filled slot's emptiness;
every *other* ring slot untouched), and references **none** of the loop's `dests`/`caps`
quantifiers or `slot_move` residue.

- **B4a — `recv` (rank 10).**
  - `lemma_recv_chan_wf(cv0, cvf, sv0, svf, ch, rr, hh, dd, nn)` — `ensures chan_wf(cvf,
    svf, ch)`. The window *shrinks* by the dequeued head `hh`; the coupling `forall`
    proves each out-of-(new-)window slot empty (head slot emptied by pass 2; every other
    out-of-new slot is out-of-old, hence empty in `sv0` and unchanged).
  - `lemma_recv_fifo_drop_first(cv0, cvf, sv0, svf, ch, rr, hh, dd, nn)` — `ensures
    ring_fifo(cvf[ch], svf, rr) == ring_fifo(cv0[ch], sv0, rr).drop_first()`.
- **B4b — `send` (rank 15).** Symmetric; the window *grows* by the new tail slot `ii`
  (head unchanged):
  - `lemma_send_chan_wf(…, ii)` — `ensures chan_wf(cvf, svf, ch)`.
  - `lemma_send_fifo_push(…, ii)` — `ensures ring_fifo(cvf[ch], svf, rr) ==
    ring_fifo(cv0[ch], sv0, rr).push(ring_msg(cvf[ch], svf, rr, ii))`.

Call-site rewrites in `recv` (post-loop `proof {}`) and `send` (post-loop `proof {}`):

- The inline `assert(chan_wf(…)) by { … }` (the windowing case-split with its `choose`
  witness-shift) and `assert(ring_fifo(…) =~= …) by { … }` (the per-index FIFO step)
  collapse to two `lemma_*` calls, prefaced by the shape/bridging asserts that
  materialise each lemma's `requires` (the loop invariants restated about the post-fire
  `slot_view`, e.g. `svf == sv_loop` in `recv`). The witness-shift `choose` + explicit
  `#![trigger (head+j)%depth]` move verbatim into the lemma bodies (§10 lines 1150–1161).
- The **"other ring untouched"** blocks (`recv` 1578–1590, `send` 1097–1109) are left
  inline — they are small and are rank 26 (C2a)'s `lemma_ring_fifo_frame` target, not
  B4's. The `send` source-emptied `ensures` (caps moved out) rides on the loop invariant,
  untouched.
- One `assert forall|c|` inside `lemma_send_fifo_push`/`lemma_recv_fifo_drop_first`
  needed an explicit `#![trigger cv0[ch].ring_cap[(rr, …, c)]]` (Verus could not infer
  one) — a one-line annotation, no proof effect.

No `rlimit`/`spinoff_prover` attributes were involved (`recv`/`send` carry none, unlike
`cdt_unlink`), so there is no last-resort lever to retire here.

## Gate (§2 step 2a — cold, authoritative, whole-crate)

`cargo clean -p kcore && cargo verus verify -p kcore` (after `cargo fmt`) ended

```
verification results:: 401 verified, 0 errors
```

**present** (a real cold run). `N` rose **397 → 401**, **+4**, exactly the four new
`proof fn`s — the predicted delta. **Gate: PASS (Y).** The trusted base is unchanged
(four ordinary verified proofs, no new seam); the ledger kcore row updates 397 → 401.

## Measurement (§2 step 2b — cold timing, 3-run medians vs. the pre-B4 baseline)

| obligation | SMT ms (before → after) | rlimit (before → after) | verdict |
|---|---:|---:|---|
| `channel::recv` (B4a) | 1 318 → **725** (−45 %) | 3 623 281 → **1 727 721** (−52.3 %) | **win** |
| `channel::send` (B4b) | 524 → **396** (−24 %) | 1 621 570 → **1 091 622** (−32.7 %) | **win** |
| 4 new lemmas (combined) | — → 67 | — → 265 786 | new |
| **recv+send+lemmas (combined)** | 1 842 → **1 188** (−35.5 %) | 5 244 851 → **3 085 129** (−41.2 %) | **win** |

Crate (median of 3 cold runs each):

| metric | before | after | ratio |
|---|---:|---:|---:|
| kcore SMT total (ms) | 63 310 | 62 188 | 0.98× (−1.8 %) |
| kcore SMT total (rlimit) | 164 991 829 | 160 357 260 | **0.97× (−2.81 %)** |

The decisive run-independent signal is the two ops' **rlimit collapse**: `recv`
3.62 M → 1.73 M (−52.3 %) and `send` 1.62 M → 1.09 M (−32.7 %), both byte-identical
across all three cold runs — genuine proof-size reductions, not ms noise. The hypothesis
holds: solving `chan_wf`/`ring_fifo` inside each op's full pass-2 context was the
dominant cost; keyed to a fresh solver, the four lemmas together cost 67 ms / 0.27 M
rlimit, against the ~654 ms / ~2.16 M the ops shed.

**Methodology note (why the median matters here).** A single before/after pair *looked
like a regression* — the first cold "after" run reported crate total 63 460 ms vs a
fast "before" outlier of 61 002 ms (+4 %). But the big untouched teardown ops
(`destroy_tcb`, `signal`, `remove_waiter`) have **identical rlimits** before and after
(34 510 921 / 20 909 644 / 19 005 838), proving their multi-second ms swings are pure
wall-clock jitter, not an effect of this change. With 3 cold runs and medians, the crate
total drops on **both** axes (−1.8 % ms, −2.81 % rlimit) — the deterministic rlimit being
the cleaner of the two. This is exactly the §2 "re-measure borderline results, trust the
rlimit" rule earning its place.

## Clarity (§2 step 4)

**Cleaner-to-neutral.** Each op's post-loop block — previously two nested
`assert … by { … }` case-splits embedded in a ~50-line `proof {}` — is now a short list
of shape asserts followed by two named, explicitly-contracted lemma calls; the windowing
`choose` and per-index FIFO reasoning live in self-contained lemmas (`lemma_recv_*` /
`lemma_send_*`) that read as a `recv`/`send` mirror pair. The four lemmas add their
`requires`/`ensures` surface (the §10 decomposition tradeoff: an explicit contract for a
small-context query); net file change is +273 lines. The plan rated B4 clarity
*neutral*; the named, mirrored contracts make it a mild readability win.

## Host tests

kcore has no host unit suite — its verification *is* its test, and the change is confined
to four new `proof fn`s and two `proof {}` call-site swaps (all erase in a normal build),
so exec behaviour is unchanged by construction. `cargo build -p kcore` compiles clean.

## Decision

**KEEP both B4a and B4b.** The optimization asymmetry is satisfied on both required axes
for each: `recv` fell 1 318 → 725 ms / rlimit −52.3 %, `send` fell 524 → 396 ms / rlimit
−32.7 %, and the crate SMT total dropped on both ms (−1.8 %) and the deterministic rlimit
(−2.81 %), while the four new lemmas cost 67 ms / 0.27 M and the diff reads cleaner. The
"other ring untouched" blocks are deliberately left inline for rank 26 (C2a). Gate 401/0.

> verified **Y** (397 → **401**, +4 lemmas) · `recv` **1 318 → 725 ms / rlimit 3 623 281
> → 1 727 721** (−45 % / −52.3 %) · `send` **524 → 396 ms / rlimit 1 621 570 → 1 091 622**
> (−24 % / −32.7 %) · 4 new lemmas **67 ms / 0.27 M rlimit** · combined **−35.5 % ms /
> −41.2 % rlimit** · kcore SMT **63 310 → 62 188 ms (−1.8 %) / rlimit 164.99 M → 160.36 M
> (−2.81 %)** · clarity **cleaner** → **KEEP**
