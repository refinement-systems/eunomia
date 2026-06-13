# Kani verification findings — part 12 (DN-4 ghost-witness routing)

Continuation of `doc/results/2_kani-findings.md` (§4.1) … `13_kani-findings-11.md`.
This part implements **recommendation #3** of the second conformance review
(`14_kani-review-2.md`): make the DN-4 teardown *routing* a Kani assertion rather
than a source-inspection comment. **No defect** — a strengthening that closes the
last source-only seam the DN-4 decomposition left. The standing caveat and design
notes (DN-1…DN-13) of the earlier parts apply unchanged; this adds no new DN (it
sharpens DN-4, it is not a new CBMC limitation).

## The seam (review-2 critique 3)

DN-4 (`2_kani-findings.md`) closes "deleting a container cap tears the container
down" as two composing Kani proofs, because a top-level `delete` dispatched
through `obj_unref`'s cap-kind `match` unrolls every recursive-destructor arm
under CBMC and never finishes:

- the teardown **body**, by direct call — `check_destroy_cspace`,
  `check_destroy_channel` (§4.3), `check_thread_teardown` (§4.4);
- the `obj_unref` **dispatch**, with the recursive destructors
  (`destroy_cspace`/`destroy_channel`/`destroy_tcb`) stubbed to no-ops so the
  formula stays tractable — `check_delete_cspace`.

The seam the review flagged: the stub was a *silent* no-op, so
`check_delete_cspace` asserted only `(*cs1).hdr.refs == 0`. It never *witnessed*
that `obj_unref` actually took the `CapKind::CSpace(p) => destroy_cspace(p)` arm
— that one `match` line was trusted by source inspection. The contrast the review
drew: `check_delete_frame` already *asserts* its routing (`count(AspaceUnmap(..))
== 1`), because the frame path's effects (`aspace_unmap`/`aspace_destroy`) are
`Env` trait methods the recording `GhostEnv` logs. The container destructors had
no such observability.

## The fix: a proof-only `Env` witness the stub records

The three stubs are *generic* — `no_destroy_cspace<E: Env>(cs, env: &mut E)` —
and must stay generic to match the signatures of the functions Kani replaces
(`cspace::destroy_cspace<E: Env>`, `channel::destroy_channel<E: Env>`,
`thread::destroy_tcb<E: Env>`). From a generic `E: Env`, the *only* handle to
harness-owned state is the `Env` trait surface — which is exactly why
`aspace_destroy` is an `Env` method in the first place. So:

- **`kcore/src/env.rs`** gains three `#[cfg(kani)]` default-no-op `Env` methods —
  `ghost_destroy_cspace`/`_channel`/`_tcb`. They are proof-only: gated
  `#[cfg(kani)]`, so they never reach the production trait surface; the kernel's
  `KernelEnv` (never built under Kani) and the *real* destructors never call them.
- **`kcore/src/proofs/ghost.rs`** adds three `GhostEvent` variants
  (`DestroyCspace(*mut CSpaceObj)` / `DestroyChannel` / `DestroyTcb`) and
  overrides the three hooks on `GhostEnv` to `push` them into the existing event
  log (`MAX_EVENTS == 16`; each delete harness pushes exactly one).
- **`kcore/src/proofs/stubs.rs`** — the no-ops now call the hook
  (`env.ghost_destroy_cspace(cs)`), so the dispatch into a stubbed destructor is
  *observable* instead of silent. Behaviour is otherwise unchanged: the stub
  still neutralizes the recursion CBMC cannot prune cheaply.

This is the same instrument `check_delete_frame` uses (a recorded `Env` effect
asserted with `GhostEnv::count`), now extended to the three container kinds.

## Harness changes (`kcore/src/proofs/teardown.rs`)

- **`check_delete_cspace` strengthened.** Added
  `assert!(w.env.count(GhostEvent::DestroyCspace(cs1)) == 1)` after the existing
  `refs == 0` assert — the routing is now witnessed, not inferred.
