# B-IRQ-B findings — GIC SPI routing + device-IRQ delivery + the IrqBind/IrqAck syscalls

Implementation notes from B-IRQ-B (`doc/plans/11_birq-detail.md`): the trusted int→ptr shell that
wires the device-IRQ→notification path end to end (the timer already proves this for its own PPI),
plus the one verified-decoder change. Builds on B-IRQ-A's verified kcore IRQ object (PR #144).

**Results:** `cargo verus verify -p kcore` **389/0 (unchanged)**; `cargo test -p kcore` 108 green;
`cargo build -p ipc` + the kernel (with the user binaries) build clean; QEMU boots to the shell
(`eunomia>`) exactly as before — the vtimer tick is untouched. Behaviour-preserving for everything
already running: the PL011 SPI is now routed + enabled at the GIC but bound to no one (the cap grant
is B-IRQ-C), so `deliver` EOI-drops it and nothing changes until a driver binds.

---

## 1. The gate stays 389/0 — B-IRQ-B adds no verified *item*

The plan's honesty-note-3 said B-IRQ-B's count goes "well above 384." That conflated *re-verifying*
with *adding*. B-IRQ-B's only verified-surface touch is two new arms in the already-counted
`kcore::sysabi::decode` plus the moved `nr >= 27 ==> UnknownCall` bound. Adding match arms to an
existing verified function does **not** create a new proof obligation item — `decode` was already one
of the 389. So the gate is **389/0, unchanged** (the +5 was all B-IRQ-A's: the three exec ops + two
lemmas). `IrqObj::boot_static` is a plain `const fn` outside `verus!` (like `init`), no obligation.

The rule: the gate count tracks *verified items* (exec/proof fns), not lines or arms. A phase that
only extends existing verified fns leaves the count flat — the B10B "count tracks items, not spec
fns" observation, one level up.

## 2. `irq_for_intid` is a trusted-shell function, NOT a `Store` seam

The plan's primary-files list put `irq_for_intid(intid) -> Option<ObjId>` in `kcore/src/store.rs`,
but B-IRQ-A had already decided otherwise (store.rs:125 comment) and it is correct: the reverse
lookup needs the `IRQ_TABLE`, which is kernel boot wiring with **no verified-core consumer** — the
verified core only ever reaches IRQ objects through the *forward* `irq_view`/`Store::irq_*` seam
(by `ObjId`, given to it by the syscall handler). So `irq_for_intid` lives in `kernel/src/irq.rs` as
a plain table scan returning `*mut IrqObj`. Adding it to the `Store` trait would have forced a spec +
two impls (`KernelStore`/`ArrayStore`) for a function the proofs never call. The Store trait was
**not touched** in B-IRQ-B at all — B-IRQ-A's accessors were already complete.

## 3. `deliver` calls the verified `signal` from trusted code — the standard boundary, not a new gap

The timer delivers via the *verified* `kcore::timer::check_expired`, which calls
`notification::signal` discharging its preconditions in-proof. B-IRQ deliberately puts `deliver` in
the **trusted** shell (`kernel/src/irq.rs`), calling `notification::signal` directly, so signal's
preconditions (`notif_wf`/`ready_wf`/`ready_complete`/the waiter-ref) are *assumed* at the call site.

This is **not** a new soundness gap: it is the exact same kernel→kcore trust as every other handler
in `kernel/src/syscall.rs` — `timer::arm`, `channel::bind`, `notification::signal` for `NotifSignal`
all call verified ops with preconditions assumed (the kernel crate is not verified). The IRQ path's
only *trusted-new* code is the GIC pokes + the INTID→object lookup, the same trust surface (rev1§6.1
(c)/(d)) as the timer's tick shell and `ARMED_HEAD` resolution. The decision to make `deliver`
trusted (rather than a verified kcore op) is what buys "no armed-list / no verified sweep" — delivery
is an O(1) table lookup, so there is nothing list-shaped to prove.

## 4. The GIC SPI path is small because the bring-up already did the hard parts

`gic::init` already sets `GICD_CTLR = ARE_NS | EnableGrp1` (affinity routing live) and `ICC_PMR =
0xFF` (mask wide open). So routing a device SPI needs **no CPU-interface change** — only the
distributor side the redistributor-PPI path lacks: group-1 bit, a priority byte (`0xA0 < 0xFF` so it
passes), level-trigger (clear the ICFGR edge bit), `IROUTER = 0` (affinity 0, IRM=0 → core 0), and
the enable bit. The per-INTID `enable`/`disable` double as the delivery path's mask/unmask
(mask-on-deliver / unmask-on-ack, `EOImode = 0`; the EOImode-split is the deferred future option).

Register banking for INTID ≥ 32: IGROUPR/ISENABLER/ICENABLER are 1 bit/INTID (word = intid/32);
IPRIORITYR is 1 byte/INTID; ICFGR is 2 bits/INTID (word = intid/16); IROUTER is one 64-bit
word/INTID at `GICD_BASE + 0x6000 + 8*intid`.

## 5. Storm safety with an enabled-but-unbound line (B-IRQ-B has no driver / no cap grant)

`init()` routes + enables the PL011 SPI at the GIC, but B-IRQ-B grants the cap to no one, so the line
is enabled-but-unbound. A level-triggered line that asserted with no receiver would storm
(`deliver` EOI-drops → re-pend on every return-to-EL0). This is safe at B-IRQ-B because the **device**
gate is independent: the PL011's own `UARTIMR.RXIM` is off until a driver sets it (C-M9), so the line
never asserts. The boot smoke confirms it (no storm, shell responsive). When B-IRQ-C grants the cap
and a driver binds + enables RXIM, `deliver` masks on the first interrupt and `IrqAck` unmasks —
the level line is serviced, never stormed. (If defensiveness is later wanted, `deliver` could also
mask *unbound* lines before EOI; the plan keeps the simpler "EOI-drop unbound" per Design decision 2.)

## 6. Small mechanical notes

- **`irq` module vs. `irq` field name clash** in the syscall handlers: the decoded `Sys::IrqBind`
  field is named `irq` (the slot index), which would shadow `use crate::irq;`. Destructure it as
  `irq: irq_cap` so `irq::bind(...)` still resolves to the module.
- **`IrqObj::boot_static` as a `const fn`** lets the kernel write `static mut IRQ_TABLE: [IrqObj;
  N_SPI] = [IrqObj::boot_static(PL011_INTID)]` directly — no `MaybeUninit`, no runtime init of the
  storage (only the GIC routing is runtime, in `irq::init`). Boot-static (Design decision 3) means
  the objects live in the kernel image, so no `ExIrqObj` opaque-size seam — the trusted-base tally
  stays at 13.
- **No ledger edit.** B-IRQ-A already added the IRQ object to the verified-surface scope paragraph
  and set the kcore baseline to 389; `sysabi::decode` was already listed; the ledger does not
  enumerate the opcode range. The `external_body`(7)/`assume_specification`(6) tally is unchanged.

## 7. What B-IRQ-C still owes

The boot grant (init's cspace gets the PL011 MMIO frame + `CapKind::Irq` cap), the end-to-end
functional QEMU test (bind→block→hardware IRQ wakes→ack→re-fire), and the teardown/accounting test
(revoke releases the bound notif's ref). All need the cap grant, so they are genuinely B-IRQ-C; the
mechanism they exercise is complete as of B-IRQ-B.
