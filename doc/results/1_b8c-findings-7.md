# B8C — Ready-queue verification: findings (part 7)

Working notes from the seventh implementation pass of **Phase B8C**
(`doc/plans/8_b8-detail.md`, Design decision 3). This pass is **B8C-3 — the kernel rewiring**:
the production `KernelStore` now executes the verified `kcore::ready` ops instead of the old
hand-written list logic. After this pass the *running* scheduler — not just the verified surface —
uses the mechanized ready-queue operations.

Continues `doc/results/1_b8c-findings-6.md` (which closed B8C-2, the seam integration, at
374/0 in-tree). Branch `b8c-ready-queue`; draft PR #138. Spec/plan refs: rev1§5.4, rev1§6.1(d),
audit §4.2.

---

## 0. Headline

**B8C-3 is done.** The kernel `enqueue`/`dequeue`/`top_ready`/`unqueue_ready` wrappers
(`kernel/src/thread.rs`) are now thin pointer-convert + `kcore::ready::*` calls via `KernelStore`,
exactly the established `cspace::delete` / `report_terminal` / `bind` / `set_priority` wrapper
pattern. `KernelStore::make_runnable`/`unqueue_ready` (`kernel/src/store.rs`) route through those
wrappers, so the verified `ready_enqueue`/`ready_unqueue` ops are what `signal`/`fire` and
`destroy_tcb` actually run in production. The old plain-unsafe-Rust ready-queue body — the audit
§4.2 finding — is **deleted**.

`cd kernel && cargo build` (debug + release) green; `cargo verus verify -p kcore` **374/0**
(unchanged — no kcore file touched); `cargo test -p kcore` green (94); the **QEMU boot smoke
reaches the interactive shell prompt** with the rewired scheduler. Behaviour-identical (B8 Honesty
note 2). No `external_body` / `assume_specification` added; no ledger/spec edit (the ledger baseline
and scope paragraph were already updated in B8C-2, and B8C-3 changes no verified item).

---

## 1. What was done

### 1.1 The four wrappers (`kernel/src/thread.rs`)

Each wrapper body is replaced by a single verified-op call; the **signatures are unchanged**, so
every call site (`maybe_switch`, `syscall.rs:470/636` spawn, `main.rs:162` idle) is untouched:

| wrapper | old body | new body |
|---|---|---|
| `enqueue(t: *mut Tcb)` | hand-written append-to-tail + `\|= 1<<prio` | `kcore::ready::ready_enqueue(&mut KernelStore, ObjId(t as u64))` |
| `dequeue(prio) -> *mut Tcb` | hand-written pop-head + clear-bit | `as_tcb(kcore::ready::ready_dequeue(&mut KernelStore, prio))` |
| `top_ready() -> Option<usize>` | `31 - READY_BITMAP.leading_zeros()` | `kcore::ready::top_ready(&KernelStore)` |
| `unqueue_ready(t: *mut Tcb)` | hand-written splice walk | `kcore::ready::ready_unqueue(&mut KernelStore, ObjId(t as u64))` |

Kept (the architectural shell the verified ops run *against*): the `READY`/`READY_BITMAP` statics,
the `as_tcb`/`tcb_id` link helpers, and the `ready_head_at`/… by-handle accessors (the
`Store::ready_*` realization). The module doc comment was refreshed to say the list *logic* is now
`kcore::ready` and only the backing + the policy (`maybe_switch`) + the asm switch stay shell.

Two small mechanical points:

