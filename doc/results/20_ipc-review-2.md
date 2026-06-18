# IPC crate follow-up review (`doc/plans/2_ipc.md`, round 2)

A second independent audit of the IPC crate (§3), read on `main` at `20279b5`.
Where `doc/results/19_ipc-review.md` (round 1) audited the plan against the merged
tree and produced five ranked recommendations, this review asks the narrower
question the user posed: **were those recommendations addressed properly, were the
remaining omissions justified and documented, and did the follow-up work introduce
anything new worth flagging.** It is a critique, so it dwells on the residue, not
the (substantial) good.

## Method

- Mapped round-1's five recommendations to the merged history and confirmed one
  scoped PR per rec, landed in order:

  | Rec | PR | Commit | Subject |
  |-----|----|--------|---------|
  | 1 (scope status) | #45 | `5d98f9c` | doc/plans: scope the IPC plan status to what shipped |
  | 2 (re-point shell) | #45 | `3a850a6` | ipc/shell: re-point the shell's spawn/reap loop onto the reactor |
  | 3 (pin seed) | #46 | `b9cc1fa` | ipc: pin the Shuttle seed + wire the replay corpus |
  | 4 (Kani + caveat) | #47 | `d97720c` | ipc: Kani-verify the session codecs + `Admission`; record the multi-source caveat |
  | 5 (tidy surface) | #49 | `11747f0` | ipc: tidy the dead/stale surface |

  (recs 1 and 2 share a PR — both small, sensibly bundled. PR #48 between them is
  the unrelated Verus scratchpad.)
- Re-read the touched sources (`ipc/src/{reactor,model,session,proofs,lib,
  transport}.rs`, `user/shell/src/main.rs`, `tla/ipc_reactor/IpcReactor.tla`) and
  the CI wiring (`.github/workflows/ci.yml`).
- **Independently re-ran every tier on `20279b5`:** `cargo test -p ipc` (17) and
  `--features wire` (21); `RUSTFLAGS=--cfg shuttle … --lib` (17);
  `RUSTFLAGS=--cfg loom … --lib` (12); `cargo kani -p ipc` (7/7 harnesses verify,
  2/2 cover properties satisfied). All green.

## Verdict

**All five recommendations were addressed, and addressed well — the two
substantive ones (rec 2 re-point, rec 4 Kani) materially, not cosmetically.** The
follow-up work kept the per-PR discipline round 1 praised (one rec, one PR, tests
green) and is honest about what it chose *not* to do (the multi-source verification
was recorded as a caveat, the sanctioned either/or). The crate has moved from
round 1's "built and verified; not yet load-bearing" to **"built, verified, and
now bearing a real multi-source load in the shell"** — the single most convincing
gap round 1 named.

What remains is a short tail: (1) one **stale doc paragraph** the rec-1 status
rewrite missed; (2) gap #1 is *reduced, not closed* — production now multiplexes
**two** bound sources (a child's exit + fault) but never **many** at once, so the
`pending`-drain / `trailing_zeros` scan over >2 simultaneously-ready bits still
runs only in harness #5; (3) the **big deferrals** (the real connect mechanism,
the bulk-window mechanism) are untouched — correctly, since no rec asked to build
them, only to document them, which rec 1 did. None of this is a defect in the
follow-up; it is the accurately-named MVP boundary.

## Per-recommendation assessment

### Rec 1 — amend the plan's status (✅ done; one stale residue)

`doc/plans/2_ipc.md`'s header now reads "**substantially implemented; scoped**"
with an explicit **Deferred (not yet built)** block naming the connect mechanism
and the bulk window, plus a narrative of recs 2–5. This is exactly the honesty
round 1 asked for, and it is kept current through rec 5.

**The one miss:** the `**Status quo:**` paragraph (`2_ipc.md:39–42`) was *not*
updated and now contradicts the header above it — it still asserts "`send`/`recv`
in `lib.rs` are `todo!()`" and "the reactor, backpressure, valuable-cap ack, and
serialization **do not exist**." All of those now exist and are verified. This is
a pre-implementation baseline left in place; a reader scanning the file meets
"substantially implemented" at the top and "do not exist" thirty lines down. It
should be relabeled (e.g. "Starting point (pre-implementation)") or deleted. Minor,
doc-only, but it is the kind of contradiction the rec-1 rewrite existed to remove.
*(Fixed in the same commit as this review: the paragraph is now relabeled
"Starting point (pre-implementation baseline)" and put in past tense.)*

### Rec 2 — re-point the shell's spawn/reap loop onto `Reactor` (✅ done; the headline fix)

