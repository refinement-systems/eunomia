# Verus findings 10 — Phase 3e: event/binding edges + `destroy_channel` deferral + phase-3 closeout

Plan: `doc/plans/3_verus-rewrite.md` (§4.3 channel) and its detailed decomposition
`doc/plans/3_verus-rewrite_phase3-detail.md` (§3e). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26` (§3a — untyped `retype_check`/`reset`), `27`
(§3b — the channel ghost-view enabling refactor), `28` (§3c — untyped
`retype_install`), `29` (§3d — channel `send`/`recv`, the FIFO core). This is the
**fifth and last** of phase 3's sub-phases: the notification-coupled channel edges
the FIFO core deferred, plus the declared cross-object-teardown scope-out and the
phase-3 documentation closeout.

**Doc numbering.** File `N` ↔ "Verus findings `N-20`": 26=6, 27=7, 28=8, (29
mislabelled "8" — it is the ninth), this is the tenth. The detail plan (§2-3e)
said "write `doc/results/26`," but 3a–3d already consumed 26–29; **30** is the next
free number — the one place the pre-written detail plan went stale.

**Outcome.** `cargo verus verify -p kcore`: **90 verified, 0 errors** (was 88 —
`+endpoint_cap_dropped`, `+bind`; the spec fn `bind_refs_post` and the
`external_body` `destroy_channel` add no verification unit). `cargo test -p kcore`:
**26 passed** (was 23 — `+endpoint_cap_dropped_decrement_and_fire`,
`+bind_install_rebind_unbind`, `+destroy_channel_deletes_caps_and_releases_bindings`).
The aarch64 `kernel` cross-build is unchanged (ghost erases; none of the three
functions is called from verified `kcore` code, so their new `requires` constrain
nothing at runtime). **One new `external_body` boundary** — `destroy_channel`,
host-test-checked — joining `notification::signal` (3b) and the inherited
`cspace::delete` as phase 3's residue. No new lemmas; both verified edges are
straight-line over the 3b `chan_*`/`obj_refs` seam.

---

## 1. What closed

- **`channel::endpoint_cap_dropped`** — decrements `end_caps[end]`, then fires the
  *other* end's peer-closed event through the verified `fire` (3b) **only** when
  the count reaches zero. `ensures`: `slot_view` unchanged on every path;
  `chan_view` updated at exactly `end_caps[end]` (`fire` frames `chan_view`); and a
  **conditional `refs_view` frame** — unchanged unless the drop hit zero (see §2.1).
  The `requires end_caps[end] > 0` discharges the `- 1`.
- **`channel::bind`** — the §3.6 binding-refcount discipline: release the old
  notification's ref, acquire the new one's, install `Binding { notif, bits }`.
  `ensures`: `slot_view` unchanged; `chan_view` updated at the one binding; and the
  `refs_view` delta `bind_refs_post` (§2.2). The `requires` refcount bounds (`old
  notif > 0`, `new notif < u32::MAX`) discharge the `- 1`/`+ 1`. This is the **first
  installment** toward `refcount_sound`'s binding term — the full census lands
  phases 4–5.
- **`channel::destroy_channel`** — kept `external_body`, the **declared scope-out**
  (detail §1.3): its body recurses through the still-`external_body` `cspace::delete`
  (cross-object teardown) and releases binding refs whose soundness needs the full
  `refcount_sound`, neither available until phases 4–5. Carries an **assumed**
  contract — `cspace_wf` preserved, the arena unchanged in extent, **every ring-cap
  slot emptied** — checked against the real body in `test_store.rs`
  (`check_destroy_channel`), exactly the discipline that lets `revoke` be verified
  against `delete`.

All three are **host-test-checked** against the real bodies (`test_store.rs`): the
non-firing/firing decrement (the firing path delivers the bits into a bound notif
while the slot/chan frame holds); the four `bind` refcount cases — install onto
unbound, rebind to a different notif, rebind to the *same* notif (net-zero), unbind;
and a teardown that deletes a queued cap in each ring and releases two distinct
binding refs.

---

## 2. Verus mechanics worth keeping

### 2.1 A frame can be **conditional** — `endpoint_cap_dropped`'s `refs_view`

`endpoint_cap_dropped` keeps `slot_view` and `chan_view` fixed unconditionally, but
its `refs_view` frame holds **on one branch only**:

> `old end_caps[end] != 1 ==> final.refs_view() == old.refs_view()`

The mutation that touches `refs_view` is not in this function — it is the
zero-triggered `fire` → `signal`, which is *permitted* to perturb `refs_view` (a
woken waiter's queued ref). So the honest postcondition asserts the frame exactly
where the body provides it (`set_chan_end_caps` frames `refs_view`; the only escape
is the conditional fire). Z3 closes it from the linear fact `old > 0 && old != 1 ==>
old - 1 != 0` (the `if … == 0` guard is false ⇒ no fire ⇒ `set_chan_end_caps`'s
`refs_view` frame is the whole story). The lesson generalises the doc-29 §2.1
"list every view a caller reads": a view's frame need not be all-or-nothing —
**state it on the branch where it actually holds**, guarded by the condition.

### 2.2 The `bind` refs delta as one spec fn — read-after-write, so rebind is free

The body decrements the old notif's ref *then* increments the new one's. Modelling
that order in the closed form makes the awkward `old == new` case (rebinding a
binding to the very notif it already names) provably net-zero **for free**:

```
spec fn bind_refs_post(r0, old_notif, new_notif) -> Map<ObjId,nat> {
    let r1 = match old_notif { Some(no) => r0.insert(no, (r0[no]-1) as nat), None => r0 };
    match new_notif { Some(nn) => r1.insert(nn, (r1[nn]+1) as nat), None => r1 }
}
```

The second `insert` reads `r1[nn]` — the *already-decremented* count when `nn ==
no` — exactly what the body's second `obj_refs(n)` reads. No case split in the
contract, no lemma: Verus symbolically executes the body (`set_obj_refs` ×{0,1,2}
then a `chan_view`-only `set_chan_binding`) straight onto `bind_refs_post`. The
`< u32::MAX` precondition on the new notif suffices even for `nn == no` (after the
`-1` the value is strictly smaller, so the `+1` cannot wrap). This is the template
for the per-op refcount deltas the rest of `refcount_sound` will accrue.

### 2.3 `external_body` ⇒ assume only the **robustly-true, checkable** core

`destroy_channel`'s real effect on `refs_view` is entangled: the per-ring-cap
`delete`s drop the refcounts of whatever objects those caps designate, *and* the
binding loop drops each bound notif's ref — and a ring cap could itself be another
channel's endpoint, so the deletes reach into *other* channels' `chan_view`. A
clean closed-form `refs_view`/`chan_view` postcondition would be false in general.
So the assumed contract states only what survives that entanglement and is
checkable against the body: `cspace_wf` preserved, `slot_view` domain fixed, every
ring-cap slot empty. The per-binding ref release — true but without a clean closed
form here — is left to the host test, which asserts it directly on the concrete
store. "Do not over-specify an assumed contract" (detail §2-3e): a *false* strong
clause is worse than an *honest* narrow one, and the host test catches the rest.

### 2.4 Folding the plain-Rust edges into the `verus!{}` blocks

`endpoint_cap_dropped` and `bind` sat between existing `verus!{}` blocks; folding
them in (deleting the intervening `} // verus!` / `verus! {` delimiters) merged
them with `endpoint_cap_added`/`fire`/`send`. The local `let old = …` in `bind`
shadowed Verus's `old(store)` spec keyword — renamed `old_b`. `destroy_channel`
(file tail, outside any block) got its own `external_body` block. No production-code
change beyond contracts: the `KernelStore` is the trusted boundary, so the ghost
`chan_*`/`obj_refs` views need only the `ExStore` spec + `ArrayStore` host bodies
(both already present from 3b).

