# Plan detail: Verus phase 7 — host chokepoints (§4.7)

**Status: proposed.** This is the per-phase detail for the next step of the
Verus rewrite (`doc/plans/3_verus-rewrite.md`). It migrates the proof
*obligation* for the four host-side chokepoint crates from Kani (bounded) to
Verus (unbounded), deletes each subsumed Kani harness in the same PR (§5
discipline), and resolves the one decision the master plan deferred: whether
`cas::disk` is a Kani holdout or Kani retires from the project entirely.

---

## Phase-number reconciliation

The master plan's §7 phasing drifted from the implementation, because what §4.1
folded into one line ("`delete`/`revoke`/`destroy_*`/`obj_unref`") could not be
proven until *every* object type existed — the teardown mutual recursion
`delete → obj_unref → destroy_{cspace,channel,tcb} → delete` spans channel +
thread, and the `refcount_sound` census assembles terms landed across phases
3/4/5. So it grew into a whole **phase 6** (16 findings docs, 41–56). The actual
mapping:

| Plan §7 step | Actual repo phase | Status |
|---|---|---|
| 0 toolchain + pilot (`carve`) | phase 0 | done |
| 1 arena rewrite | phase 1 | done |
| 2 cspace/CDT | phase 2 (2b/2c sub-steps, docs 21–25) | done |
| 3 untyped + channel | phase 3 (docs 26–30) | done |
| 4 notification + thread | phase 4 (+ timer; docs 31–35) | done |
| 5 aspace + sysabi | phase 5 (docs 36–40) | done |
| *(folded into §4.1)* | **phase 6 cross-object teardown + refcount census** (6a–6f, docs 41–56) | done |
| **6 host chokepoints (§4.7)** | **→ phase 7 — NEXT (this doc)** | proposed |
| 7 commit recovery (§4.8) | → phase 8 | pending |
| 8 closeout | → phase 9 | pending |

CLAUDE.md already states this renumber ("§4.7, **phase 7 — next**"). The next
phase is therefore **phase 7: host chokepoints**.

---

## Scope: what exists today

Four `proofs.rs` Kani harness files — the full inventory to migrate or retire:

| Crate | Harnesses | Subject | Kani's *documented* limit (stated in the harness itself) |
|---|---|---|---|
| `urt/src/proofs.rs` | 4 | `slots` free-list; `time` tick→ns | `slots` at CAP=4 / unwind 6; `time` **totality only** — monotonicity "did not terminate in many minutes" (two u128 divisions, `doc/results/8_kani-findings-7.md` SOLVER note) |
| `dma-pool/src/proofs.rs` | 2 | pool alloc/free disjointness | **DN-10**: two-buffer disjointness + alignment round-up bit-blasts CaDiCaL to OOM; only *one* concrete pair is checked |
| `ipc/src/proofs.rs` | 7 | `Header`, `ConnectReq`/`GrantReply`, `Admission` | `Admission` at 3 steps / unwind 4 |
| `cas/src/proofs.rs` | 2 | superblock `validate_geometry` + `decode_checked` | `cas::tlv` **not harnessed at all** — `Vec` parse OOMs CBMC (18.5k VCCs), already fuzz-only |

The two italicised limits — **time monotonicity** and **dma two-buffer
disjointness** — are exactly the "unbounded beats bounded" trophies §4.7
promised. This phase is where that thesis is literally cashed: both are
properties Kani's own harnesses record as intractable, and both become ∀
theorems in Verus.

---

## The enabling concern the master plan did not surface