This was round 1's highest-value item and it is done properly
(`user/shell/src/main.rs:537–562`). The hand-rolled `sys::notif_wait(EVENT_NOTIF)`
bit-group scan is **gone** (grep confirms no residual manual scan in the shell):
the loop now arms the child's exit/fault bindings (`rec.arm`, a `thread_bind`
before `start`), registers them as two **externally-bound, edge-triggered** sources
via `Reactor::register_bound(EXIT_BIT, EXIT_KEY)` / `register_bound(FAULT_BIT,
FAULT_KEY)`, and blocks in `reactor.wait()` — naming no notification bit. The shell
is now a genuine `register_bound` production consumer, and `register_bound` —
designed for exactly this edge-triggered, no-poll-once case — is the right
primitive (a channel `register` would have fabricated a poll-once and reported a
child dead before it was). The dispatch path even has a focused std unit test
(`reactor_register_bound_dispatch`, `model.rs:719`) proving the no-poll-once
property by signaling only the high bit and asserting its key.

**The honest limit on the win:** the shell runs **one child at a time** (`run_once`
spawns, then `wait`s, then reaps), and builds a fresh 2-source reactor per spawn.
So production now multiplexes **two** bound sources behind one wait — strictly more
than storaged's one, and enough to make round 1's "the reactor's multiplexing… is
never exercised in a shipping binary" **no longer true** — but the *many*-source
behaviour (several bits ready at once, drained across `wait` calls through
`pending`, picked lowest-first by `trailing_zeros`) is still exercised only by
harness #5. Both keys also collapse to "go reap" (reap reads exit-vs-fault back
from the report), so the key *discrimination* isn't load-bearing in the shell. Net:
gap #1 is **materially reduced, not closed** — a fair outcome for the MVP, and one
the plan should arguably state as precisely as this paragraph does.

### Rec 3 — pin the Shuttle seed + wire the replay corpus (✅ done, clean)

`model.rs:228–257`: `check_pinned` runs a `RandomScheduler::new_from_seed(
SHUTTLE_SEED, SHUTTLE_ITERS)` (`0x1C_5EED`, 1000 iters) — the same machinery
`check_random` uses internally, with the entropy seed replaced by a fixed one — and
**all six** shuttle harnesses route through it (`rig_smoke`, `fifo_no_drop`,
`reactor_no_lost_wakeup`, `full_backpressure_no_drop`, `valuable_cap_ack_no_loss`,
`fairness_smoke`). The `shuttle_replay_corpus` test is wired with the
`&[(fn(), &str)]` plumbing type-checked and empty until the first bug — the
designated landing spot, matching the fuzz-corpus convention. A CI failure now
reproduces from source. Nits, both acceptable: the seed is a single fixed value
(bumping it to widen coverage is a manual, deliberate act, like the tool-version
pins — and is documented as such); the corpus is empty (correct — the convention is
"for any bug found," and none was).

### Rec 4 — Kani harnesses for the session codecs + `Admission`; multi-source caveat (✅ done; sound)

`ipc/src/proofs.rs` gained five harnesses, all carrying `kani::cover!`
anti-vacuity checkpoints (the CI vacuity guard counts them):
`check_connect_req_decode_total` / `_roundtrip`, `check_grant_reply_decode_total` /
`_roundtrip`, and `check_admission_never_over_grants`. The decode-total proofs
establish exactly the §5.4 property the plan demanded of "any new *pure* codec
helper" — totality over arbitrary bytes and acceptance **iff** well-formed. The
`Admission` proof is the one with real content and it is **sound**: each step reads
`before = remaining()` fresh and asserts the contract relative to it
(`Ok ⟹ size==req ∧ req≤before ∧ remaining()==before−req`; `Err ⟹ req>before ∧
remaining() unchanged`), so the `granted ≤ budget` invariant's preservation is
proven step-local, and Kani's overflow checking on `remaining()`'s `budget −
granted` turns any over-grant into a verification failure. The 3-step bound
(`unwind(4)`) is more than sufficient — the invariant is inductive and each step
proves the inductive case against a fully symbolic entering state.

For the multi-source verification, the team took the review's explicitly-offered
second option — **record the caveat** rather than extend TLA/Loom to ≥2 sources —
and documented it in three coherent places: the `IpcReactor.tla` header "Scope
limitation" block (`:17–25`), plan §5.4, and a pointer to round 1's gap #5. This is
a defensible call (extending the careful single-source spec, or risking a
multi-bit Loom state-space blow-up against the 15-min `concurrency` cap, is
disproportionate to a follow-up). The cost is named plainly: multi-bit dispatch and
the `trailing_zeros` lowest-bit fairness bias rest on harness #5 alone, with **no
starvation/bounded-wait property in any tier**. That remains true; it is now a
known, written limitation rather than a silent one.

### Rec 5 — tidy the dead/stale surface (✅ done; two cosmetic residues)

All three sub-items landed: `ConnectErr` is trimmed to its only constructed variant
`Refused` (`session.rs:42–49`, with a comment that the richer client-side errors
return with the deferred mechanism); the `lib.rs` "Async send/recv" responsibilities
line is corrected to "non-blocking + blocking/bounded-retry … no `async`/`.await`
form" (closing gap #6's stale-doc half); and `timer_arm` is removed from the
`Transport` trait and `ModelTransport` (which had a panicking `unimplemented!()`),
with the rationale — a timer reaches the reactor via `register_bound`, not a
`Transport` method — recorded at `transport.rs` and plan §3.1.

