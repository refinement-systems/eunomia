# Kani verification findings — part 3 (§4.3 channel)

Continuation of `doc/results/2_kani-findings.md` (§4.1) and
`doc/results/3_kani-findings-2.md` (§4.2) for the channel suite (plan
`doc/plans/0_kani-rewrite.md` §4.3). Harnesses live in
`kcore/src/proofs/channel.rs` under `#[cfg(kani)]` and run with the rest of
the suite via `cargo kani -p kcore` (CI job `kani`, pinned cargo-kani
**0.67.0**). The standing caveat, the bounds policy, and the design notes
(DN-1…DN-4) of parts 1–2 apply unchanged; only what is *new* to §4.3 is
recorded here.

## Standing caveat (unchanged)

**Every result here is bounded.** The channel harnesses use the TLC-scale
ring depth (`CHAN_DEPTH = 2` = TLA `QueueDepth`) and a transition length of
`K = 4` for the FIFO harness — enough to exercise fill → drain → index
wrap-around at depth 2, which is the subtle part of the modular `head`/`count`
arithmetic. Scaling either up is a one-line change in
`kcore/src/proofs/bounds.rs` / the harness `K`.

## What §4.3 verifies

| Harness | Property | Plan row |
|---|---|---|
| `check_ring_fifo` | send/recv against an independent ghost FIFO: payloads delivered in send order, `Full`/`Empty` track the count exactly, indices in bounds, `chan_wf` after every step — for any op sequence at depth 2 | row 1 |
| `check_send_move` | a cap leaves the sender's slot exactly on success; `Full`/`PeerClosed` leave sender slots untouched | row 2 |
| `check_recv_atomic` | `NoCapSlot` leaves the message **fully queued** — no partial cap install, payload + count intact (recv validates all dests before moving any) | row 3 |
| `check_recv_null_tolerant` | a queue slot emptied by revocation in flight is delivered as an absent cap (mask bit clear), no panic (§3.4 null-slot rule) | row 4 |
| `check_peer_closed` | dropping an end's last cap fires the *other* end's peer-closed binding into a live notification; send into the closed peer errors (nondet over which end closes) | row 5 |
| `check_bind_refcounts` | bind/rebind/unbind keep the bound notifications' refcounts exact; rebind releases the old before taking the new | row 6 |
| `check_destroy_channel` | TSpec `ReclaimedReleased`: every queued cap deleted (object unref'd), every binding ref released | row 7 |
| `check_teardown_fire_safe` | TSpec `ChannelFireSafe`: the M1 EL0 step-6 scenario as a proof — both endpoints' peer-closed bound to a separately-funded notification, endpoint caps deleted in nondet order, each surviving peer's binding fires into a live notification, the notification outlives the channel | row 8 |

All eight verify. No defects found — every property held on the real code at
the stated bounds.

## Design / engineering notes new to §4.3

- **DN-5 — `chan_wf` is the ring invariant; the `end_caps` census lives in the
  cap-census harnesses, not in `chan_wf`.** Plan §4.3 lists "`end_caps`
  consistent with a ghost cap census" as part of `chan_wf`. Folding that into
  the shared `chan_wf` predicate would break the harnesses that legitimately
  set `end_caps` without a matching slot census — `check_ring_fifo` uses a
  bare `ChannelPool` with no cspace slots and sets `end_caps = [1, 1]` to model
  two open ends, so an `end_caps == #Channel-caps` check would (correctly, for
  that pool) fail. `chan_wf` therefore stays the pure ring invariant
  (`count ≤ depth`, `head < depth`, out-of-window slots empty, §3.4), and
  `end_caps` consistency is proven where a real cap census exists:
  `check_retype_channel` (§4.2: two endpoint caps ⇒ `end_caps == [1, 1]`,
  `refs == 2`), `check_peer_closed`, and `check_teardown_fire_safe` (the
  `end_caps` → 0 transition driving peer-closed).

- **DN-4 refinement — deleting a channel/cspace cap is tractable when its
  container is *empty*.** Part 1's DN-4 records that deleting a frame, channel,
  or cspace cap unrolls the recursive container teardown past the CI budget.
  §4.3 sharpens the boundary: the wall is specifically about *populated*
  containers, where `destroy_channel`/`destroy_cspace` loop over occupied slots
  and recurse into `delete`. With an **empty** ring,
  `check_teardown_fire_safe` deletes channel endpoint caps — triggering
  `obj_unref → destroy_channel` — and verifies in ~7 s, because the
  destroy loop bodies are all skipped (no nested deletes). `check_destroy_channel`
  goes one step further: it queues two **notification** caps, whose teardown
  (`destroy_notif`) is itself loop/recursion-free, and still verifies in ~5 s.
  So the TSpec teardown mirrors *are* full Kani proofs here — DN-4's wall only
  bites when the queued caps are themselves containers (channel/cspace) forcing
  a recursive cascade, which these harnesses deliberately avoid.

- **Harness cost: prefer the smallest object set.** Two harnesses first ran
  over the full `World` and blew the ≤5-min budget (plan §8): `check_ring_fifo`
  at **~11 min** (the K=4 branching over the whole `World` — two cspaces, two
  TCBs with 272-byte trap frames, notifications, timers, the event log) and
  `check_send_move` at **~9.5 min** (a single send, but its 256-byte payload
  copy *into* the World-embedded ring made CBMC's memory model blow up — the
  early-returning `recv` harnesses that never copy the payload stayed at a few
  seconds). Both touch only the channel (plus, for send, one sender slot), so
  scoping each to a standalone `ChannelPool` + `GhostEnv` (and a stack
  `CapSlot`/`NotifObj` for send) cut them to ~2.4 min and a few seconds with
  identical coverage. The lesson, recorded for future harnesses: scope the
  harness state to exactly the objects the op reaches — a payload write inside
  a large allocation is far costlier than the same write into a small one.

## Findings

None. The channel ops satisfied move semantics, receive atomicity,
null-tolerance, peer-closed firing, binding-refcount exactness, and the two
TSpec teardown properties at the TLC bounds.

| ID | Date | Harness | Bounds | Severity | Description | Status |
|----|------|---------|--------|----------|-------------|--------|
| —  | —    | —       | —      | —        | (no defects found) | — |

## Harness solver times (informational; CI budget ≤5 min/harness, §8)

Measured on the dev machine (cargo-kani 0.67.0).

| Harness | Bounds | Time |
|---------|--------|------|
| `check_ring_fifo` | `ChannelPool`, depth 2, K=4 | ~142 s |
| `check_send_move` | `ChannelPool` + standalone slot, nondet scenario | ~41 s |
| `check_recv_atomic` | `World`, one recv | ~3.2 s |
| `check_recv_null_tolerant` | `World`, one recv | ~6.8 s |
| `check_peer_closed` | `World`, nondet end | ~5.6 s |
| `check_bind_refcounts` | `World`, nondet refs | ~1.3 s |
| `check_destroy_channel` | `World`, 2 queued notif caps + 1 binding | ~5.5 s |
| `check_teardown_fire_safe` | `World`, 2 endpoint caps + 2 bindings, nondet order | ~6.9 s |

`check_ring_fifo` dominates: the K=4 transition over a depth-2 ring is the only
multi-step harness in §4.3 and accounts for most of the suite's time. Two
harnesses needed the cost fix below — `check_ring_fifo` (~11 min → ~2.4 min)
and `check_send_move` (~9.5 min → see table) — both because `send`/`recv`
write the 256-byte ring payload, and doing so inside a full `World` allocation
made CBMC's memory reasoning blow up; scoping to a `ChannelPool` fixed both.