All four crates **cross-build into aarch64 userspace binaries** — path-deps in
`user/{init,shell,storaged,selftest,hello}` pull in `ipc`, `urt`, `cas`, and
`dma-pool`. Adding `vstd` to them (the `kcore` recipe:
`vstd = { version = "=0.0.0-2026-05-31-0205", default-features = false }`,
`[package.metadata.verus] verify = true`, the `unexpected_cfgs` lint) means
**vstd must erase cleanly under the userspace mini-workspace build**
(`kernel/build.rs`, build-std) — not merely under host `cargo test`. `kcore`
proves vstd cross-builds for `aarch64-unknown-none-softfloat`, but it is linked
by the *kernel* crate, not by the separate userspace workspaces. **This is the
chief new risk and the reason for a pilot first** (the master plan's §7.0
discipline, re-applied to the userspace target).

`cas` adds a wrinkle: it is `std`-on-host / `no_std`-userspace and
`Vec`/`BTreeMap`-heavy, so `cargo verus verify -p cas` must pin a feature
config. The superblock functions are feature-agnostic byte parsing, so verify
with `--no-default-features`.

**Partial adoption.** Verus only verifies code inside `verus!{}` blocks; the
rest of each crate (the reactor, the wire codec, the store engine) stays plain
Rust treated as `external`. A `verus!{}` fn that calls a plain helper needs that
helper to carry a spec or be assumed — normal partial-adoption friction,
expected on `Admission` and `decode_checked`.

---

## Sub-phasing

Each sub-phase is one PR that lands the Verus proof green in CI, deletes the
subsumed Kani harness in the *same* PR (the property is never unguarded between
tiers), and writes a `doc/results/57+_verus-findings.md`. Ordering: de-risk the
toolchain first, bank the easy wins, then the two hard trophies, then the
Kani-retirement decision last (it gates whether the whole tier dies).

### 7a — pilot: `ipc::header`

The cleanest end-to-end proof (fixed 8-byte header, pure byte arithmetic).
Obligations: `decode` total over all byte strings, accepts iff
`len == HEADER_SIZE`, `encode∘decode = id` in both directions — all ∀, replacing
`check_header_decode_total` / `check_header_roundtrip`. **The real job is the
toolchain proof: confirm vstd erases under the userspace cross-build** by
building all five user binaries via `kernel/build.rs` and confirming green,
before any hard proof rests on it. Wire `cargo verus verify -p ipc` into the
`verus` CI job; add the `[package.metadata.verus]` + `unexpected_cfgs` stanza to
`ipc/Cargo.toml`.

### 7b — `ipc::session`

`ConnectReq` / `GrantReply` totality + round-trip (the tag/length discipline as
`ensures`); and **`Admission` never over-grants for *all* admit/release
sequences** — `granted ≤ budget` as a loop/recursion invariant, so
`remaining()`'s `budget - granted` subtraction is proven non-underflowing — vs
Kani's 3-step bound. Delete the remainder of `ipc/proofs.rs`; drop `-p ipc` from
the `kani` CI job.

### 7c — `urt::slots`