Two residues, both cosmetic:
- **`sys::timer_arm` is now genuinely caller-less** (grep: referenced only by doc
  comments). It was kept as "the raw kernel ABI," which is defensible — `sys.rs`
  mirrors the syscall surface whether or not the crate calls each one — but it is,
  strictly, the same species of unreachable surface rec 5 set out to prune. Leaving
  it is the right call; it is worth a one-word note in `sys.rs` that it is ABI-
  complete-on-purpose, so a future tidy pass doesn't re-flag it.
- **`ConnectErr` is now a single-variant enum** used as a `Result` error. Harmless,
  and it preserves intent + forward-compatibility for the deferred connect path; an
  `Option<WindowGrant>` would have been equivalent but rippled into the tests and
  the Kani harness, so keeping the named enum was the lower-risk choice.

## Scorecard

| Round-1 item | Round-1 status | Now | Note |
|--------------|----------------|-----|------|
| Rec 1 status scope | open | ✅ | done; stale `Status quo` para fixed in this commit |
| Rec 2 shell re-point (gap #1) | open | ✅ / ⚠️ | 2-source production multiplexing live; many-source still harness-only |
| Rec 3 pinned seed (gap #4) | open | ✅ | `check_pinned` over all 6; corpus plumbed |
| Rec 4 session/Admission Kani (gap #7) | open | ✅ | 5 harnesses, sound, cover-guarded |
| Rec 4 multi-source verify (gap #5) | open | ✅ (caveat) | recorded in 3 places; no fairness property |
| Rec 5 tidy (gaps #6, #8a) | open | ✅ | `ConnectErr`/lib doc/`timer_arm` done |
| Connect mechanism (gap #2) | deferred | ⏸ | untouched — by design; now documented |
| Bulk window (gap #3) | deferred | ⏸ | untouched — by design; now documented |
| Model nits `_dests`/shared `cap` (gap #8b/c) | noted | ⏸ | 2 of 3 remain by design (`model.rs:154`, one `cap`) |

## Are the remaining omissions justified and documented?

- **Yes, and documented:** the dynamic connect mechanism and the bulk-window
  mechanism (gaps #2/#3) are the real remaining engineering, both genuinely gated
  on kernel cap-transfer wiring, and both now sit in the plan's **Deferred** block
  and §6-phase-6 note. No recommendation asked to build them — recs were ranked
  *actionable follow-ups*; these were always "what remains," and round 1's job for
  them was to get them named honestly, which rec 1 did. The multi-source
  verification caveat (gap #5) is likewise a justified, triply-documented scope cut.
- **Yes, but with a residue:** the two model-fidelity nits (gap #8) — `recv_nb`
  ignoring `_dests` (`model.rs:154`, cap presence simulated by the mask) and a
  single shared queue `cap` depth across channels — are unchanged. They were never
  in a rec and are acceptable for the MVP model (the real cap move is the kernel's,
  per `CapRevocation`), but they remain a quiet shallowness in what harness #4
  "proves." Worth a sentence in the `ModelTransport` doc so the model's depth is
  not over-read.
- **Was not quite, now fixed:** the stale `Status quo` paragraph was an
  undocumented contradiction — the only place the follow-up's own honesty
  discipline had slipped — and is corrected in the same commit as this review.

## What still remains (unchanged from round 1, correctly)

- **The real connect (§3.5/§4.6):** a client `connect()` that funds and passes an
  endpoint cap, and `storaged` accepting a second concurrent session — the kernel
  cap-transfer "Phase 3."
- **The bulk-window mechanism:** a server-granted frame + `(window, offset, length)`
  descriptor / doorbell, so `WindowGrant` grants something a window reader consumes.
- **Many-source production exercise + a fairness property:** a shipping consumer
  that multiplexes >2 concurrently-ready sources, and/or a TLA/Loom model of the
  multi-bit scan with a starvation/bounded-wait property addressing the
  `trailing_zeros` bias. Until then, the strongest form of the reactor's raison
  d'être is harness-only.

## Bottom line

The five recommendations were taken seriously and executed with the same per-PR,
negative-control, verify-every-tier discipline that made round 1 praise the crate
in the first place. The two that mattered — making the shell a real reactor
consumer and proving the session codecs + `Admission` — are done substantively, and
I re-verified all four tiers green on `20279b5`. The IPC crate is no longer "not yet
load-bearing": it bears the shell's exit/fault multiplexing in production. The
residue is a tail of three small things — a stale plan paragraph (the one actual
slip, fixed in this commit), a model whose depth shouldn't be over-read, and the
still-deferred connect/bulk mechanisms; the latter two are the MVP boundary, now
honestly drawn. The right next step is smaller in scope than round 1's: decide
whether the next real consumer (a many-source multiplexer, or the dynamic connect)
is worth pulling forward from "deferred."
