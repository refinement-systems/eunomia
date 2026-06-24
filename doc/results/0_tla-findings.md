# TLA+ / TLC optimization findings

*Intermediate working document (doc/results). Records the outcome of each
attempt from `doc/plans/0_tla-optimization.md` so the effort leaves a trail
even when an item turns out to be a null result. Per the project's comment
discipline it is temporary, will be removed, and must not be referenced from
code, specs, or guidelines.*

All measurements below are **cold, single-worker** (`TLC_WORKERS=1`, the
`scripts/tla-baseline.sh` default), `-fp 0 -fpmem 0.5 -coverage 1`, vendored
`tools/tla/tla2tools.jar` (matches its `.sha1`), Temurin 17, host Darwin arm64.
Single worker makes the generated-state count deterministic, which is what lets
the semantics-preserving A/B bar (byte-identical counts) be checked at all.

---

## B1 — `Send`: quantify over `SUBSET cspaces[p]` instead of `SUBSET CapIds`

**Status: adopted, but as a readability/correctness-neutral change — the
anticipated wall-clock win did not materialize (null result on the speed axis).**

### The change

`tla/cap_revocation/CapRevocation.tla`. The `Send` disjunct, in all three
lock-step action bodies (`Next`, `NextBad`, `NextNoGuard`), was:

```tla
\/ \E p \in Procs, ch \in Channels, cs \in (SUBSET CapIds) : Send(p, ch, cs)
```

`Next` enabled `Send` over `cs \in SUBSET CapIds` (all `2^|CapIds| = 16`
subsets) and the `Send` body then discarded everything failing
`cs \subseteq cspaces[p]`. A process can only send its own caps, so it now
quantifies over exactly the enabling assignments:

```tla
\/ \E p \in Procs : \E ch \in Channels, cs \in SUBSET cspaces[p] : Send(p, ch, cs)
```

The nested `\E` is required: the flat `cs \in SUBSET cspaces[p]` does not parse
because `cs`'s bound would reference `p` in the same quantifier list. The
`Send` body is unchanged — `cs /= {}`, `cs \subseteq cspaces[p]` (now a
tautology over the smaller domain, kept as the documented move guard),
`Cardinality(cs) <= MaxCapsPerMsg`, and `Len(queues[ch]) < QueueDepth` all
still filter identically. All three bodies were changed together so the
negative controls stay in lock-step.

### Correctness — the strict semantics-preserving bar (MET, exactly)

`CapRevocation.cfg` (`CapIds={c0,c1,c2,c3} Procs={p0,p1} Channels={ch0}
Threads={t0} Notifs={nf0,nf1} QueueDepth=1`), all 6 safety invariants +
`ReportMonotone` + the `EventuallyRevoked` liveness tableau:

| metric | before (clean) | after (B1) |
|---|---:|---:|
| distinct states | 503,070 | 503,070 |
| generated states | 4,831,322 | 4,831,322 |
| gen:dist | 9.6 | 9.6 |
| diameter | 22 | 22 |
| final `Next` coverage (`distinct:generated`) | 503069:4853388 | 503069:4853388 |
| verdict | No error found | No error found |

distinct, generated, diameter, and the **final** per-action coverage are
**byte-identical** — the strict bar for an expression rewrite. (The
`tla-baseline.sh` "top 8 actions" surfaced *intermediate* `-coverage 1`
snapshots, which are sampled at wall-clock minute boundaries and so differ run
to run by timing; the authoritative final coverage block is identical. The only
other difference is the coverage line reference `292 → 296`, shifted by the
rationale comment added to `Send`.) All invariants and both temporal properties
still pass. SANY parses clean.

Negative controls (`scripts/tla-neg-controls.sh`) — all six still fail as
designed, including the two driven by the changed bodies:

```
ok  CapRevocation_NegControl.cfg    LiveParent violated as expected (exit 12)   # NextBad
ok  CapRevocation_NegLiveness.cfg   EventuallyRevoked violated as expected (13) # NextNoGuard
ok  CommitProtocol_NegControl.cfg   RecoverReconstructs violated as expected
ok  IpcReactor_NegControl.cfg       NoLostWakeup violated as expected
ok  IpcReactor_NegBackpressure.cfg  NoLostWakeupWritable violated as expected
ok  IpcReactor_NegLostWakeup.cfg    NoLostWakeup violated as expected
```

`CapRevocation_Teardown` (`TSpec`) is untouched by B1 and not re-measured.

### Performance — wall-clock (the only axis B1 could move): null result

Three single-worker, full-liveness runs:

| run | wall |
|---|---:|
| before (clean tree) | 07min 36s (456 s) |
| after (B1) | 07min 33s (453 s) |
| after, repeat (B1) | 07min 31s (451 s) |

The before→after delta (−3 s, −0.7 %) is **smaller than the after-vs-after
run-to-run scatter (2 s)**, so it is indistinguishable from noise. B1 is
non-regressing (it provably does strictly less enumeration work, so it cannot
be slower), but it delivers **no measurable speedup** on this host.

### Why the anticipated win did not appear

The plan framed B1 as "a pure time-per-state win on the biggest model's hot
action." Two structural facts make that win negligible here:

1. **TLC attributes all generation cost to a single `Next` action** (coverage
   groups the 10 disjuncts under the `Next` operator's source location). The
   `Send` subset enumeration is one cheap conjunct-chain among ten disjuncts,
   and the per-state cost is dominated by the `RevokeStep`/`Copy` interleavings
   that produce the 9.6:1 generated:distinct blow-up — not by `Send`.
2. **The saving is small even where it applies.** `|CapIds| = 4` so the
   discarded enumeration is at most `16 → 2^|cspaces[p]|` subsets, and in any
   state where a process holds all its caps `cspaces[p] = CapIds`, giving *zero*
   reduction. Each discarded subset only costs a cheap `\subseteq` test, so the
   eliminated work is a tiny fraction of a 456-second run.

Because the change is invisible to every deterministic TLC metric (by
construction the state counts are identical) and below noise on the only
advisory metric, there is no measurable performance basis to *adopt* B1 — nor
any to *reject* it.

### Decision

**Adopted on readability + correctness-preservation grounds, not performance.**
The rewrite expresses the real semantics directly ("a process sends a subset of
its own caps") and removes a huge-set-then-filter idiom; the plan independently
tags B1 *readability: improves*. It preserves coverage exactly (the byte-
identical state counts and every verdict) and keeps the negative controls in
lock-step. Its performance is a wash — recorded honestly as a null result so the
plan's "measure every change" discipline is not overstated into a speedup that
isn't there.

If a future change wants an actual wall-clock win on `CapRevocation.cfg`, the
lever is not `Send`: it is `-workers` (Tier-A A1, already landed: ~4.5× at 8
workers) on the liveness arm, and `SYMMETRY` on a separated safety-only arm
(B2–B4) — symmetry being unsound under the liveness property that dominates this
run.

### Follow-ups (out of scope here)

- **B5** (factor the three `Next`/`NextBad`/`NextNoGuard` bodies through a
  shared `CommonActions`). The plan pairs it with B1; this attempt did B1 alone
  per the request. B5 remains the natural next change on these bodies — and the
  lock-step risk it addresses is exactly why B1 touched all three at once.
- **D1** (16 stray `*_TTrace_*` scratch files in `tla/`) is unrelated hygiene.
