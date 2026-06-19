# 71 — Follow-on-fix ledger: D-B1 Option 2 (priority into the Store seam · reducing `derive` ceiling)

> A *change* ledger for the doc-70 **D-B1 "Recommended follow-on — Option 2"**. One entry,
> three landed parts: (1) priority surfaced in the verified Store view, (2) a verified
> `thread::set_priority` that the spawn path routes its priority write through, and (3) the
> optional reducing `prio_ceiling` parameter on `derive`, wired end-to-end through the
> `CapCopy` ABI. Authority is the spec (`doc/spec/2_spec_rev2.md` §2.3 line 71, §5.4 line 360)
> and the verified source; the disposition followed is doc 70's Option 2 (lines 262–283).

## Provenance

Doc 70 closed **D-B1** at the cap-model level (the §5.4 ceiling became a `u8` on
`CapKind::Thread`, monotone through `derive`) but recorded two residues as Option 2:

- **F-70-6** — priority lived in the `kcore` `Tcb` struct *outside* `tcb_view()`, so spawn's
  `(*tp).priority = prio` was an unverified raw-pointer hop; the §5.4 `tcb.priority ≤ ceiling`
  was not machine-checked end-to-end.
- **F-70-9** — `derive` only *preserved* the ceiling, so the §2.3 supervision grant
  ("attenuated as desired") was unrealized.

**Headline:** priority is now a field of the verified `TcbView`; `thread::set_priority(store,
t, prio, ceiling)` carries `requires prio ≤ ceiling` / `ensures tcb_view()[t].priority == prio`
and both spawn syscalls route their write through it; and `derive` gained a `prio_ceiling`
parameter proving `child.max_prio == min(parent.max_prio, prio_ceiling)` (∀), exposed to
userspace as `cap_copy_prio`. Verified: **318 verified, 0 errors** (`cargo verus verify -p
kcore`); **88** `kcore` host tests (+2 witnesses); the AArch64 shell builds; QEMU boots with
init's two children spawning under the new verified write. **Runtime behaviour is preserved**
on the real boot path — a plain `cap_copy` passes the no-reduction sentinel, and the spawn
gate is unchanged.

## The change

| Layer | Edit |
|---|---|
| View (`kcore/src/cspace.rs`) | `TcbView` gains `priority: u8`. New `ExStore` contracts `tcb_priority` / `set_tcb_priority` (mirror `tcb_report`, frame the other six views). |
| `Store` trait (`kcore/src/store.rs`) | exec sigs `tcb_priority` / `set_tcb_priority`. |
| Verified op (`kcore/src/thread.rs`) | `set_priority<S: Store>(store, t, prio, ceiling)` — `requires prio ≤ ceiling`, `ensures priority == prio (≤ ceiling)` + six-view frame. |
| `derive` (`kcore/src/cspace.rs`) | `derived_kind` gains a `prio_ceiling` arg with a reducing `Thread` arm `min(parent, prio_ceiling)`; `derive` gains the param; ceiling `ensures` strengthened to `== min(p_mp, prio_ceiling) ∧ ≤ p_mp ∧ ≤ prio_ceiling`. |
| ABI (`kcore/src/sysabi.rs`) | `Sys::CapCopy` gains `prio_ceiling` (reads free reg `a[3]`); new `NO_PRIO_CEILING = 0xFF` sentinel. |
| Shell (`kernel/`) | both spawn handlers call `thread::set_priority(tp, prio, max_prio)` (verified wrapper) instead of `(*tp).priority = prio`; `CapCopy` handler passes `prio_ceiling`; `destroy_tcb` gets `#[verifier::rlimit(30)]` headroom. |
| Userspace (`ipc/`, `kernel/src/user.rs`) | both `cap_copy` wrappers send the `0xFF` sentinel; new `cap_copy_prio(src, dst, rights, prio_ceiling)`; the m1-test thread-cap copy now reduces the ceiling. |
| Tests (`kcore/src/test_store.rs`) | `TcbState.priority`; exec getter/setter; `derive` call sites take the 4th arg; `+derive_attenuates_thread_priority_ceiling`, `+set_priority_writes_within_ceiling`. |

## Findings

### F-71-1 — F-70-6 closed: the priority write is machine-checked end-to-end