- **`check_delete_channel` (new), the channel analog.** A `Channel` cap
  (`ChanEnd::A`) is the last ref in a cspace slot; `(*ch).hdr.refs = 1`. Because
  `delete` fires the channel's peer-closed event *before* `obj_unref` (the
  load-bearing ordering, TSpec `ChannelFireSafe` / DN-2), the harness sets
  `(*ch).end_caps[0] = 1` so `endpoint_cap_dropped` decrements end A 1→0 and
  fires peer-closed into end B's binding — *null* here, a no-op — rather than
  underflowing `end_caps: [u32; 2]`. Asserts slot emptied/detached,
  `(*ch).hdr.refs == 0`, and `count(DestroyChannel(ch)) == 1`.
- **`check_delete_tcb` (new), the TCB analog.** A `Thread` cap is the last ref;
  `(*t).hdr.refs = 1` (`World::new` zeroes TCB refs). A Thread cap takes no
  channel/frame side path in `delete`, so the ghost witness is the whole
  observable routing. Asserts slot emptied/detached, `(*t).hdr.refs == 0`, and
  `count(DestroyTcb(t)) == 1`.

Both new harnesses carry all three `#[kani::stub]` attributes and
`#[kani::unwind(6)]`, exactly as `check_delete_frame`/`check_delete_cspace` do:
per DN-4 the cap kind is reloaded from slot memory and is not constant-folded, so
even a *concrete* `Thread`/`Channel` cap makes `obj_unref` explore every arm. The
concrete cap kind keeps only the feasible arm's recorded event on the live path,
so `count(..) == 1` holds. The harnesses are fully concrete (no `kani::any`/
`assume`), so every assertion sits on the single reachable path — the counts are
genuinely checked, not vacuous.

## Results

All three verify under cargo-kani 0.67.0 (`-Z stubbing`, from the repo root):

| Harness | Result | Solver time (decision proc.) |
|---|---|---|
| `check_delete_cspace` (strengthened) | ✅ SUCCESSFUL | ~0.012 s |
| `check_delete_channel` (new) | ✅ SUCCESSFUL | ~0.013 s |
| `check_delete_tcb` (new) | ✅ SUCCESSFUL | ~0.012 s |

`check_delete_frame` and the host `cargo test -p kcore` (11 tests) re-verified —
no regression. The host build of `GhostEnv` is unaffected: the new hooks are
`#[cfg(kani)]`, so under `cargo test` the `Env` trait does not declare them and
`impl Env for GhostEnv` stays complete; the new `GhostEvent` variants compile
unconditionally (pub-enum variants do not warn when unconstructed).

## CI / budget

The `kani` CI job runs `cargo kani -Z stubbing -p kcore` with **no `--harness`
filter**, so `check_delete_channel` and `check_delete_tcb` auto-gate from now on
(the property `13_kani-findings-11.md` flagged as worth preserving). Both are
stubbed, concrete, and unwind-6 — solver time ~0.01 s each — so they are
negligible against the ~7 min of headroom under the 30-min suite budget; the
rec-#3 `cover!` post-check tally is unaffected (these harnesses add no `cover!`).

## Residual (unchanged)

This closes the **one-level dispatch** seam only. The honest DN-4 residual stands:
**deeply nested** container teardown — a container whose resident is itself a live
container (`delete → destroy_* → delete → destroy_*`) — remains TSpec + QEMU
covered (`spawn-test.sh` reclaim loop), not Kani-proven. What changed is that the
*routing* into the (one-level) stubbed destructor is now a Kani assertion on all
three container kinds, not a source comment.

## Status of recommendation #3

✅ Done. The DN-4 decomposition's last source-only seam is closed: the
`obj_unref` → container-destructor routing is witnessed by a ghost event and
asserted in `check_delete_cspace` plus the new `check_delete_channel` /
`check_delete_tcb` analogs. Remaining open review-2 items: #4 (correct the
`bounds.rs` "same state space as TLC" comment), #5 (the CI cover-message nit),
and #6 (the off-path `-Z function-contracts` spike on `revoke`/`obj_unref`).
