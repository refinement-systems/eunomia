# Kani

Kani (the bounded model checker, CBMC underneath) was the kernel's **interim
mechanized-proof tier**. It is **retired**: every target it covered is now proven
*unbounded* in Verus, and its CI job, the pinned `cargo-kani 0.67.0` install, and
the `#[cfg(kani)]` scaffolding are gone. This note exists so a contributor neither
reintroduces Kani by reflex nor assumes it is forbidden when it is genuinely the
right tool.

## What happened

Kani checked the kernel object core (the CDT, untyped retype, channel/notification
rings, thread reports) and the §4.7 host chokepoints (`urt::time` tick→ns,
`urt::slots`, `dma-pool` disjointness, the `ipc` header codec, `cas` TLV +
superblock geometry) at small symbolic bounds — TLC-scale BMC run against the real
implementation. Phase 2 of the Verus rewrite re-proved the kernel core unbounded;
phases 7a–7f re-proved each chokepoint. With 7f the last harness left CBMC and the
`kani` job was removed from CI (`../results/62_verus-findings.md`).

The historical record stays put: the DN-1…DN-14 defect notes and the two
conformance reviews live in `../results/2…18` (the `*_kani-findings*.md` /
`*_kani-review*.md` series), and the original plan is `../plans/0_kani-rewrite.md`.
None of it is edited or deleted — it is the dated account of what the bounded tier
found, and it stands.

## The rule: Verus first

**Kani may not be used where Verus is the better tool.** For this codebase's shape
— a host-buildable `kcore`, explicit `wf()` predicates, the handle/`Store` seam
that keeps hardware out of the proof surface, and no integer→pointer casts in the
core — Verus is *strictly* better for the kernel core and the host chokepoints: it
proves the same properties **for all inputs**, plus **termination** and functional
**`ensures`**, where Kani could only sample inputs up to an `unwind`/bound.
Reintroducing Kani for any code Verus already proves is a regression, not a second
opinion. (The master plan §2, `../plans/3_verus-rewrite.md`, is the authoritative
best-tool table.)

## Where Kani could still earn a place

The bar is high and the default is Verus. Two cases remain legitimate:

- **Fast bounded triage** on *new* code, before its Verus proof is written. Kani
  prints a concrete failing input; Verus prints an SMT context — so a
  counterexample trace can be the quicker route to *finding* a bug, with the Verus
  proof following as the durable artifact. The Kani scaffolding is **removed** once
  that proof lands; it does not accrete into a permanent second harness.
- **Code genuinely intractable for Verus *and* small enough to bound**, where a
  bounded check beats no mechanized check at all. This is rare in this tree — the
  arena rewrite (master plan §3) is precisely what made the core first-order and
  Verus-tractable.

Either way, Kani returns only with a **recorded justification** of why Verus does
not fit — mirroring the project's "best tool for the job, applied honestly." A
silent reintroduction is the failure this note guards against.

## It never competed with the other tiers

Kani's retirement is local to the mechanized-proof tier. It never overlapped with,
and does not affect:

- **TLA+** — the *design* tier (the `CapRevocation` / `CommitProtocol` models):
  finite transition systems, checked before code.
- **Loom / Shuttle** — *concurrency interleavings* (reactor lost-wakeup /
  backpressure, the `urt::time` seqlock). Verus does not touch these either.
- **cargo-fuzz** — *adversarial bytes* (the wire/on-disk decoders, the ELF loader,
  mount over arbitrary device contents — see `fuzzing.md`).

Those three are unchanged by Kani leaving; do not read its retirement as a change
to any of them.