---

## 3. Phase-3 closeout

3e is also the phase-3 documentation closeout (3a–3d deliberately seeded only their
findings docs — doc 29 §3). `CLAUDE.md`'s `### Verus` section and the §6
verification-tier table now record the full phase-3 result: untyped
`retype_check`/`retype_install`/`reset` and channel `send`/`recv`/
`endpoint_cap_added`/`endpoint_cap_dropped`/`bind`/`fire` on the **proven** list
(with the §2.5 sub-untyped-never-PHYS rights theorem among them), and
`notification::signal` + `channel::destroy_channel` as the new host-test-checked
`external_body` residue alongside `delete`. No spec-doc edit (that is the phase-8
closeout). The cross-object `delete` body and the full `refcount_sound` pass
forward to phases 4–5 unchanged.

### Phase-3 exit criterion — met

`cargo verus verify -p kcore` proves untyped `retype_check`/`retype_install`/`reset`
and channel `send`/`recv`/`endpoint_cap_added`/`endpoint_cap_dropped`/`bind`/`fire`
against `cspace_wf` + `chan_wf` + the FIFO `Seq` model, with the §2.5 sub-untyped-
never-PHYS rights theorem among them; `notification::signal` and
`channel::destroy_channel` are the only new `external_body` ops, both host-test-
checked; the aarch64 `kernel` build and `cargo test -p kcore` (26 tests) are green.

## 4. Scope held (what 3e did *not* touch)

- **`destroy_channel` body proof** — closes with the cross-object teardown
  (`delete`/`obj_unref`/`destroy_*` recursion + the seL4-zombie measure), phases 4–5.
- **`notification::signal` body proof** — phase 4 (3e kept the 3b assumed frame).
- **Full `refcount_sound`** — `bind`'s delta is the first binding installment; the
  census over cspace + queue + TCB-bind slots + bindings + waiters + timers + frame
  mappings is phases 4–5.
- `obj_unref`/`destroy_cspace`/`unref_*` stay plain Rust.