- **`top_ready` must be fully qualified.** The local wrapper and the kcore op share the name; an
  unqualified `top_ready()` would recurse. `kcore::ready::top_ready(&KernelStore)` (note `&`, not
  `&mut` — it's a read-only bit-scan).
- **`dequeue` totality.** The old `dequeue` assumed a non-empty level (it `(*t).qnext`-derefs the
  head unconditionally). `ready_dequeue` is total: `None` on an empty level, which `as_tcb` maps to
  a null pointer. `maybe_switch` only ever dequeues a known-non-empty `top`, so the reachable
  behaviour is identical; the new form is also crash-safe on the (unreached) empty case.

### 1.2 The seam realizations (`kernel/src/store.rs`)

`KernelStore::make_runnable`/`unqueue_ready` are left **delegating** to `crate::thread::enqueue` /
`crate::thread::unqueue_ready` — which now route through the verified ops. This keeps a single
routing point and mirrors how the `ArrayStore` host model realizes the same seam (`test_store.rs`
routes through `ready_enqueue`/`ready_unqueue`). Added a comment recording that both realizations
now execute verified list logic, so `destroy_tcb`'s `store.unqueue_ready(t)` and `signal`/`fire`'s
`store.make_runnable(t)` discharge the `kcore::cspace` seam contracts (the `StoreSpec` lift of those
ops) with verified code rather than a trusted hand-written body.

---

## 2. Why this is sound (the load-bearing reasoning)

B8C-3 is a kernel-only change; the kernel crate is the trusted shell (not Verus-verified), so the
op `requires`/`ensures` are **erased** in the kernel build. The correctness argument is therefore
*behaviour parity* plus *no new aliasing*, not a proof.

### 2.1 Behaviour parity — field-by-field identical writes

The verified ops were written to perform exactly the old mutations (this is what made B8C
"behaviour-identical" in the first place). Confirmed term-for-term:

- **enqueue ↔ `ready_enqueue`**: set `state = Runnable`; `qnext = None`; if tail null set head else
  set old-tail `qnext = Some(t)`; set tail `= Some(t)`; `bitmap |= 1<<prio`. Identical.
- **dequeue ↔ `ready_dequeue`**: read head; `head = qnext(head)`; if now null clear tail + bit;
  popped `qnext = None`; return popped. Identical (for the non-empty case `maybe_switch` reaches).
- **top_ready ↔ `top_ready`**: `None` iff bitmap 0, else `31 - leading_zeros`. Identical.
- **unqueue_ready ↔ `ready_unqueue`**: walk from head tracking `prev`; on match, splice (head or
  predecessor `qnext` past `t`), tail-fixup if `t` was tail, `t.qnext = None`; clear bit if head
  now null. Identical.

### 2.2 No re-entrancy or aliasing hazard

- **No seam cycle.** The verified ops call only the low-level `Store` accessors (`set_tcb_*`,
  `ready_*`) — never the `make_runnable`/`unqueue_ready` *seam*. So the call graph is
  `seam → wrapper → verified op → accessors`, acyclic. (Crucially, `ready_enqueue`/`ready_unqueue`
  do **not** re-invoke `make_runnable`/`unqueue_ready`.)
- **`&mut KernelStore` nesting is sound.** `KernelStore` is a zero-sized unit struct; all real
  state lives in `static mut` reached through the accessors. `&mut KernelStore` constructs a fresh
  ZST temporary each call, so the seam method holding `&mut self` while a wrapper constructs a new
  `&mut KernelStore` is not a memory alias (no bytes are shared) — the same ZST-wrapper pattern the
  kernel already uses for `cspace::delete`, `bind`, `report_terminal`, `set_priority`.

### 2.3 No proof debt

No file under `kcore/` changed, so the Verus gate is definitionally unchanged at **374/0** (and the
forced re-verify confirmed it). B8C-3 adds no verified item and removes no proof; it only swaps
which body the kernel links for the four wrappers.

---

## 3. Gate state

| gate | B8C-2 (findings-6, in tree) | this pass (B8C-3) |
|---|---|---|
| `cargo verus verify -p kcore` | 374 / 0 | **374 / 0** (kcore untouched) |
| `cargo test -p kcore` | green (94) | green (94) |
| `cd kernel && cargo build` (debug + release) | green | green |
| QEMU boot | unchanged | **reaches `eunomia>` shell; scheduler rewired** |

Boot-smoke evidence (debug kernel, bare runner): `Eunomia OS` → `MMU + GICv3 + tick @ 100 Hz up`
→ `boot: init ELF loaded` → `entering EL0` → `[init] system up` → `Eunomia shell` / `eunomia>`.
Every context switch on that path (init ⇄ idle ⇄ storaged ⇄ shell) goes through
`maybe_switch` → the now-verified `top_ready`/`dequeue`/`enqueue`. (`[storaged] FATAL: no
virtio-blk device` is the expected no-disk-image behaviour of the default runner — unchanged from
before B8C-3.)

`external_body` seams and `assume_specification`s: **unchanged** (none added). No `[verifying]`
table edit, no §6.1 prose edit (Honesty note 4 — the ready queue has no blessed `[verifying]` tag);
no ledger edit (baseline 374 + the verified-surface scope paragraph were set in B8C-2, and B8C-3
changes no verified item).

---

## 4. What remains

**B8C-4** (optional, not load-bearing): deeper host-test assertions — `check_signal_frame` /
`check_destroy_tcb` could assert the precise bit-set / tail-position / splice-out beyond the
faithful-impl exercise the 94 tests already give — plus any ledger polish. Per findings-6 §5 this
is not load-bearing for the verified surface, which is complete: the ready-queue list logic is
verified in `kcore`, integrated through the `make_runnable`/`unqueue_ready` seams, **and now
executed in production by the kernel**. The audit §4.2 ready-queue item is closed end-to-end.