`tcb_view()` now carries `priority`, and the spawn-time write goes through the verified
`set_priority`, whose `ensures tcb_view()[t].priority == prio` combined with `requires prio ≤
ceiling` (discharged by the runtime `prio > max_prio` gate the spawn handlers already had)
makes `tcb.priority ≤ ceiling` a reachable postcondition rather than a shell promise. The
single unverified hop doc 70 named is gone; what remains is the trusted `Store`-trait
realization of `set_tcb_priority` (one raw write behind the handle seam) — the **same**
trusted-base posture as `set_tcb_report` / `set_tcb_bind_bits`, a *satisfiable* seam, not a
vacuous one (contrast D-A1's `!is_homed`).

### F-71-2 — F-70-9 closed: `derive` strictly attenuates, and userspace can ask for it

`derived_kind`'s new `Thread` arm reduces the ceiling to `min(parent, prio_ceiling)`, proven
`== min ∧ ≤ parent ∧ ≤ prio_ceiling` for all derivations. The ripple was tiny exactly as doc 70
predicted: `derived_kind` is referenced only inside `derive` itself (its ensures, body assert,
and one proof assert) — the dead-object lemmas reason via `is_thread_cap_for`, untouched. The
`CapCopy` ABI carries the ceiling on the previously-unused `a[3]`; `cap_copy_prio` exposes it.
The supervision grant of §2.3 ("hand a supervisor an attenuated thread cap") is now literal.

### F-71-3 — Behaviour preservation rides a sentinel, not a code path (novel)

The plain `cap_copy` wrapper passes `prio_ceiling = NO_PRIO_CEILING (0xFF)`; since priorities
are `< NUM_PRIOS = 32`, `min(parent, 0xFF) = parent` — exact preservation, so every existing
`cap_copy` caller (frame/notif copies, and the m1-test thread-cap copy) is behaviour-identical.
The *only* place a copied thread cap's ceiling could change is a deliberate `cap_copy_prio`.
This is why the change is runtime-invisible on the real boot path while still opening the new
authority — the sentinel does the compatibility work that an ABI bump would otherwise break.
(Both `cap_copy` wrappers previously sent `a[3] = 0`; left as-is, the new decode would have
read that as ceiling 0 and collapsed every copied thread cap — the sentinel is load-bearing.)

### F-71-4 — Surfacing a field re-tightened an unrelated rlimit (mirrors D-B1's own note)

Adding `priority` to `TcbView` enlarges every `tcb_view()` term in the SMT batch, which pushed
the already-borderline `destroy_tcb` (a `spinoff_prover` body doc 70's commit `5c68082` had
*just* tuned for the `max_prio` field) past the default 10 s budget on this platform. The fix
is the next standard Verus headroom lever after isolation: a private `#[verifier::rlimit(30)]`
on that one isolated body — the proof is unchanged, only its resource cap moved. This is the
recurring "new clauses destabilize an unrelated proof's rlimit" effect (doc 51 §3); worth
recording that *view-shape* growth, not just new axioms, triggers it.

### F-71-5 — Witness correspondence

`set_priority_writes_within_ceiling` witnesses the F-71-1 postcondition on `ArrayStore`
(priority written exactly, within ceiling, boundary `prio == ceiling` admissible).
`derive_attenuates_thread_priority_ceiling` witnesses F-71-2's reducing arm (parent 19,
`prio_ceiling 5` → child 5; and a `prio_ceiling` above the parent cannot *raise* it).
`derive_preserves_thread_priority_ceiling` (doc 70's witness) now passes the `0xFF` sentinel and
remains the preservation branch. The m1-test selftest routes its real thread-cap copy through
`cap_copy_prio(.., 3)` on hardware.

## Verification evidence

| Gate | Command | Result |
|---|---|---|
| Proof (primary) | `cargo verus verify -p kcore` | **318 verified, 0 errors** |
| Witnesses | `cargo test -p kcore` | **88 passed, 0 failed** (incl. the two new) |
| Host build | `cargo build -p kcore -p ipc` | clean, no warnings |
| Shell build | `cd kernel && cargo build` | clean (AArch64 bare-metal) |
| Boot smoke | QEMU (`virt`, gic v3) | `[init] system up`; storaged + shell spawn; `eunomia>` prompt; no panic, no spurious `ERR_PERM` (the `storaged` virtio-blk FATAL is expected with no `-drive` attached) |

## Disposition feed-forward

- **D-B1 Option 2:** closed (this change). The doc-70 follow-on items 1–3 are all landed; the
  F-70-6 seam and F-70-9 reduction gap are closed.
- The §2.3 line 71 / §5.4 line 360 spec notes were updated in place: priority is in the verified
  Store view, the write is a verified `set_priority`, and `derive` strictly attenuates via
  `prio_ceiling`. The only residual seam noted is the trusted `set_tcb_priority` realization.
- **ABI note:** `CapCopy` now consumes `a[3]`; a future syscall touching that register must
  account for the priority ceiling. `cap_copy_prio` is the userspace entry point for the grant.
