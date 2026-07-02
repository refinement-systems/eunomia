# Verification: choosing the method

This work is licensed under a [CC0 1.0 Universal](https://creativecommons.org/publicdomain/zero/1.0) license.

This is the dispatcher. The verification *tiering* — which class of claim is
discharged by which method, and why the tiers compose — is rev2§6, which is
authoritative. This note turns that tiering into a problem-shape → tool routing
table and points each entry at the dedicated guideline that teaches the method
in depth:

- **Verus** — deductive proof of extracted functions: `doc/guidelines/verus.md`.
- **TLA+/TLC** — design-level state machines: `doc/guidelines/tla.md`.
- **cargo-fuzz** — adversarial bytes: `doc/guidelines/fuzzing.md`.
- **Loom/Shuttle** — concurrency interleavings: `doc/guidelines/loom.md`.
- **proptest/Miri** — pure policy/schedulers and undefined-behaviour oracles.

Each method proves a different kind of claim; none subsumes another, so the
routing decision is about the *shape of the problem*, not about reaching for the
strongest-sounding tool.

## Routing table — problem shape to method

| The problem is… | Route to | One-line "use this when" |
|---|---|---|
| A function's functional contract, termination, and `wf` invariants must hold *for all inputs* with no bound to pick | **Verus** (`verus.md`) | The claim is about an extracted, pure-ish function and you want an unbounded proof, not a sampled one. |
| A *design-level state machine* — a protocol whose correctness lives in how concurrent actions interleave (revocation/CDT teardown, the storage commit + crash-recovery protocol, the IPC lost-wakeup/backpressure handshake) | **TLA+/TLC** (`tla.md`) | The bug you fear is an interleaving or a missing fairness/guard in the *design*, above the code level; you want exhaustive (bounded) state exploration of the model. |
| *Adversarial bytes* — wire/on-disk decoders, ELF, a mount over arbitrary device contents | **cargo-fuzz** (`fuzzing.md`), paired with a Verus decode-totality/canonical-form proof | Untrusted input crosses a parse boundary. Verus proves decode totality and canonical form; differential/corpus coverage stays fuzzing's. |
| *Concurrency interleavings* in code (lost-wakeup, seqlock torn reads) | **Loom or Shuttle** (scope to the tool — see below), alongside the TLA+ design model | You need to exercise the actual code's interleavings, not just the design model. |
| A *pure policy or scheduler over already-verified ops* — code that decides *when* an effect fires, not *how* it persists or what data it touches (a flush trigger, a dispatch order) | **proptest / Miri** | The underlying ops already carry their `ensures`; you are testing the schedule, so no new proof chokepoint is needed. Miri also serves as the undefined-behaviour oracle for `unsafe`. |

## Routing nuances that decide the call

- **Loom and Shuttle are not interchangeable; scope the claim to what the tool
  models.** Loom enumerates interleavings but does *not* faithfully reorder
  Relaxed atomics — a structure correct via an explicit `Release`/`Acquire` fence
  (with data fields `Relaxed`) is proven correct *via the modeled fence*, not over
  every C11-permitted reordering, and the claim must say so. A module with no
  atomics (synchronizing only through `Mutex`/`Condvar`) gains nothing from Loom's
  weak-memory modeling, so a thread-interleaving checker (Shuttle) is the
  load-bearing tool there. Name the actually-load-bearing tool, not the
  strongest-sounding one. A further hard limit fixes which side a *weak-memory*
  protocol lands on: the Verus ghost atomics under the version pin are
  sequentially-consistent only, with no standalone fence, so a structure that ships
  Relaxed data behind an `Acquire`/`Release` fence (the seqlock shape) cannot be proven
  faithfully in Verus — a SeqCst proof would certify a *different* binary than ships, and
  under SeqCst the structure is trivially correct, so the proof would also dodge the real
  reordering question. It is irreducibly Loom-certifying / Shuttle-smoke. Only when the
  shared state genuinely *is* a SeqCst atomic does the Verus tokenized-state-machine path
  apply (`verus.md`). The seam, model-authoring, negative-control, and reproducibility
  mechanics are `loom.md`'s.

- **A design-level safety invariant can split between the tiers — its per-step inductive
  arm to Verus, its global/liveness arm to the model.** A protocol whose *design* lives in
  TLA+ may still rest on a per-step safety fact the running code can carry as a named
  `ensures` (a FIFO-discipline step, a no-drop step, a write-once monotone step, a "commit
  never targets the live slot" step): that arm is the unbounded deductive twin and moves to
  Verus (`verus.md`), while the global counting / liveness / cross-restart arms — which a
  live-window data structure structurally cannot witness — stay the model's (`tla.md`).
  When a fact moves it must *re-route, not duplicate*: retire or demote the model's copy
  and record the split, so a trust-routed property is never mistaken for a mechanized one
  and two independently-drifting copies never coexist.

- **Provenance, not just concern-class, decides routing.** Adversarial bytes earn
  the decode-totality proof *plus* fuzzing; trusted-provenance input — typed
  interactive input, or a value your own code just produced — earns neither.
  Record the absent overflow/totality guard as a deliberate, documented non-guard
  rather than proving or fuzzing it (a shell's decimal parser may panic on an
  over-`u64::MAX` digit string: a forward note, not a wire decoder). A verified
  gate's postcondition can still keep a *downstream plain-Rust* step total over
  arbitrary device bytes — sequence the gate before the operation
  (`validate_geometry(&sb)?;` ensuring `sb.head <= len`, *then* the
  `buf.rotate_left(sb.head)` that now provably cannot panic) — the proof crosses
  the `verus!{}` boundary by execution order, not by re-verifying the step.

- **A decode-totality proof changes the paired fuzzer's *job*, not just its
  presence.** Once the totality / canonical-form proof lands, the crash the search was
  hunting is gone, and a differential oracle built around the verified decoder inherits
  its exact branch structure — so a small curated seed set that touches every branch and
  equivalence class already saturates coverage. A coverage count that then stays *flat*
  across a long run is evidence of completeness, not a reason to grow the corpus: enrich
  the named seeds rather than committing raw hunt output. The fuzzer stays load-bearing
  (differential and corpus coverage remain its arm), but for a proven-total decoder its
  posture is curated-seed-first; the corpus-management mechanics are `fuzzing.md`'s.

- **Some things have no method and are trusted by construction.** The asm shell
  (boot, MMU/TLB, GIC, MMIO, the one PA→pointer site) is inherently unverifiable
  trusted base; the whole `kcore` split exists to keep it small. Crypto and
  perf inner loops (blake3, the FastCDC gear loop) are out of scope — stub a hash
  with an injective-on-small-inputs ghost where a proof needs one.

## A model or proof is only worth its teeth

Whatever the method, a model or proof earns its keep only when a deliberately
broken variant is *confirmed to fail*. A passing check over a model that cannot
express the defect proves nothing; a control that finds no violation is the
alarm, not the all-clear. For Verus this is the host-test-with-teeth discipline
in `verus.md`; for TLA+ it is the *runnable negative control* — the real action
minus exactly one load-bearing conjunct, asserted to fail and confirmed to reach
a concrete bad state — whose construction, faithfulness traps, and CI wiring are
in `tla.md`; for Loom/Shuttle it is a deliberately broken variant confirmed to
fail — a `--cfg` negative-control that deadlocks, or a dropped-lock red control
that trips the race detector — in `loom.md`. Keep the framing here at the level of
the principle; the per-method "how" lives in each tool's guideline.
