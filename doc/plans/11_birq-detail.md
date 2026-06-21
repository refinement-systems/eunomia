# Plan — Part B-IRQ detail: the kernel IRQ-handler object (a verified `CapKind::Irq` + device-IRQ→notification delivery + `IrqBind`/`IrqAck` syscalls + GIC SPI routing)

Detailed, separately-implementable decomposition of **Phase B-IRQ** from
`doc/plans/0_address_audit_rev0.md`. B-IRQ is the Wave-3 kernel item that builds the **one rev0§1
kernel object that is mandated but entirely absent**: the IRQ-handler cap. The spec lists it among
the kernel object set ("**IRQ handlers** — caps granting the right to receive and acknowledge an
interrupt," `spec_rev1.md:26`), has init hold the device IRQ caps ("all device resources (MMIO
frames, IRQ caps)," `:32`), and routes its delivery through the notification mechanism ("IRQ handlers
bind identically (seL4 precedent)," rev1§3.6 `:188`) — but no `CapKind::Irq` exists, `gic.rs`
explicitly defers it ("Userspace IRQ-handler caps (rev1§1) are introduced by the userspace drivers,"
`kernel/src/gic.rs:3-4`), and every device IRQ that is not the vtimer is EOI'd and **dropped**
(`handle_el0_irq`'s else branch, `kernel/src/exceptions.rs:220-225`). B-IRQ makes the object real and
verified, wires the general device-IRQ→notification path the timer already proves end-to-end for its
own PPI, and adds the two syscalls (`IrqBind`/`IrqAck`) a driver needs.

B-IRQ is the **console track's long pole**: the audit folds the IRQ object into M-9, but the IRQ
investigation confirmed it is M-9's *prerequisite*, not part of it — a console driver needs RX
interrupts (you cannot usefully poll a console for input the way the block driver polls a completion),
and today device IRQs reach no one. C-M9 is the driver-and-shell rewiring **on top of** the kernel
object B-IRQ delivers. B-IRQ is also the enabler for retiring driver polling generally (e.g. the
virtio-blk used-ring spin, B2/I-4) — a bonus, not a dependency.

**Honesty note up front (read first): B-IRQ is NOT behaviour-identical, and it widens the verified
ABI and the verified cap set.** Unlike B8 (verification-only) and like B9/B10, B-IRQ adds syscalls and
a new kernel object: two new opcodes (`IrqBind` = 25, `IrqAck` = 26) that touch the **verified syscall
decoder** (`kcore::sysabi::decode`, `:111`; the `nr >= 25 ==> UnknownCall` bound `:114` moves to
`nr >= 27`), a new `CapKind::Irq` variant that **re-opens every cap-set proof** (`cap_obj`,
`derived_kind`, `caps_consistent`, the teardown SCC `obj_unref`, and — the load-bearing one — the
central `obj_census`/`refcount_sound`), and a new device-interrupt code path in the trusted exception
shell. Existing opcodes 0..=24 and every existing handler are byte-for-byte unchanged. The regression
gate is therefore **the QEMU boot still green plus a bound device IRQ demonstrably waking an EL0
thread**, not full ABI/behaviour immutability.

**Closes (from the parent plan):**
- **The rev0§1 "IRQ handlers" kernel object — mandated but entirely absent.** The audit folds this
  into M-9 [high]; it is in fact M-9's long pole and a prerequisite for it. B-IRQ builds the object,
  its delivery path, and its syscalls. Also enables interrupt-driven drivers generally (retiring the
  virtio-blk poll, cf. B2/I-4 — bonus, not a dependency).

**Conforms rev1§1, rev1§3.6, rev1§2.7 (the IRQ object is blessed target; B-IRQ builds it).** B-IRQ is
a *conformance* phase, like B10: rev1 already blesses the IRQ-handler object and its notification
delivery (Part A is blessed first), so B-IRQ brings the code into conformance and makes **no normative
spec edit** (honesty note 4). It does not soften the spec.

**Spec target (blessed in rev1 — B-IRQ conforms code to it; no normative spec edit, honesty note 4):**
- **rev1§1 "Architecture"** (`spec_rev1.md:26`) — the kernel object set includes "**IRQ handlers** —
  caps granting the right to receive and acknowledge an interrupt." The exact object B-IRQ builds;
  "receive" = bind a notification (rev1§3.6), "acknowledge" = the `IrqAck` syscall. The same section
  (`:32`) gives the provenance: "At boot the kernel constructs … **init**, whose cspace holds … all
  device resources (MMIO frames, **IRQ caps**)." B-IRQ grants init the PL011 IRQ cap, the device-IRQ
  analog of the device-MMIO frame caps init already holds (`kernel/src/main.rs:100-126`).
- **rev1§3.6 "Event multiplexing: notifications"** (`spec_rev1.md:188`) — "**IRQ handlers bind
  identically** (seL4 precedent); timer objects bind identically (providing wait timeouts and the
  storage flush timer); … One object type, three pointer-sized slots per endpoint, and no allocation
  on any event path." The binding model B-IRQ implements: an IRQ cap binds a (notification cap, bit)
  pair exactly as the timer does (`TimerArm`, rev1§3.6), and the hardware interrupt signals that
  notification — no allocation on the event path (the binding is preallocated in the IRQ object). The
  lost-wakeup discipline ("bind, poll once, then wait") lives in the IPC crate, unchanged.
- **rev1§2.7 "The syscall boundary"** (`spec_rev1.md:125-135`) — "every `nr` outside the defined range
  is `UnknownCall`," the untrusted-decode discipline (unknown → error, never crash). B-IRQ's two new
  opcodes extend the defined range under this discipline; the decode stays total and its `ensures` is
  re-established with the new arms (honesty note 1) — the B10B precedent.
- **rev1§2.2 "Revocation"** (`spec_rev1.md:48`) — "Revoking a cap eagerly deletes all of its
  descendants." The new `CapKind::Irq` is a derivable, revocable cap like every other: init holds it,
  delegates an attenuated copy to the console driver, and revoking the grant tears the binding down
  (releasing the bound notification's ref). B-IRQ must re-establish the revoke/teardown proofs over
  the widened cap set (the teardown SCC `obj_unref`, honesty note 3).
- **rev1§7 "Toolchain"** (`spec_rev1.md:431,433`) — the console rationale: "The user-facing console is
  a userspace UART driver holding the PL011 IRQ and MMIO caps," and the debug-UART scaffold is
  time-boxed until "**the device-interrupt-to-notification path a receive side needs (§3.6)**" is
  built. B-IRQ builds exactly that path; C-M9 then retires the scaffold on top of it.

Because Part A is blessed first, **B-IRQ makes no normative spec edits** — rev1§1/§3.6/§2.7 are the
fixed targets. The only doc touches are the A4-style ledger updates: the verified-surface scope
paragraph gains the IRQ object + its census term, the `[verifying]`/`external_body` tally is unchanged
(B-IRQ adds verified ops, not trusted seams), and the kcore baseline rises (honesty note 3).

**Verification finding that motivates this phase** (from the IRQ-path investigation, re-confirmed
against the current tree):
- *The delivery primitive exists and is verified.* The ARM vtimer interrupt (PPI 27,
  `gic::INTID_VTIMER`, `gic.rs:17`) is taken in `handle_el0_irq` (`exceptions.rs:209`) →
  `timer::check_expired` (`exceptions.rs:214`) → `notification::signal` on the timer's bound
  notification (`kcore/src/timer.rs:847`); the arm/disarm + the binding's refcount census are verified
  in `kcore::timer` (`arm` `:313`, `disarm` `:72`, `check_expired` `:723`, the census term
  `armed_timer_refs` `cspace.rs:4118`). "Hardware interrupt → userspace notification" already works
  end-to-end and is verified — it is just hardwired to the timer. This de-risks the design: B-IRQ
  re-uses `notification::signal` for delivery and the timer's census machinery for the binding.
- *Device MMIO is already a frame cap, granted to init at boot.* init holds device-MMIO frame caps —
  the virtio-mmio window at `0x0a00_0000` (`main.rs:100-112`) and the PL031 RTC region at
  `0x0901_0000` (`main.rs:114-126`) — written directly into its cspace slots, **boot-static, not
  retyped**. Granting the PL011 region (`0x0900_0000`) is a small addition reusing this exact
  mechanism; the IRQ-handler cap follows the same boot-static-grant precedent (Design decision 3).
- *But the general device-IRQ path is unbuilt.* Concretely missing: **(1)** no `CapKind::Irq` — the
  enum stops at `Timer(o)` (`cspace.rs:97-118`), so the rev0§1 IRQ object does not exist; **(2)**
  `gic::init` enables only the redistributor PPI for the vtimer (`GICR_ISENABLER0`, `gic.rs:39`) —
  there is no distributor `GICD_ISENABLER`/`GICD_IROUTER` SPI enable+route (PL011 RX is SPI 1 → INTID
  33 on QEMU virt); **(3)** `handle_el0_irq`'s non-timer branch (`exceptions.rs:220-225`) EOIs and
  **drops** the interrupt — it neither signals a bound notification nor masks the line; **(4)** no
  `IrqBind`/`IrqAck` syscalls — the `Sys` enum (`sysabi.rs:43-69`) stops at `AspaceTopUp` (opcode 24)
  and has `TimerArm`/`ChanBind`/`ThreadBind` but no IRQ op; **(5)** corollary: every current driver
  **polls** (virtio-blk's used-ring spin, I-4), so there is no device-interrupt code path to copy
  beyond the timer's.

**Primary files:**
- `kcore/src/cspace.rs` — the **verified cap-set + census core** (the bulk of the work):
  - `enum CapKind` `:97-118` (add `Irq(crate::id::ObjId)` after `Timer(o)` `:117`); `Cap` `:121`,
    `CapSlot` `:143` (the `revoking` marker `:158` is unaffected — it keys off `.cap`/links, not the
    new kind).
  - `cap_obj` `:1344-1353` (add `CapKind::Irq(o) => Some(o)` — a designating cap); `derived_kind`
    `:1427-1432` (the `_ => k` arm `:1430` already makes `Irq` a faithful designating copy — **no new
    arm needed**, like `Notification`/`Timer`); `cap_max_prio` `:1376` (unaffected — Irq carries no
    ceiling).
  - `obj_census` `:4191-4198` — **the central perturbation**: add a seventh summand
    `irq_binding_refs(store.irq_view(), o)`, the mirror of `armed_timer_refs(store.timer_view(), o)`
    `:4196`. Every lemma that enumerates the census terms (the `census_delta_frozen` / `refcount_sound`
    frame family) gains one "`irq_binding_refs` framed because `irq_view` untouched" clause.
  - `armed_timer_refs` `:4118-4121` + its lemmas `lemma_armed_timer_refs_pos` `:4124`,
    `lemma_armed_timer_disarm`, `lemma_armed_timer_retarget` — the **term-for-term template** for the
    new `irq_binding_refs` + `lemma_irq_binding_refs_pos` / `lemma_irq_binding_unbind`.
  - `refcount_sound` `:4202`, `census_dom_complete` `:4221` — automatically range over the new term
    once `obj_census` includes it; re-established by the IRQ ops' census lemmas.
  - `cap_consistent` `:5291-5330` (add an `Irq(o)` arm: `irq_view` dom + finite + `irq_wf`, mirroring
    the `Timer(o)` arm `:5326-5330`); `caps_consistent` `:5340`.
  - `derive` `:8914` — re-verifies over the widened cap set with **no body change** (the B9 guard
    `:8975` and the `derived_kind`-driven copy already handle any designating kind); cite as
    "unchanged, re-verified."
  - `obj_unref` `:10078` — the teardown dispatch: add a `CapKind::Irq(o)` arm calling the new verified
    `destroy_irq` (the `CapKind::Timer(o)` arm `:10118` → `destroy_timer` is the template); extend the
    per-kind `requires` block (`:10118-10130`) with the Irq precondition (bound ⇒ notif live).
- `kcore/src/irq.rs` — **new module** (the `kcore/src/timer.rs` mirror, *minus the armed list*):
  `IrqObj` (the `TimerObj` analog `timer.rs:26-34`, carrying `intid: u32`, `notif: Option<ObjId>`,
  `bits: u64`, `bound: bool`, `masked: bool` — **no `next` field**, no armed-list membership); the
  verified `irq_bind` (the `arm` analog `timer.rs:313` minus the head-push: `+1` notif ref, set
  fields), `irq_unbind`/`destroy_irq` (the `disarm`/`destroy_timer` analog `timer.rs:72`/`:460` minus
  the splice walk: release the notif ref, clear the binding). Registered in `kcore/src/lib.rs`.
- `kcore/src/store.rs` — the `Store` trait: add the IRQ accessors mirroring the timer ones `:111-120`
  (`irq_notif`/`set_irq_notif`, `irq_bits`/`set_irq_bits`, `irq_intid`, `irq_bound`/`set_irq_bound`,
  `irq_masked`/`set_irq_masked`) and the `irq_view()` spec view (the `timer_view()` analog); the
  INTID→ObjId resolution seam `irq_for_intid(intid) -> Option<ObjId>` (the delivery-path lookup).
- `kernel/src/irq.rs` — **new** trusted int→ptr shell (the `kernel/src/timer.rs` mirror `:1-78`): the
  `IRQ_TABLE` static (one `IrqObj` slot per supported device SPI; the `ARMED_HEAD` analog
  `kernel/src/timer.rs:19`), `bind`/`unbind` wrappers calling the verified `kcore::irq` ops, the
  `deliver(intid)` path (lookup the `IrqObj`, call `notification::signal` on its bound notif), and the
  per-IRQ GIC mask/unmask helpers.
- `kernel/src/gic.rs` — `init` `:27` (extend with distributor `GICD_ISENABLER`/`GICD_IROUTER`/
  `GICD_IPRIORITYR`/`GICD_ICFGR` for device SPIs, beside the redistributor PPI enable `:38-39`); add
  per-IRQ `enable`/`disable`(mask)/`set_route` helpers beside `ack` `:52`/`eoi` `:58`; drop the
  "deferred IRQ caps" comment `:3-4`.
- `kernel/src/exceptions.rs` — `handle_el0_irq` `:209`: rework the non-timer else branch `:220-225` —
  on a *bound* device INTID, **signal the bound notification and mask the source** (do not silently
  EOI-and-drop), routed through `crate::irq::deliver`; an *unbound* INTID still EOIs and drops (no one
  to deliver to). The vtimer branch `:211-219` is unchanged.
- `kernel/src/syscall.rs` — add the `Sys::IrqBind` / `Sys::IrqAck` handlers (the `Sys::TimerArm`
  handler `:501-528` is the `IrqBind` template — slot resolve, type-check `CapKind::Irq`, WRITE-right
  on the notif, call the bind); the errno block `:59-73` (reuse existing errnos — no new errno).
- `kcore/src/sysabi.rs` — `enum Sys` `:43-69` (add `IrqBind { irq, notif, bits }` opcode 25,
  `IrqAck { irq }` opcode 26 after `AspaceTopUp` `:68`); `decode` `:111` (two new arms `:193`-adjacent,
  move the `nr >= 25 ==> UnknownCall` bound `:114` to `nr >= 27`); the decode tests `:204-247` (extend
  the known-calls + the "first unknown is now 27" case, the B10B test precedent `:235`).
- `ipc/src/sys.rs` — the userspace libcall surface (the `aspace_topup`/`timer_arm` precedent): add
  `irq_bind(irq, notif, bits)` (opcode 25) and `irq_ack(irq)` (opcode 26).
- `kernel/src/main.rs` — boot grant: create the PL011 `IrqObj` and write **two** new slots into init's
  cspace beside the device frames `:100-126` — the PL011 MMIO frame (`base: 0x0900_0000`, the
  virtio/RTC frame-cap pattern `:106-112`) and the PL011 IRQ-handler cap (`CapKind::Irq(intid 33)`),
  so init can delegate both to the console driver (C-M9); `gic::init` is already called `:57`.
- `doc/guidelines/verus_trusted-base.md` — the verified-surface scope paragraph `:17-18` (add "the IRQ
  object: the verified `irq_bind`/`destroy_irq` ops and the `irq_binding_refs` census term, the timer
  object's twin"); the Baselines kcore total `:140` (384 → the new total). **No `external_body`/
  `assume_specification` tally change** (`:111-112`; B-IRQ adds verified ops, not trusted seams — but
  see honesty note 5 for the one possible new opaque-size registration); **no `[verifying]` table edit,
  no §6.1 spec edit** (honesty note 4).

Secondary: `kcore/src/test_store.rs` (the array-backed in-memory `Store`: extend with the `irq_view` +
accessors so the verified ops execute in host unit tests — bind/unbind/destroy + the census round-trip);
`kcore/src/untyped.rs` (only if Design decision 3's *rejected* retyped-IRQ branch is taken — then an
`ExIrqObj` opaque registration `:248`-style joins the tally; the adopted boot-static branch touches it
**not at all**, honesty note 5).

---

## Verification tier & baseline (applies to all sub-phases)

B-IRQ's verified work is a single tier: the **`kcore` Verus chokepoint** (rev1§6 routing — the kernel
object core and syscall decode are Verus). The new IRQ object (`irq_bind`/`destroy_irq` + the
`irq_binding_refs` census term) and the extended `decode` join the `cargo verus verify -p kcore` gate;
the GIC register work, the `handle_el0_irq` device branch, the syscall handlers, the boot grant, and
the libcalls are trusted int→ptr shell (rev1§6.1(c)/(d), the same posture as the timer's
`kernel/src/timer.rs` shell over the verified `kcore::timer`). Five honesty notes up front:

1. **B-IRQ adds syscalls and therefore touches the verified decoder — but nothing else in the existing
   ABI shifts.** Two new opcodes (`IrqBind` = 25, `IrqAck` = 26) mean `kcore::sysabi::decode` gains two
   arms and its `nr >= 25 ==> UnknownCall` `ensures` `:114` becomes `nr >= 27`; the decode stays
   **total** over all `(nr, args)` (unknown → `UnknownCall`, never a panic) per rev1§2.7. `IrqBind`
   packs three raw `u64`s (`irq`, `notif`, `bits`) and `IrqAck` one (`irq`), none needing a range
   `ensures` (unlike `ThreadStart`'s `prio`) — they are validated downstream by the cap lookup + the
   verified bind. Existing opcodes 0..=24, their decode `ensures`, and every existing handler are
   byte-for-byte unchanged — `storaged`/`init`/`shell`/`loader` see identical signatures for everything
   they already call. The regression gate is the QEMU boot still green plus the new IRQ path exercised,
   not full ABI immutability (the B10B posture).

2. **The IRQ object is the timer object minus the hard part, plus the central census perturbation.**
   The IRQ binding is structurally the *timer's notification binding* — a (notif, bits) pair the
   object holds, with a refcount on the notification so revoking the notif cap cannot free it out from
   under a bound IRQ (the exact hazard `armed_timer_refs` guards for timers). So `irq_bind` is `arm`
   (`timer.rs:313`) and `destroy_irq` is `destroy_timer` (`:460`), **but with no armed list**: delivery
   is by direct INTID→`IrqObj` lookup (Design decision 2), not by sweeping a chain, so there is **no
   `timer_chain`/`timer_seq`/`timer_complete`/`disarm`-splice analog** — the single hardest part of the
   timer proof (the `remove_waiter`-shaped splice walk, `timer.rs:158-289`) is **absent**. What B-IRQ
   *does* inherit is the **central** cost: `obj_census` (`cspace.rs:4191`) gains a seventh summand, so
   every `census_delta_frozen`/`refcount_sound` frame in the teardown family (`signal`, `wait`,
   `remove_waiter`, `delete`, `obj_unref`, and the timer/notif/channel/thread destructors) must frame
   the new term. That perturbation — not the new ops — is why B-IRQ is L/high: it re-opens proofs
   across the whole object core, even though each individual edit is the mechanical "`irq_binding_refs`
   is framed because `irq_view` is untouched here."

3. **The gate is a floor that rises; no existing proof is weakened.** `cargo verus verify -p kcore` is
   **384/0** today (ledger `:140`). B-IRQ-A adds verified items — `irq_bind`/`destroy_irq`, the
   `irq_binding_refs` term + its `_pos`/`_unbind`/`_retarget` lemmas, the `cap_consistent(Irq)` arm,
   and the `obj_unref` Irq arm — and B-IRQ-B adds two decode arms (re-establishing `decode`'s `ensures`
   at the new bound), so the count goes **well above 384** (record the new total in the ledger). The 7
   `external_body` + 6 `assume_specification` seams (ledger `:111-112`) are **unchanged under the
   adopted boot-static design** (honesty note 5). B-IRQ adds verified ops; it does not widen the
   trusted base. The four kcore `external_body` (`ExTcb`/`ExNotifObj`/`ExTimerObj`/`fixed_object_bytes`)
   are untouched.

4. **No §6.1 `[verifying]` flip — B-IRQ is a conformance + additive verified-surface gain (like B10's
   `grow_pool` and B8C's ready queue).** rev1§6.1 carries no `[verifying]` tag for the IRQ object;
   rev1§1/§3.6 simply *list* it as a standing part of the object set. So B-IRQ makes **no normative
   §6.1 edit**: the IRQ object is a *new verified object in the object core*, not a trusted seam being
   drawn in. B-IRQ records the gain in the **ledger** alone — adding the IRQ object to the
   verified-surface scope paragraph (`:17-18`, beside "channels, notifications, timers") and bumping
   the baseline. The exception-entry shell (`handle_el0_irq`), the GIC register access, and the
   INTID→`IrqObj` int→ptr lookup stay trusted exactly as the timer's tick shell and `ARMED_HEAD`
   resolution do (§6.1(d)).

5. **The provenance model is a load-bearing design choice — flagged for sign-off (Design decision
   3).** The adopted answer — IRQ objects are **boot-static** (the kernel pre-creates one `IrqObj` per
   device SPI and grants init the handler cap, the device-MMIO-frame precedent) — keeps the trusted
   base **unchanged** (no new `ExIrqObj` opaque-size registration, no retype geometry) and sidesteps
   the seL4 IRQControl **uniqueness invariant** (one handler cap per INTID) entirely, because the fixed
   table is disjoint-by-construction. The principled alternative — IRQ objects **retyped from untyped**
   like timers, gated by an IRQControl cap with a verified per-INTID uniqueness invariant — is
   user-accounted and unbounded but adds an `ExIrqObj` seam (one new `external_body`) and a new
   structural invariant to the verified core (M-L of extra proof). B-IRQ's effort rating and trusted-
   base tally depend on this choice; it is the one decision to confirm before B-IRQ-A starts.

**Baseline to re-establish at end of B-IRQ:**
- `cargo verus verify -p kcore` ≥ **384/0**, **> 384** after B-IRQ-A/B (record the new total in the
  ledger). The 7 `external_body` + 6 `assume_specification`s unchanged (adopted boot-static design).
- The aarch64 build boots: `cd kernel && cargo build` + the QEMU boot smoke pass; a bound device IRQ
  wakes a waiting EL0 thread and the thread re-arms via `IrqAck` (the M-9-prerequisite acceptance,
  exercised by a synthetic harness — functional, not just compiling).
- `cargo test -p kcore` green (the `test_store` IRQ units: bind/unbind/destroy + the census round-trip;
  a bind that `+1`s the notif ref, an unbind/teardown that `-1`s it, `refcount_sound` preserved).
- `cargo build -p ipc` and the user binaries build against the new `irq_bind`/`irq_ack` libcalls.
- The ledger scope paragraph names the IRQ object; the kcore baseline `:140` reflects the final total;
  no §6.1 prose changed; the `external_body`/`assume_specification` tally `:111-112` unchanged.

---

## Design decision 1 — the IRQ object & cap representation: a verified `IrqObj` (the timer's twin) keyed in a census view *(the crux — resolve before B-IRQ-A)*

The parent plan writes `CapKind::Irq(intid)`. But the verified core tracks every binding's refcount by
**object id** through a per-type `Store` view and a census term keyed on `ObjId` (`obj_census`,
`cspace.rs:4191`; `armed_timer_refs(tmv, o)`, `:4118`). An IRQ binding must hold a ref on its bound
notification (else revoking the notif cap frees it under a bound IRQ — the timer's exact hazard), so it
needs a census home keyed by `ObjId`, like every other binding. The representation question is *where
that home lives*.

- **Adopted — `CapKind::Irq(ObjId)` designating a kernel `IrqObj`, the term-for-term twin of
  `TimerObj`, keyed in a new `irq_view: Map<ObjId, IrqView>` with a new `irq_binding_refs` census
  term.** Concretely:
  1. **The object.** `IrqObj { hdr: ObjHeader, intid: u32, notif: Option<ObjId>, bits: u64, bound:
     bool, masked: bool }` in `kcore/src/irq.rs` — the `TimerObj` analog (`timer.rs:26-34`) with
     `intid` replacing `deadline`, `bound` replacing `armed`, a `masked` line-state bit, and **no
     `next`** (no armed list — Design decision 2). The cap is `CapKind::Irq(ObjId)` designating it; the
     `intid` rides the *object*, not the cap discriminant (so the cap is a plain designating handle,
     uniform with `Notification(o)`/`Timer(o)`).
  2. **The census term.** Add `irq_binding_refs(iv: Map<ObjId, IrqView>, o: ObjId) -> nat` — the count
     of bound IRQ objects whose `notif == Some(o)`, a `dom().filter().len()` copy of `armed_timer_refs`
     (`:4118`). Add it as the seventh summand of `obj_census` (`:4196`-adjacent). `irq_bind` `+1`s the
     bound notif's ref and `irq_binding_refs` rises by one in lockstep; `destroy_irq`/`irq_unbind` `-1`
     in lockstep — exactly `arm`/`disarm`'s `armed_timer_refs` discipline, so `refcount_sound` carries.
  3. **The verified ops** (`kcore/src/irq.rs`): `irq_bind(store, irq, notif, bits)` — the `arm` analog
     (`timer.rs:313`) *without* the head-push: `irq_unbind` first (idempotent re-bind, net-zero on a
     same-notif rebind, the `arm`-calls-`disarm` precedent `:347`), `+1` on `refs[notif]`, set
     `notif`/`bits`/`bound`; `ensures` the timer-`arm`-shaped census-frozen + conditional
     `refcount_sound`. `irq_unbind`/`destroy_irq` — the `disarm`/`destroy_timer` analog
     (`:72`/`:460`) *without* the splice walk: if `bound`, release `refs[notif] -= 1`, clear the
     binding; `ensures` `census_delta_frozen` + the timer-`disarm`-shaped frames. Because there is no
     list, these are **straight-line** (no `while`/`decreases`/chain lemmas) — the proof is the census
     bookkeeping alone.
  4. **The cap-set arms.** `cap_obj`: `Irq(o) => Some(o)` (`:1353`-adjacent, a designating cap).
     `derived_kind`: **no new arm** — the `_ => k` fallthrough (`:1430`) already makes `Irq` a faithful
     designating copy (like `Notification`/`Timer`). `cap_consistent`: an `Irq(o)` arm (`irq_view` dom
     + finite + `irq_wf`: `bound ==> notif is Some && notif live`, the `Timer(o)` arm `:5326` shape).
     `obj_unref`: an `Irq(o)` arm → `destroy_irq` (the `Timer(o)` arm `:10118` template), with the
     matching per-kind `requires` (bound ⇒ notif live).
  - **Decisive reasons:** (a) it reuses the **proven** timer census machinery term-for-term —
    `armed_timer_refs` + its `_pos`/`disarm`/`retarget` lemmas are the literal template, so the new
    proofs are copies, not inventions; (b) the cap is uniform with the other designating caps, so
    `cap_obj`/`derive`/`obj_unref` extend by pattern, and `derived_kind` needs **zero** new code; (c)
    keying by `ObjId` is the only representation the census architecture supports — a bare `intid`
    discriminant has no home for the refcount the notification binding requires.
- **Rejected — `CapKind::Irq(intid)` (the parent plan's literal form) + a fixed kernel table indexed
  by `intid`, no backing object.** The binding (notif, bits, masked) would live in a `[IrqBinding;
  N_SPI]` array indexed by raw `intid`. But then there is **no `ObjId` to key the census by**: the
  notification's refcount must still count "+1 per IRQ bound to it," and `obj_census` is `ObjId`-keyed
  — so this either drops the refcount (unsound: revoke-frees-notif-under-IRQ) or bolts a parallel
  `intid`-keyed census onto the `ObjId`-keyed one (a second, incompatible accounting scheme in the
  verified core). The ObjId-designating form *is* the parent plan's intent (`intid` rides the object);
  this is the same object the parent plan describes, represented the way the proof architecture needs.
- **Rejected — reuse `TimerObj` (overload the timer object as an IRQ source).** A timer fires on a
  deadline sweep; an IRQ fires on a hardware line. Conflating them would force the armed-list sweep to
  carry IRQ objects it never expires (or special-case them), muddying the verified `check_expired`.
  Two clean objects sharing the *census template* but not the *object* is strictly simpler.

**Recommendation: adopt `CapKind::Irq(ObjId)` → `IrqObj`, the timer's twin, with an `irq_binding_refs`
census term and verified `irq_bind`/`destroy_irq` ops copied from `arm`/`destroy_timer` minus the
armed list. Confirm before B-IRQ-A.**

---

## Design decision 2 — the delivery & masking model: direct INTID→`IrqObj` lookup + verified `signal`, mask-on-deliver / unmask-on-ack *(resolve in B-IRQ-B)*

Today `handle_el0_irq` (`exceptions.rs:209`) acks the INTID, and for anything but the vtimer EOIs and
**drops** it (`:220-225`). A device IRQ must instead reach its bound notification and not storm a
level-triggered line before the driver services it.

- **Adopted — on a bound device INTID, look up the `IrqObj`, call the verified
  `notification::signal` on its bound notif, and *mask the line* (disable the INTID) before EOI;
  `IrqAck` unmasks.** Concretely:
  1. **Delivery (`kernel/src/irq.rs::deliver(intid)`, trusted shell).** Resolve the `IrqObj` via the
     `Store::irq_for_intid` seam (the INTID→`ObjId` int→ptr lookup, the `as_tcb`-style trusted
     resolution); if it is `bound`, call `notification::signal(&mut KernelStore, notif, bits)` — the
     **already-verified** delivery primitive `check_expired` uses (`kcore/src/timer.rs:847`) — set the
     `IrqObj`'s `masked` bit, and have the GIC **disable** the INTID (`GICD_ICENABLER`). Then EOI
     normally (priority-drop + deactivate). If *unbound*, EOI-and-drop as today (no receiver). There is
     **no list to walk**: the lookup is O(1), so delivery is "trusted lookup + one verified `signal`"
     — the timer's `check_expired` *without the sweep*.
  2. **Why mask-then-EOI (not EOI-and-leave-enabled).** The CPU-interface running priority must drop
     before returning to EL0, or no further interrupt (the 10 ms tick included) can preempt while the
     driver services the device in EL0 — so we **must** EOI (priority-drop + deactivate). But a
     level-triggered line still asserted (the driver has not yet read the device) would immediately
     re-pend if the INTID stayed enabled → interrupt storm. **Disabling the INTID** (`GICD_ICENABLER`)
     before EOI prevents the re-pend; the driver services the device in EL0, then `IrqAck` re-enables
     (`GICD_ISENABLER`). This is the seL4 mask-on-deliver / ack-unmask pattern with `EOImode = 0`.
  3. **`IrqAck(irq_cap)`.** Validate the IRQ cap, clear the `IrqObj`'s `masked` bit, re-enable the
     INTID at the GIC. The driver calls it after servicing — the "acknowledge" half of rev1§1's "the
     right to **receive and acknowledge** an interrupt."
  4. **`IrqBind(irq_cap, notif, bits)`.** The `TimerArm` handler (`syscall.rs:501-528`) term-for-term:
     resolve the IRQ slot + the notif slot (`ERR_BADSLOT` on null), destructure `CapKind::Irq(i)` /
     `CapKind::Notification(n)` (`ERR_TYPE` otherwise), require `WRITE` on the notif cap ("the kernel
     will signal through this cap," rev1§3.6, as `ChanBind` `:413` and `TimerArm` `:518` do), then call
     `irq::bind(irq_ptr(i), notif_ptr(n), bits)` → the verified `kcore::irq::irq_bind`. No new errno.
  5. **The GIC enable/route (`gic::init` + helpers).** Extend `init` (`:27`) with the distributor side
     the redistributor PPI path (`:38-39`) lacks: for each device SPI, `GICD_IPRIORITYR` (a priority
     below the mask), `GICD_ICFGR` (level-triggered for PL011 RX), `GICD_IROUTER` (affinity-route to
     core 0), and `GICD_ISENABLER` (enable). Add `enable(intid)`/`disable(intid)`/`set_route(intid)`
     helpers beside `ack`/`eoi` (`:52`/`:58`).
  - **Decisive reasons:** (a) delivery reuses the **verified** `signal` and the **verified** binding
    census — the only new *trusted* code is the GIC register pokes + the INTID lookup, the same trust
    surface as the timer's tick shell; (b) no armed list means no verified sweep — the delivery proof
    obligation collapses to "the looked-up notif is the bound one," an int→ptr fact (trusted, §6.1(d),
    like `ARMED_HEAD` resolution); (c) the mask/EOI model is the minimal correct level-triggered
    discipline and matches the seL4 precedent rev1§3.6 cites.
- **Rejected — EOImode-split (priority-drop on deliver via `ICC_EOIR1`, deactivate on ack via
  `ICC_DIR`).** GICv3 can split EOI into priority-drop and deactivate (`ICC_CTLR_EL1.EOImode = 1`),
  deferring deactivation to `IrqAck` instead of disabling the INTID. It is the "more correct" GIC
  idiom, but it changes a global CPU-interface mode the vtimer path also runs under (`gic.rs:43-47`),
  re-opening the timer's interrupt model for no functional gain at MVP. The disable/enable form is
  local to each device INTID and leaves the vtimer path untouched. Recorded as the cleaner-GIC
  alternative to adopt if a future driver needs nested same-priority device IRQs.
- **Rejected — EOI-and-leave-enabled (no mask).** Storms a level-triggered line: the device line stays
  asserted until the EL0 driver services it, so the INTID re-pends on every return-to-EL0, livelocking
  the core. Not viable for PL011 RX (the console use case).

**Recommendation: deliver via the verified `signal` + a trusted INTID→`IrqObj` lookup; mask
(`GICD_ICENABLER`) on deliver and EOI normally; `IrqAck` unmasks (`GICD_ISENABLER`). `IrqBind` copies
the `TimerArm` handler. Keep `EOImode = 0`; the EOImode-split is a future option.**

---

## Design decision 3 — provenance, accounting & the boot grant: boot-static IRQ objects granted to init *(the load-bearing sign-off — resolve before B-IRQ-A)*

rev1§1 (`:32`) has init hold "all device resources (MMIO frames, IRQ caps)" at boot. The device-MMIO
frames are **boot-static** — written directly into init's cspace slots in `main.rs` (`:100-126`), not
retyped from untyped. The question is whether IRQ objects follow that precedent or are user-retyped
like timers (which carry an `ExTimerObj` opaque-size seam, ledger `:105`, and retype geometry).

- **Adopted — boot-static: the kernel pre-creates a fixed `IRQ_TABLE` of `IrqObj` (one per supported
  device SPI) and grants init the handler cap, exactly as it grants the device-MMIO frame caps.**
  Concretely:
  1. **Creation (`kernel/src/irq.rs`).** A `static mut IRQ_TABLE: [IrqObj; N_SPI]` (the `ARMED_HEAD`
     static analog, `kernel/src/timer.rs:19`), each entry initialized with its `intid`, `bound: false`,
     `refs: 1` (the init grant) — the boot-static device-frame discipline. `N_SPI` is the small fixed
     set the platform supports (PL011 = INTID 33 at MVP; room for virtio = 32-tuple later).
  2. **The grant (`main.rs`).** Write **two** new slots into init's cspace beside the device frames
     (`:100-126`): the PL011 MMIO frame (`CapKind::Frame { base: 0x0900_0000, … }`, the virtio/RTC
     pattern `:106-112`) and the PL011 IRQ-handler cap (`CapKind::Irq(ObjId(&IRQ_TABLE[PL011]))`). init
     delegates an attenuated copy of each to the console driver (C-M9), exactly as it delegates the
     virtio frame to the block driver.
  3. **Accounting & teardown.** The init grant is one cap → `slot_refs` counts it (the boot-static
     timer/notif discipline). Delegating to the driver is `derive` (refs rise in lockstep). Revoking
     the grant runs the teardown SCC: `obj_unref`'s new `Irq` arm → `destroy_irq` releases the bound
     notif's ref (the `armed_timer_refs` analog) and the binding clears — the rev1§2.2 "revoke deletes
     descendants" path, re-verified over the widened cap set. The object's *memory* is kernel-static
     (not user-donated), so there is no untyped to return — consistent with the device-MMIO frames,
     which are also kernel-static device resources, not user memory.
  - **Decisive reasons:** (a) it keeps the **trusted base unchanged** — no `ExIrqObj` opaque-size
    `external_body` (the retyped branch needs one, ledger `:105` shape), no retype geometry to verify;
    (b) it sidesteps the seL4 **IRQControl uniqueness invariant** (one handler cap per INTID) entirely
    — the fixed table is disjoint-by-construction, so "two handler caps for one line" is impossible
    without a new structural proof; (c) it matches the device-MMIO-frame precedent init already uses
    for device resources, so the grant is a 2-slot addition to a pattern the boot code already runs.
- **Rejected — user-retyped IRQ objects gated by an IRQControl cap, with a verified per-INTID
  uniqueness invariant.** Retype an `IrqObj` from untyped (carrying the `intid` as the param), gated by
  a single IRQControl cap init holds, enforcing one live handler per INTID. This is the seL4 model:
  user-accounted, unbounded, principled. **Why rejected for B-IRQ:** it adds (a) an `ExIrqObj`
  opaque-size `external_body` to the trusted-base tally (`:111`), (b) a new IRQControl cap kind, and
  (c) a **structural uniqueness invariant** to the verified core (no two live `Irq` caps share an
  `intid`) — M-L of proof for a bounded hardware resource that the fixed table makes unique for free.
  The hardware INTID space is fixed and not user memory, so user-accounting it buys little at MVP. Kept
  as the principled fallback **iff** dynamic per-driver IRQ allocation is judged necessary (then B-IRQ
  rescopes to add the uniqueness invariant and the `ExIrqObj` seam).
- **Rejected — no cap at all (ambient IRQ delivery, like the debug-UART scaffold).** Contradicts
  rev1§1's "IRQ handlers — **caps**" and rev1§2's capability model; the whole point is that the console
  driver's authority to receive PL011 interrupts is a delegable, revocable cap. The ambient debug path
  is the disclosed time-boxed scaffold (rev1§7 `:433`) B-IRQ+C-M9 *retire*, not a model to copy.

**Recommendation: adopt boot-static IRQ objects granted to init (the device-MMIO-frame precedent),
keeping the trusted base unchanged and sidestepping the IRQControl uniqueness invariant. This is the
load-bearing sign-off (honesty note 5); fall back to the retyped+IRQControl model only if dynamic IRQ
allocation is required.**

---

## Sub-phase B-IRQ-A — the verified kcore IRQ object *(builds the rev0§1 object's verified core; conforms rev1§1/§3.6)*

The Verus deliverable and the long pole. Adds `CapKind::Irq` + the `IrqObj` + the `irq_binding_refs`
census term + the verified `irq_bind`/`destroy_irq` ops, and re-establishes every object-core proof
over the widened cap set and census. Independent of B-IRQ-B's shell wiring (B-IRQ-B consumes its
signatures). After B-IRQ-A the IRQ binding is a verified object operation: bind `+1`s the notif ref,
teardown `-1`s it, `refcount_sound` holds — the timer's census guarantees, for IRQs.

- **Touches:**
  - `kcore/src/cspace.rs` — `enum CapKind` `:117`-adjacent (add `Irq(ObjId)`); `cap_obj` `:1353` (Irq
    arm); `cap_consistent` `:5326`-adjacent (Irq arm, `irq_wf`); `obj_census` `:4196` (the
    `irq_binding_refs` summand); `armed_timer_refs` `:4118` + lemmas as the template for the new
    `irq_binding_refs` + `lemma_irq_binding_refs_pos`/`_unbind`/`_retarget`; `obj_unref` `:10118` (Irq
    arm → `destroy_irq` + its per-kind `requires`); audit the `census_delta_frozen`/`refcount_sound`
    frame family (`signal`/`wait`/`remove_waiter`/`delete`/the destructors) for the new census term
    (Design decision 1; honesty note 2 — the central perturbation). `derive` `:8914` re-verifies
    unchanged (cite it; `derived_kind`'s `_ => k` `:1430` needs no Irq arm).
  - `kcore/src/irq.rs` (new) — `IrqObj`; the verified `irq_bind` (⟵ `timer.rs:313` `arm`, minus the
    head-push) and `irq_unbind`/`destroy_irq` (⟵ `timer.rs:72`/`:460` `disarm`/`destroy_timer`, minus
    the splice). Registered in `kcore/src/lib.rs`.
  - `kcore/src/store.rs` — the IRQ accessors + `irq_view()` (⟵ the timer accessors `:111-120`) and the
    `irq_for_intid` resolution seam.
  - `kcore/src/test_store.rs` — extend the array-backed store with `irq_view` + accessors; host units:
    `irq_bind` `+1`s `refs[notif]` and `irq_binding_refs(notif)`; `destroy_irq`/`irq_unbind` `-1`s
    both; a same-notif rebind is net-zero; `refcount_sound` preserved across bind→teardown.
  - `doc/guidelines/verus_trusted-base.md` — record the raised kcore total `:140`; add the IRQ object
    to the verified-surface scope paragraph `:17-18`.
- **Depends on:** Part A blessed; Design decisions 1 & 3 signed off (representation + provenance). No
  intra-phase dependency (B-IRQ-B/C consume its signatures).
- **Work:** Design decision 1 — the object, the census term, the two verified ops, the cap-set arms,
  and the central `obj_census`-perturbation frame audit. The substance is **not** the new ops (they are
  straight-line `arm`/`disarm` minus the list) but threading the seventh census summand through the
  teardown family so `refcount_sound`/`census_delta_frozen` re-verify (honesty note 2).
- **Acceptance:**
  - `irq_bind`/`destroy_irq` verify with the `arm`/`destroy_timer`-shaped `ensures` (bind: `bound`,
    `notif`/`bits` set, `refs[notif] +1`, `census_delta_frozen`, conditional `refcount_sound`;
    teardown: binding cleared, `refs[notif] -1`, the same census frames); the `cap_consistent(Irq)` and
    `obj_unref(Irq)` arms verify.
  - `obj_census` carries the seventh term; `refcount_sound`/`census_dom_complete`/`caps_consistent`
    re-verify across the whole object core (the central perturbation discharged).
  - `cargo verus verify -p kcore` **> 384/0** (record the new total — the largest single bump);
    `cargo test -p kcore` green (the `test_store` IRQ units).
- **Effort/Risk:** L / high — the proof-engineering sub-phase. The new ops are cheap (no list); the
  cost is the `obj_census` perturbation rippling through the teardown family. Bounded by the timer
  template existing and green, but it re-opens proofs across the object core.

---

## Sub-phase B-IRQ-B — GIC SPI routing + device-IRQ delivery + the `IrqBind`/`IrqAck` syscalls *(builds the device-IRQ→notification path; conforms rev1§2.7/§3.6)*

The shell deliverable (trusted int→ptr, §6.1(c)/(d)) plus the one verified-decoder change. Adds the
GIC distributor SPI enable/route, the `handle_el0_irq` device branch (lookup + verified `signal` +
mask), and the two syscalls + libcalls. Depends on B-IRQ-A's `irq_bind`/`destroy_irq` signatures.
After B-IRQ-B a userspace thread can bind an IRQ cap to a notification, and a hardware interrupt on
that line signals the notification and masks the source until acked.

- **Touches:**
  - `kcore/src/sysabi.rs` — add `Sys::IrqBind { irq, notif, bits }` (opcode 25) and `Sys::IrqAck {
    irq }` (opcode 26) to `Sys` `:68`-adjacent; add the two decode arms `:193`-adjacent; move the
    `nr >= 25 ==> UnknownCall` bound `:114` to `nr >= 27`; re-establish `decode`'s `ensures` (Design
    decision 2.4; honesty note 1) — **the one verified-decoder change**. Extend the decode tests
    `:204-247` (known-calls for 25/26; "first unknown is now 27", the B10B `:235` precedent).
  - `kernel/src/irq.rs` (new shell) — the `IRQ_TABLE` static (Design decision 3.1; the `ARMED_HEAD`
    analog `kernel/src/timer.rs:19`); `bind`/`unbind` wrappers over the verified `kcore::irq` ops (the
    `kernel/src/timer.rs:42-50` `arm` wrapper pattern); `deliver(intid)` (lookup + `notification::
    signal` + mask, Design decision 2.1); the per-IRQ mask/unmask helpers.
  - `kernel/src/gic.rs` — extend `init` `:27` with the distributor SPI path (`GICD_IPRIORITYR`/
    `GICD_ICFGR`/`GICD_IROUTER`/`GICD_ISENABLER`) beside the redistributor PPI enable `:38-39`; add
    `enable`/`disable`/`set_route` helpers beside `ack` `:52`/`eoi` `:58`; drop the deferred-IRQ comment
    `:3-4`.
  - `kernel/src/exceptions.rs` — `handle_el0_irq` `:209`: rework the non-timer else branch `:220-225`
    to route a bound device INTID through `crate::irq::deliver` (signal + mask), unbound INTIDs still
    EOI-and-drop; the vtimer branch `:211-219` unchanged.
  - `kernel/src/syscall.rs` — the `Sys::IrqBind` handler (⟵ the `Sys::TimerArm` handler `:501-528`:
    slot resolve, `CapKind::Irq`/`CapKind::Notification` type-check, WRITE-right on the notif, call
    `irq::bind`) and the `Sys::IrqAck` handler (resolve the IRQ cap, clear `masked`, GIC-unmask). Reuse
    the existing errno set (`ERR_BADSLOT`/`ERR_TYPE`/`ERR_PERM`) — no new errno.
  - `ipc/src/sys.rs` — `irq_bind(irq, notif, bits)` (opcode 25) and `irq_ack(irq)` (opcode 26), the
    `timer_arm`/`aspace_topup` libcall pattern.
- **Depends on:** B-IRQ-A (the `irq_bind`/`destroy_irq` signatures). Independent of B-IRQ-C.
- **Work:** Design decision 2 — the decode arms + bound move, the GIC distributor enable/route, the
  `handle_el0_irq` device branch, the two handlers, the libcalls. Confirm the decode stays total (the
  rev1§2.7 negative case: opcode 27+ still `UnknownCall`). **No vtimer-path change** — the PPI delivery
  `:211-219` and `EOImode = 0` are untouched (Design decision 2's rejected EOImode-split).
- **Acceptance:**
  - `IrqBind`/`IrqAck` decode (opcodes 25/26) and dispatch; opcode 27+ still `UnknownCall`; a bind with
    a non-Notification or no-WRITE notif is refused (`ERR_TYPE`/`ERR_PERM`); the GIC routes + enables
    the PL011 SPI.
  - `cargo verus verify -p kcore` **> 384/0** (the decoder re-verifies with the two new arms); QEMU
    boot green; a synthetic EL0 thread binds the PL011 IRQ to a notification, blocks on it, and is
    woken when the line fires (the M-9-prerequisite acceptance).
  - `cargo build` (kernel) + `cargo build -p ipc` + the user binaries build against the new libcalls.
- **Effort/Risk:** M / medium. Mostly the GIC register work + the handler/delivery wiring + the small
  decoder change; the verified core is B-IRQ-A's. The judgment is the GIC SPI config (priority/route/
  trigger) and the mask/EOI ordering (Design decision 2.2).

---

## Sub-phase B-IRQ-C — boot grant + integration test + ledger closeout *(grants init the PL011 caps; conforms rev1§1)*

The conformance-closeout deliverable. Creates the PL011 `IrqObj`, grants init the PL011 MMIO frame +
IRQ-handler cap, proves the end-to-end path by QEMU integration test, and lands the ledger/baseline
updates. Depends on B-IRQ-A+B for the mechanism; can land alongside them so the ledger updates once.

- **Touches:**
  - `kernel/src/main.rs` — write the two new init cspace slots beside the device frames `:100-126`: the
    PL011 MMIO frame (`base: 0x0900_0000`, the virtio/RTC pattern `:106-112`) and the PL011 IRQ-handler
    cap (`CapKind::Irq(ObjId(&IRQ_TABLE[PL011]))`); bump the slot count comment block (Design
    decision 3.2).
  - `kcore/src/test_store.rs` / kernel integration — a QEMU smoke that a thread holding the PL011 IRQ
    cap binds it to a notification, blocks (`NotifWait`), is woken by a real PL011 RX interrupt
    (keystroke), reads the delivered bits, and `IrqAck`s to re-arm — and that a *second* interrupt
    after the ack is delivered (the mask/unmask cycle works); a teardown test that revoking the IRQ
    cap releases the bound notification's ref (accounting closes).
  - `doc/guidelines/verus_trusted-base.md` — finalize the kcore baseline `:140` (the B-IRQ-A+B total);
    confirm the verified-surface scope paragraph `:17-18` names the IRQ object; confirm the
    `external_body`/`assume_specification` tally `:111-112` is **unchanged** (the adopted boot-static
    design adds no opaque-size seam, honesty note 5); **no `[verifying]` table edit, no §6.1 spec edit**
    (honesty note 4).
  - `doc/spec/spec_rev1.md` — **no change** (rev1§1/§3.6 already bless the IRQ object; honesty note 4).
- **Depends on:** B-IRQ-A + B-IRQ-B (the mechanism). No new mechanism.
- **Work:** Design decision 3 — the boot grant; the end-to-end QEMU test (bind → block → hardware IRQ
  wakes → ack → re-fire); the teardown/accounting test; the ledger baseline + scope-paragraph
  finalization. Confirm revoking the IRQ cap runs `destroy_irq` and releases the notif ref (no leak,
  no double-free).
- **Acceptance:**
  - init holds the PL011 MMIO frame + IRQ-handler cap and can delegate both; a bound device IRQ wakes a
    waiting EL0 thread via its notification, which acks to re-arm, and a subsequent IRQ is delivered
    (the M-9-prerequisite acceptance, end to end in QEMU).
  - Revoking the IRQ cap releases the bound notification's ref (accounting closes); the QEMU teardown
    smoke is green.
  - The ledger scope paragraph names the IRQ object; the kcore baseline reflects the final total; the
    `external_body`/`assume_specification` tally is unchanged; no §6.1 prose changed.
- **Effort/Risk:** S–M / low–medium. Boot wiring + tests + docs; the heavy lifting is in B-IRQ-A/B.
  The judgment is the QEMU interactive-IRQ test harness (driving a real PL011 RX interrupt).

---

## Execution order

```
B-IRQ-A  verified kcore IRQ object (CapKind::Irq + IrqObj + irq_binding_refs + irq_bind/destroy_irq)
                                                        [Verus core; the long pole; independent]
B-IRQ-B  GIC SPI routing + handle_el0_irq delivery + IrqBind/IrqAck syscalls + decoder + libcalls
                                                        [shell + the one verified-decoder change; depends on B-IRQ-A]
B-IRQ-C  boot grant (PL011 MMIO frame + IRQ cap) + integration test + ledger closeout
                                                        [boot/tests/docs; depends on B-IRQ-A+B]
```

- **B-IRQ-A is the long pole** (the `obj_census` perturbation rippling through the teardown family),
  though it is template-driven by the timer's census machinery. **B-IRQ-B depends on B-IRQ-A's
  `irq_bind`/`destroy_irq` signatures** and carries the ABI change (the two opcodes + the decoder arm)
  and the GIC work. **B-IRQ-C** wires the boot grant and proves the end-to-end path — land it alongside
  B-IRQ-A/B so the kcore baseline and scope paragraph update once. Mirrors B5/B6/B7/B8/B9/B10's A/B/C
  decomposition.
- The parent plan sequences **B-IRQ after B8** so B8's freshly-verified kernel surface (cap-side MAP,
  priority gate, ready queue) is not churned by the `CapKind::Irq` cap-set widening; B-IRQ is otherwise
  independent of B9/B10 (it touches the cap census + a new object module, disjoint from B9's revoke
  marker and B10's aspace pool). It is the parent plan's "L / high (new *verified* kernel object + GIC
  work; the console track's long pole)."
- **The two sign-off gates are Design decisions 1 and 3** (the object representation and the provenance
  model, honesty note 5): together they set whether B-IRQ adds a new `external_body` (it does **not**
  under the adopted boot-static `IrqObj`) and whether a uniqueness invariant joins the verified core
  (it does **not** under boot-static). Confirm both before B-IRQ-A.

## Out of scope for B-IRQ (recorded so it is not mistaken for a gap)

- **The userspace console driver and the shell rewiring (C-M9).** B-IRQ builds the *kernel* IRQ object,
  its delivery path, and its syscalls, and grants init the PL011 caps. Writing the userspace PL011
  driver, making the "console cap" a channel to it, moving the shell off `sys::debug_*`, and retiring
  the debug-UART scaffold are **C-M9**, which depends on B-IRQ (this phase) **and C1** (the named-grant
  table delivering the console cap under `stdin`/`stdout`). B-IRQ is the prerequisite, not the console.
- **Retiring the virtio-blk poll (B2/I-4).** B-IRQ makes device-IRQ-driven drivers *possible* (the
  virtio-mmio IRQ can now be bound), but converting the block driver's used-ring spin to an
  interrupt-driven completion is a B2/driver follow-on, not B-IRQ's mechanism. Recorded as the bonus
  the parent plan names, not a deliverable here.
- **User-retyped IRQ objects + IRQControl + a per-INTID uniqueness invariant.** Design decision 3's
  rejected branch. B-IRQ adopts boot-static `IrqObj` granted to init (the device-MMIO-frame precedent),
  keeping the trusted base unchanged. Adopt the retyped model only if dynamic per-driver IRQ allocation
  is signed off as necessary (then B-IRQ rescopes to add the `ExIrqObj` seam + the uniqueness proof).
- **The EOImode-split GIC discipline (priority-drop / deactivate separation).** Design decision 2's
  rejected branch. B-IRQ uses `EOImode = 0` with mask-on-deliver / unmask-on-ack, local to each device
  INTID and leaving the vtimer path untouched. The EOImode split is a future option for nested
  same-priority device IRQs.
- **An armed-list / sweep analog for IRQs.** There is none (honesty note 2): delivery is by direct
  INTID→`IrqObj` lookup, so there is no `timer_chain`/`timer_seq`/`timer_complete`/`disarm`-splice
  analog to verify. The IRQ object is the timer's *census twin*, not its *list twin*.
- **A §6.1 `[verifying]` flip or any normative spec edit.** There is none (honesty note 4): rev1§1/§3.6
  already bless the IRQ object as a standing part of the object set, and the delivery uses the
  already-verified `signal`. B-IRQ records the gain in the ledger scope paragraph + baseline only; the
  exception-entry shell, the GIC register access, and the INTID→`IrqObj` lookup stay trusted exactly as
  the timer's tick shell and `ARMED_HEAD` resolution do (§6.1(d)).
- **Multiple device IRQs / a full SPI table.** B-IRQ delivers the PL011 line (INTID 33) as the console
  prerequisite and sizes `IRQ_TABLE` for the platform's device SPIs, but wiring every QEMU-virt device
  IRQ (virtio transports, RTC) is a follow-on grant, not B-IRQ's mechanism — the mechanism is uniform,
  so adding a line is a boot-grant addition (Design decision 3.2), not new verified code.
- **SGIs / IPIs and multi-core IRQ routing.** The system is single-core at MVP (`gic.rs:1`, "single
  core, group-1 only"); B-IRQ routes device SPIs to core 0. Inter-core interrupts and affinity routing
  beyond core 0 are out of scope.
- **Tuning the GIC priority / the `IRQ_TABLE` size.** Shell policy (the `GICD_IPRIORITYR` value, `N_SPI`),
  not verified parameters; B-IRQ picks safe defaults (a device priority below the mask, a table sized
  for the platform) and leaves sizing to the device set.