The bitmap free-list, proven **∀ `cap` and `WORDS`** (vs Kani's CAP=4): every
`alloc` hands out a distinct in-window slot; exhaustion is exact; a freed slot
is reusable; `alloc_range` contiguity; the double-free precondition. Needs
`bit_vector`-mode reasoning to relate `free[i/64] & (1<<(i%64))` to a ghost
"free set", plus a light exec restructure of the `.find().map()` combinators
into explicit invariant-carrying loops (the shape kcore's loop ops already
took). Delete `check_slots_*`.

### 7d — `urt::time` (trophy #1)

`utc_ns_at` totality is easy (the u128/i128 decomposition, no overflow ∀). **The
prize is monotonicity** — `c1 ≤ c2 ⇒ utc_ns_at(c1) ≤ utc_ns_at(c2)` — which
proptest only samples and Kani *could not do* (nonlinear: relating two u128
divisions; the `doc/results/8` SOLVER note). This is the single hardest
host-chokepoint proof; budget for `nonlinear_arith` lemmas and a
division-monotonicity helper. Landing it converts the `conversion_is_monotone`
proptest from probabilistic to theorem. The seqlock *interleaving* stays Loom
(master plan §2) — unchanged. Retire `-p urt` from the `kani` job.

### 7e — `dma-pool` (trophy #2)

A `spec fn pool_wf` over the fixed `free: [(off,len); N]` sorted-disjoint extent
list, preserved by `alloc` (split / alignment round-up) and `free` (the
two-sided merge via `copy_within`). This is structurally the array-splice
reasoning kcore *already did* for `cdt_unlink` / `slot_move`. **Unlocks the
DN-10 case Kani OOM'd on**: two live buffers pairwise disjoint + the alignment
round-up honoured, ∀ sizes — not one concrete pair. Delete both harnesses;
retire `-p dma-pool` from the `kani` job.

### 7f — `cas::disk` superblock + the holdout decision

`validate_geometry` is clean (`checked_add` only, no `Vec`): totality + "every
committed region within `dev_len`" ∀. `decode_checked` reads a fixed prefix;
`blake3` gets the same ghost stub Kani used (`Hash::of` as an
`external_body`/assumed total function — totality needs no collision-freedom).

**The decision:** if both port without disproportionate `vstd::Vec` pain,
`cas::disk` is *not* a holdout, and 7g retires Kani wholesale. If the std/`Vec`
entanglement makes it cost-ineffective, this stays the single allowed Kani
holdout (master plan §4.7 scope flag, §5).

### 7g — `cas::tlv` decision + Kani-tier closeout

`cas::tlv` has **no Kani harness to delete** (already fuzz-primary, §4.7); a
Verus proof here is purely *additive* (`vstd::Vec` totality + canonical-form
`decode → re-encode = id`). Recommend **deferring it** unless cheap — cargo-fuzz
already owns that oracle over millions of cases.

Then, conditioned on 7f:
- **No holdout:** retire the `kani` CI job entirely; delete the pinned
  cargo-kani-0.67.0 install dance, the `#[cfg(kani)]` scaffolding in the four
  `lib.rs` files, and the CLAUDE.md / CI Kani sections (master plan §5, §8). The
  project keeps Verus + TLA+ + Loom/Shuttle + Miri/proptest + cargo-fuzz.
- **`cas::disk` holdout:** the `kani` job shrinks to just
  `cargo kani -p cas -Z stubbing`.

---

## CI / pinning deltas

- The `verus` job grows from `-p kcore` to `+ -p ipc -p urt -p dma-pool -p cas`,
  one `-p` per landing PR. No per-proof filter — a new obligation auto-gates, as
  today.
- The `kani` job shrinks per-target across 7b/7d/7e, then is removed or reduced
  to the `cas::disk` holdout at 7g.
- The `layering` grep is unaffected — the host crates aren't subject to the
  no-`asm`/no-`as *mut` rule (that is kcore-only).
- Verus stays pinned at `0.2026.06.07.cd03505` / `vstd =0.0.0-2026-05-31-0205`;
  no upgrade is part of this phase.

---

## Risks specific to phase 7

- **vstd in the userspace cross-build** (chief, new): mitigated by the 7a pilot
  gating on all five user binaries building before any deep proof rests on it.
- **Time monotonicity** (7d) is genuinely hard nonlinear arithmetic — the one
  place this phase could stall. Sequenced after the workflow is banked; its
  fallback is keeping the `conversion_is_monotone` proptest (no regression vs
  today).
- **`cas` std/`Vec` weight** (7f): mitigated by scoping Verus to the fixed-size
  superblock functions under `--no-default-features` and ghost-stubbing blake3,
  exactly as Kani did.

---

## Explicitly *not* in this phase

- **Phase 8** — `cas::store` recovery-core extraction + `AckedWritesRecoverable`
  on the pure decision function (master plan §4.8).
- **Phase 9** — closeout: spec `2_spec_rev2.md` §6, `CLAUDE.md`, and a
  `0_kani-rewrite.md` closeout banner (master plan §8, §11).
