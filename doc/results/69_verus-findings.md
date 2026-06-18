# 69 — Verus drift audit against spec rev2 (fresh-eyes re-derivation)

## Method and provenance

This ledger re-derives, section by section, the property each verified component
*ought* to guarantee from the text of `doc/spec/2_spec_rev2.md` **alone** — written
before opening any `ensures` — then compares it against the actual `spec fn` model,
`requires`, and `ensures` in the Rust source. Authority is the spec file and the source
only. Accumulated justification prose — `doc/results/*`, `doc/plans/*` obligation
tables, and code comments citing "plan §6e / doc 55" — was deliberately ignored: a
comment asserting a property is treated as a *claim to verify*, never as the proof. The
proven guarantee of a function is exactly its `ensures` under its `requires`, nothing the
comments add.

The audit ran as a fan-out of eight independent section-auditors (§2.2 revocation/CDT,
§2.3 attenuation, §2.5 memory, §3.1–3.4 channels, §3.6 notifications/timers, §5
threads, §4 storage commit/recovery, and a cross-cutting refcount/§6 pass), each
followed by an adversarial verifier that re-opened spec and code to confirm, refute, or
reclassify every candidate. 35 candidates were raised; 24 survived adversarial
verification, 11 were refuted. The two high-severity findings (**D-A1**, **D-B1**) and
all three watch-list items were then re-verified by hand against the actual
`requires`/`ensures` and the kernel call path (`cur_slot` → unverified wrapper →
`kcore::cspace::revoke`); both held.

**Headline verdict: the verified code is substantially faithful, with one systemic
faithfulness gap and a recurring pattern of honest-but-unstated proof boundaries.**
Where Verus proves a property, it proves it well: rights-mask monotonicity is
`∀`-quantified (not sampled); the CDT `revoke` loop structurally forces *every*
transitive descendant gone (not just direct children); the channel FIFO `Seq` model and
the notification waiter queue are faithful deterministic encodings; mount geometry
validation and WAL-replay bounds are total over arbitrary bytes; the at-most-one
terminal-report guarantee is grounded on the report field, not on an unverified
scheduler. The genuine concerns are: (1) `revoke`'s `!is_homed` precondition is **false
for every real syscall invocation**, so the proven contract is vacuous over the
implementation's actual inputs — the single highest-severity finding; (2) the §5.4
priority lattice has **no verified model at all**; (3) a family of leaf ops drops a
soundness postcondition the spec-load-bearing invariant needs; and (4) several spec
safety properties are delivered only up to honest trusted-base seams (physical-region
exclusivity, cap↔PTE correspondence, the DMA PA boundary, commit atomicity) that the
spec itself partly routes to TLA+. No drift below is a green proof against a *drifted*
obligation — the `ensures` honestly state what they prove; the gaps are properties the
spec assigns that the verified core does not reach. **No drift is closed silently:**
every entry carries a disposition (fix-now / follow-on / accepted-documented), and the
accepted-and-fix-now items feed the 9e spec/CLAUDE.md edits in the final section.

## Watch-list resolution

**(1) revoke root-survival / `!is_homed` vs "revoke destroys all descendants" + the
seL4-zombie case.** Resolved into two distinct facts that the original framing conflated.

- The spec's *descendant-destruction* guarantee (§2.2 "deletes all descendants … the
  guarantee is unconditional, with no 'except messages in flight' caveat") IS faithfully
  delivered: `revoke` (cspace.rs:9291) ensures `final.slot_view()[slot].first_child is
  None` (9307) plus `cspace_wf` (9305), and via `parent_has_first_child` (1241) +
  `empty_slots_detached` (1247) + acyclicity this structurally forces every live
  transitive descendant gone. That part of the `ensures` does **not** depend on
  `!is_homed`.
- The spec never promises the revoked *root* cap survives — that is an *extra* `ensures`
  (`!is_empty_cap`, 9309) the implementation adds. The seL4-zombie test
  (`revoke_can_empty_its_own_root_zombie`, test_store.rs:2764, passing) shows a homed
  root **is** emptied when its subtree holds the homing cspace's last cap. Root-survival
  is therefore genuinely conditional — narrower than what §2.5's reuse pattern and the
  TLA+ row-(b) model assume. **The spec admits the self-empty case** (it never forbids
  it); the code's `!is_homed` precondition is the honest acknowledgement of it.
- The decisive problem (escalated to **high**, D-A1): `!is_homed` is a precondition of
  the *whole function*, and the only revoke entry path (syscall.rs:247 → `cur_slot`
  (99–107, a resident of the caller's cspace) → unverified kernel wrapper
  kernel/src/cspace.rs:19 → kcore `revoke`) always supplies a target that is
  `homed_in_cspace`, so `!is_homed` is **always false** for real syscalls. The entire
  verified contract — descendant-deletion included — is unreachable from the
  implementation's actual inputs; on the real call shape it is witnessed only by an
  executable unit test, not by a Verus `ensures`.
- **Disposition:** survival-on-homed and sees-through-queues residue are **follow-on**;
  the contract split that makes the descendant-deletion guarantee reachable from the real
  call path is **follow-on but highest-priority** (it is the difference between "proven
  for the kernel" and "proven only for inputs the kernel never supplies"). The runtime
  code is correct either way — preconditions erase — so this is a verification-scope gap,
  not a code defect.

**(2) Phase-8 commit-recovery = structural/arithmetic half, content-coverage to TLA+;
anything reading as "fully mechanized" when it is not.** Resolved: the Verus surface over
the WAL/superblock mechanizes exactly the **in-bounds / totality / maximal-run** half
(`replay_bound` store.rs:651, `decode_frame` store.rs:520, `lemma_gap_freedom`
store.rs:843, `commit_target` store.rs:381). Per-record **content acceptance is
uninterpreted** (`content_ok_spec` store.rs:583 is `uninterp`; `wal_content_ok` is
`external_body`), and `commit()` itself (store.rs:1561) is plain Rust outside all
`verus!` blocks. This split is **spec-consistent**: §6 row (a) assigns the commit/recovery
*protocol* (the "recovered state = committed roots + replay" invariant) and atomicity to
**TLA+**, with the Verus row struck through. So there is **no spec violation** and **no
in-code overclaim** (no `ensures` asserts the full replay invariant; `lemma_gap_freedom`
honestly labels itself the "shadow"/"half"). **Disposition: accepted-documented** — the
only live risk is a *downstream* artifact reading §6 row (a) as "fully Verus-mechanized,"
which would be wrong (half is TLA+; the content half is mechanized nowhere on the real
code). That honest-boundary note is an input to 9e.

**(3) `refcount_sound` claimed as a system invariant vs proven per-op-conditionally.**
Resolved: `obj_census` (cspace.rs:3326) is a *faithful* model — it sums all six
reference-bearing slot kinds the spec demands (ordinary slots, channel bindings,
in-flight ring slots, notification waiters, timer→notif, frame→aspace, TCB→cspace/aspace).
The CDT single-cap `delete` primitive preserves it unconditionally (cspace.rs:8990), and
the **teardown family is closed** over it (`obj_unref`/`destroy_*` require *and* ensure
it; the destroy preconditions are discharged from `refcount_sound` at `obj_unref`). But
"system invariant" is **stronger than the `ensures` prove**: (a) no verified entry point
establishes it from a base state — it is only ever a `requires` or an `old⇒final`
implication, with from-scratch construction living only in host test fixtures and the
`unsafe` boot shell; and (b) the preserving set is **not closed at the contract level** —
`timer::arm`, `timer::disarm`, `notification::wait`, `channel::send`, `channel::recv` do
not export it, and `thread::bind` requires-but-does-not-ensure it. These ops *do*
preserve it in fact (the census moves in lockstep; `slot_move` holds `refs_view`
literally fixed; `wait`/`disarm` even export the exact refs delta — `arm` does not even do
that), so this is a missing-obligation, not a latent unsoundness. Crucially, the spec's
retype-exclusivity linchpin (§2.2) is delivered by the proven **watermark + CDT**, *not*
by `refcount_sound`, so the gap does not touch the headline safety property.
**Disposition: follow-on** for the leaf-op postconditions (D-E1/D-F2); the model itself is
a confirmed faithful match, and the "system invariant" wording in comments should be
softened to "per-op contract" (9e).

## Drift ledger

| ID | Title | Class | Severity | Disposition |
|---|---|---|---|---|
| D-A1 | `revoke`'s `!is_homed` precondition is false for every real syscall target; verified contract vacuous over actual inputs | D2 | high | follow-on (top priority) |
| D-A2 | Root-survival gated by `!is_homed`; homed (seL4-zombie) target's cap can be emptied | D2 | medium | follow-on |
| D-A3 | "revoke sees through queues" is only structurally implied, never an `ensures`/obligation | D1 | medium | follow-on |
| D-B1 | §5.4 max-controlled-priority ceiling: no model on the cap, monotonicity entirely unverified | D1 | high | follow-on |
| D-B2 | Untyped sub-range "+ rights mask" realized as a fixed READ\|WRITE strip (no caller mask) | D3 | low | accepted-documented |
| D-C1 | Retype does not prove physical-region exclusivity; reduced to "no immediate CDT child" via a trusted bridge | D3 | medium | accepted-documented |
| D-C2 | Cross-untyped non-overlap unmodeled (independent root untypeds) | D1 | medium | accepted-documented |
| D-C3 | "delete unmaps it" rests on a trusted uninterpreted `aspace_unmap` seam + unverified cap↔PTE bridge | D1 | medium | accepted-documented |
| D-D1 | `recv` does not export cap-installation into the receiver's cspace (receive-half of move semantics) | D1 | medium | follow-on |
| D-D2 | `destroy_channel` does not fire peer-closed; whole-object teardown firing realized indirectly via per-cap delete | D3 | low | accepted-documented |
| D-E1 | `wait`/`arm`/`disarm` carry no refcount-soundness obligation; `bind` requires-but-omits it | D1 | medium | follow-on |
| D-E2 | `check_expired` excludes N-timers→1-notification (`timer_notif_injective`) | D2 | medium | follow-on |
| D-E3 | `signal` does not wake a waiter when the resulting word is zero | D3 | low | accepted-documented |
| D-E4 | Timer expiry is a `>=` tick sweep, not the exact CNTV compare register | D4 | low | accepted-documented |
| D-F1 | Suspend-not-destroy: the suspended-state half is set in the unverified shell; model proves no report↔state link | D1 | medium | accepted-documented |
| D-F2 | `bind` drops `refcount_sound` from its `ensures` (the -1 delta IS proven inside `delete`, just not re-exported) | D1 | low | fix-now |
| D-F3 | Report-mechanism access control (only cap holder configures; no self-silencing) is unverified shell | D1 | medium | accepted-documented |
| D-F4 | `thread_exit`/`read_report` syscall plumbing unverified; only the record-write is in kcore | D4 | low | accepted-documented |
| D-G1 | Per-record WAL content-acceptance is uninterpreted; content-coverage to TLA+, not mechanized on real code | n-a-d | low | accepted-documented |
| D-G2 | `pick_survivor` tie-break (equal valid generations ⇒ slot A) resolves a case the spec's "higher generation" rule treats as impossible | D3 | low | accepted-documented |
| D-H1 | §6 verification table is stale: marks Verus deferred / names retired Kani tier; reality is Verus-verified | D4 | low | fix-now |
| D-H2 | §6 row (a) silent on the real Verus structural proofs over cas WAL/superblock | D4 | low | follow-on |

---

### D-A1 — `revoke`'s `!is_homed` precondition excludes every real revoke target

**Class D2 · Severity high · Disposition follow-on (highest priority)**
**Spec anchor:** §2.2 (line 50): "`revoke(cap)` eagerly deletes all descendants … the
guarantee is **unconditional**, with no 'except messages in flight' caveat"; §6 row (b):
"checked unconditionally."
**Code anchor:** kcore/src/cspace.rs:9303 `requires … !is_homed(old(store), slot)` on
`pub fn revoke` (9291). `is_homed` (3593) = `homed_in_cspace || homed_in_chan ||
homed_in_tcb`; `homed_in_cspace` (3571) `:= ∃ cs,i: cspace_view[cs].slots[i] == x`, true
for any cap that is a cell of some cspace. Real path: syscall.rs:247 `Sys::CapRevoke` →
`cur_slot(slot)` (99–107: `CSpaceObj::slot(cspace_ptr(current.cspace), idx)`, a cell of
the calling thread's cspace) → unverified kernel wrapper kernel/src/cspace.rs:19 (no
`requires`) → kcore `revoke`. **There is no verified caller of `revoke` inside kcore.**
**Spec says vs code proves:** the spec promises descendant-deletion unconditionally for
any cap. The code proves it (plus a non-spec root-survival clause) only when `!is_homed`
— i.e. when the target is *not* a cspace cell. Every syscall revoke target is `cur_slot`,
hence `homed_in_cspace`, hence `!is_homed` is **always false**. The verified contract —
including the spec-mandated descendant-deletion — is vacuous over the implementation's
actual inputs; on the real shape it is witnessed only by the passing executable test
test_store.rs:2764, not by a Verus `ensures`. (Independently re-verified by hand: the
slot model, the `cur_slot` body, and the absence of any verified kcore caller all confirm
the claim.)
**Disposition text:** split the contract so `first_child is None` + `cspace_wf` are
proven **without** `!is_homed` — they already hold from `delete`'s unconditional contract
plus the `decreases`, independent of homing — keeping the extra `!is_empty_cap`
root-survival under `!is_homed` (or as a separate lemma). This restores a verified
descendant-deletion / see-through-queues guarantee reachable from the real (homed) call
path. Until then, the doc comment at revoke's contract should state plainly: *"this
contract is currently unreachable from the CapRevoke syscall, which always supplies a
homed target; the spec-mandated descendant deletion on the real path is witnessed only by
`revoke_can_empty_its_own_root_zombie`."*

### D-A2 — Root-survival conditional on `!is_homed`; homed target can be self-emptied

**Class D2 · Severity medium · Disposition follow-on**
**Spec anchor:** §2.5 (line 105): "revoking the parent cap unmaps every sharer
everywhere" (granter retains the parent); the §5.1/§2.5 reclaim flow (`revoke(untyped)`
then `reset`) relies on the untyped cap surviving the revoke.
**Code anchor:** kcore/src/cspace.rs:9309 `ensures !is_empty_cap(final.slot_view()[slot].cap)`
proven only under `requires !is_homed` (9303). Negative witness: test_store.rs:2764–2802
asserts `st.at(SlotId(0)).cap.is_empty()` after revoking a homed root whose subtree holds
the homing cspace's last cap.
**Spec says vs code proves:** the reuse pattern relies on the granter retaining the
(cspace-resident, hence homed) parent cap after revoke. The code proves survival only for
un-homed roots; a homed root whose subtree holds its homing cspace's last cap is
self-emptied by the cross-object destructor cascade. For the normal non-zombie reclaim
flow a top-level untyped donation can be arranged un-homed, so the proof applies there;
the zombie shape arises only in a self-referential configuration the §2.5 grant/reuse
pattern never constructs. The spec admits the self-empty (it never forbids it), so this is
a faithfulness *gap* (survival narrower than the reuse pattern silently assumes), not a
contradiction.
**Disposition text:** §2.2/§2.5 should gain an honest note that root-survival across
revoke is guaranteed for un-homed (e.g. donated-untyped) targets; the seL4-zombie
self-empty of a homed root whose subtree holds its homing object's last cap is admissible.
The resident-with-external-reference case needs the refs-monotone frame already flagged as
residue at cspace.rs:9282–9286.

### D-A3 — "revoke sees through queues" only structurally implied

**Class D1 · Severity medium · Disposition follow-on**
**Spec anchor:** §3.4 (line 155): "Revocation therefore finds and deletes in-flight caps
like any other descendants — no special case in the revoke logic, no caveat in its
specification"; §6 row (b) "in-flight caps included." M1 (line 435) requires "revoke
verifiably destroys descendants **including a cap queued in an in-flight message**."
**Code anchor:** kcore/src/cspace.rs:9304–9309 revoke `ensures` mention no
`ring_cap`/queue/`in_live_window`; `slot_move` (cspace.rs ~7792) inherits the parent edge
into the ring slot **in its body** but its `ensures` carries no link-field postcondition;
`send`'s `ensures` (channel.rs:676–701) says nothing about the moved cap's parent edge.
**Spec says vs code proves:** the named, load-bearing sees-through-queues property is
*provable* from the structural invariants (a queued cap is a real CDT descendant, and
`first_child is None` + cdt_wf force the whole subtree empty) but is never a Verus
`ensures`, lemma, or driven test. The only nod (test_store.rs ~2965) *simulates* an
emptied ring slot. TLA+ checks it explicitly (`queues' = … \ dead`); Verus does not. This
is the one M1 exit-criterion property whose Verus witness is absent.
**Disposition text:** add a `subtree_empty`/`no_live_descendant` predicate and an
`ensures` (or lemma) on `revoke` asserting a queued cap descended from the target is empty
afterward, plus a test driving real `revoke` through a queued descendant. Until then,
document at revoke's contract that queue-reaching is *inferred* from cdt_wf +
`slot_move`'s body, and a change to either could silently break it with no failing
obligation.

### D-B1 — §5.4 priority ceiling: no cap model, monotonicity entirely unverified

**Class D1 · Severity high · Disposition follow-on**
**Spec anchor:** §2.3 (line 71): "The §5.4 maximum-controlled-priority ceiling is a
**value on the cap**, not a bit, and **attenuates the same monotone way**"; §5.4 (line
360): "the priority lattice is monotone like every other derivation (§2.3)."
**Code anchor:** kernel/src/syscall.rs:414 / :545 `if prio > (*thread::current()).priority
{ return Some(ERR_PERM); }`; `CapKind::Thread(ObjId)` (cspace.rs:110) has no priority
field; priority exists only as `Tcb.priority: u8` (thread.rs:75); `derive`
(cspace.rs:7196) attenuates `rights & mask` only and never mentions priority.
**Spec says vs code proves:** the spec makes a live verified-invariant claim — the
priority lattice is monotone "like every other derivation," and every *other*
derivation's monotonicity is a proven `ensures`. The verified core proves **nothing**
about priority: there is no priority value on any cap (not even a bit), and the only
enforcement is an unverified `if` gating on the *caller thread's live run-priority* rather
than a cap-carried ceiling. So the priority axis is both *modeled differently* from the
spec (caller-priority vs cap-carried MCP) and *outside the verified boundary entirely*.
The runtime check is real (a child cannot spawn above the caller's priority), so this is a
verification/faithfulness gap, not an exploitable hole — hence not fix-now. (Re-verified
by hand: `CapKind::Thread` carries only an `ObjId`; the gate is on `current().priority`.)
**Disposition text:** to restore faithfulness, a `max_prio: u8` field would be added to
`CapKind::Thread`, `derive` would attenuate it monotonically (`child.max_prio ≤
parent.max_prio`), and spawn would gate on the cap's ceiling with a verified `ensures
child.priority ≤ ceiling`. Until then, §2.3/§5.4 must carry an honest note that the
priority axis of the monotone lattice is **not verified** and is enforced by an unverified
kernel-shell guard on caller priority.

### D-B2 — Untyped "+ rights mask" is a fixed READ|WRITE strip

**Class D3 · Severity low · Disposition accepted-documented**
**Spec anchor:** §2.3 lattice table: "Untyped / memory | sub-range (page-aligned) **+
rights mask**."
**Code anchor:** kcore/src/cspace.rs:7245 `derive` returns `Err` on `CapKind::Untyped`;
the only sub-untyped path is untyped.rs retype, ensuring `dst.rights.0 == (parent.rights.0
& (READ|WRITE))` with `(rights & PHYS) == 0` (∀-proven by `bit_vector`), range
containment + 4096 alignment proven (`carve_place`).
**Spec says vs code proves:** the table lists a rights mask as a deriver-chosen
attenuation, by symmetry with the adjacent rows. The code pins it to `parent.rights &
(READ|WRITE)` — caller cannot choose. The chosen reading is monotone and security-positive
(PHYS provably cannot flow down ordinary derivation chains), and for an untyped the entire
meaningful rights universe is {READ, WRITE}, so there is essentially nothing for a caller
mask to attenuate. The spec is genuinely underspecified about *who* chooses the untyped
mask.
**Disposition text:** §2.3 should note that the untyped row's rights mask is realized as a
fixed READ|WRITE strip (provably clearing PHYS), not a caller-supplied mask, since
READ/WRITE are the only meaningful untyped rights.

### D-C1 — Retype does not prove physical-region exclusivity

**Class D3 · Severity medium · Disposition accepted-documented**
**Spec anchor:** §2.2 (line 50): "retyping untyped memory is only sound if the kernel can
establish that no outstanding caps reference the region, and **revoke is how exclusivity
is proven**."
**Code anchor:** kcore/src/untyped.rs:474–546 `retype_install` ensures (advances
watermark, installs CDT child, **no exclusivity clause**); `reset` (698–717) requires only
`first_child is None`. `cspace_wf` (cspace.rs:1306) = `cdt_wf && acyclic && sib_acyclic` —
purely pointer structure; no `spec fn` relates a `Frame{base,pages}` range to an untyped's
`[base,size)`.
**Spec says vs code proves:** "no outstanding caps reference the region" is
operationalized as "the untyped has no immediate CDT child." The bridge — every cap into
the carved physical region is a CDT descendant of this untyped — is established **by
construction** (the only Frame-creation path is `retype_install`, which sets `parent ==
Some(ut_slot)`) but is neither stated nor provable at the Store seam (which deliberately
has no physical-memory model). The spec's own sentence locates exclusivity *in* the
revoke/CDT mechanism and does not specify that the proof must reach down to physical
bytes; the code picked the reading "exclusivity = CDT structural absence of descendants,"
faithful to §6's scoping of the mechanized revocation model to slot/pointer structure.
**Disposition text:** record as an explicit trusted-base assumption — the Store seam
carries no physical-memory model, so "all caps into region X are CDT descendants of X's
untyped" is established by construction, not by an invariant. Belongs in the §4.8-style
proof-boundary statement (9e).

### D-C2 — Cross-untyped non-overlap is unmodeled

**Class D1 · Severity medium · Disposition accepted-documented**
**Spec anchor:** §2.5 (line 103): "Frames are retyped from untyped (… contiguous sizes;
contiguity comes free from retype)" + the §2.2 region-exclusivity condition.
**Code anchor:** kcore/src/untyped.rs:309–321 `carve_place` ensures per-untyped
containment only (`base+watermark <= c.start`, `c.end <= base+size`); cspace.rs:1306
`cspace_wf` has no region/disjointness predicate (grep over kcore for disjoint/overlap
returns nothing).
**Spec says vs code proves:** in-untyped disjointness is proven (watermark monotonicity);
sub-untyped-vs-parent and sibling disjointness follow from parent containment. But
disjointness of the **independent root untypeds** (set up in the boot shell — slot 0, slot
2, the device frame, the aspace pool) is unprovable in the model — `base`/`size` are raw
u64 with no global frame-table. The spec's region model is broader than what is proven.
**Disposition text:** record the cross-root non-overlap obligation as a boot-setup axiom:
the disjointness of the static base/size literals in the boot shell (inside `unsafe`
outside `verus!`) plus the int→ptr seam is trusted; `carve_place` + watermark monotonicity
then propagate disjointness within each root. Add to the §4.8-style boundary note.

### D-C3 — "deleting the cap unmaps it" rests on a trusted uninterpreted seam

**Class D1 · Severity medium · Disposition accepted-documented**
**Spec anchor:** §2.5 (line 105): "Mapping state lives in the frame cap … deleting or
revoking the cap unmaps it. … revoking the parent cap unmaps every sharer everywhere."
**Code anchor:** `Store::aspace_unmap` (cspace.rs ~1080) ensures frame *all*
object/refs/cspace views but says **nothing** about `pt_lookup`/page-table state (the
comment admits the TLBI log is left unconstrained); `delete` (cspace.rs:8963) calls
`aspace_unmap(asp, va, pages)` from the cap's `mapping` field. The real PTE-clearing proof
(`unmap_in`, aspace.rs:2090: `pt_lookup → None`, one TLBI/page) lives over raw page-table
slices and is not connected to the cap model. `ArrayStore::aspace_unmap`
(test_store.rs:164) has a literal empty body and *satisfies* the contract.
**Spec says vs code proves:** the cap-side bookkeeping is faithfully proven (a derived
copy starts unmapped; `delete` invokes unmap with the cap's own coordinates). But because
`aspace_unmap`'s contract frames object state only, proving the `ensures` says nothing
about clearing PTEs. The join — "the cap's recorded mapping is the true PTE location, and
`aspace_unmap` actually clears those PTEs" — lives in the unverified kernel crate
(reconstituting tables via unsafe raw pointers). So "deleting the cap unmaps it" is proven
up to a trusted host-checked boundary, not end-to-end.
**Disposition text:** document that the cap↔PTE correspondence and the actual PTE clearing
are a trusted seam: `aspace_unmap`'s contract frames object state only; the page-table
clearing is proven independently in `unmap_in` and joined in the unverified kernel store.
Note for 9e.

### D-D1 — `recv` does not export cap-installation into the receiver's cspace

**Class D1 · Severity medium · Disposition follow-on**
**Spec anchor:** §3.3 (line 144): "On receive, transferred caps are installed into the
receiver's cspace"; §3.4 (line 151): "lands in the receiver's cspace at receive time …
exactly one owner — sender, queue slot, or receiver."
**Code anchor:** kcore/src/channel.rs:1035–1055 `recv` `ensures` (Ok case): `chan_wf`,
`cspace_wf`, dom equality, count−1, `ring_fifo == old.drop_first()`, other ring unchanged,
`res->Ok_0.0 == msg_len[head]`. The `dests` array appears **nowhere** in the `ensures`;
the install mask `res->Ok_0.1` is unconstrained.
**Spec says vs code proves:** the proven postcondition is dequeue-only — the cap *left* the
queue (`drop_first` + chan_wf's out-of-window-empty clause). Where it *went* is not
exported: a verified caller cannot conclude `final.slot_view()[dests[c]].cap == <arriving
cap>`. The installation IS performed in the body (`slot_move` at channel.rs ~1193, whose
own `ensures` proves it) but is not even a pass-2 loop invariant, let alone a
postcondition. This is asymmetric with `send`, which exports both halves (source emptied
AND cap-in-queue via the `ring_fifo` push). The only `recv` callers are unverified
shell/test code, so no verified consumer currently mis-relies — a contract-export gap, not
a body bug. (Independently confirmed: the Ok-ensures block carries no `dests` clause.)
**Disposition text:** lift the pass-2 installation into `recv`'s `ensures` (∀ installed
`c`: `final.slot_view()[dests[c]].cap == old arriving cap` ∧ ring slot emptied) and
constrain the returned mask, restoring symmetry with `send` and exporting the receive-half
of "exactly one owner."

### D-D2 — `destroy_channel` does not fire peer-closed

**Class D3 · Severity low · Disposition accepted-documented**
**Spec anchor:** §3.3 (line 142): "destroying the whole object at once … fires every
endpoint's binding before reclamation."
**Code anchor:** kcore/src/channel.rs:1532–1576 `destroy_channel` ensures empty every ring
cap + frames; no fire/`EV_PEER_CLOSED`. `EV_PEER_CLOSED` fires in `endpoint_cap_dropped`
(channel.rs:314) from `delete` (cspace.rs:9080).
**Spec says vs code proves:** the spec promises a net behavior, not that `destroy_channel`
is the firing locus. The only production `destroy_channel` caller is `obj_unref`'s Channel
arm (cspace.rs:8469–8488), gated on `obj_refs(o)==0`; via `refcount_sound`, `obj_refs==0 ⇒
slot_refs(o)==0 ⇒ both endpoint caps already deleted`, and each deletion already fired the
surviving peer with bindings intact. So "fires every endpoint's binding before
reclamation" is delivered (and machine-proven through the refcount chain) for the only
spec-admitted whole-object destruction path (untyped revoke = CDT walk over endpoint-cap
descendants) — the firing locus simply differs from the prose.
**Disposition text:** §3.3 should note the firing happens via per-endpoint-cap `delete`
during the revoke walk, before `destroy_channel` reclaims — the behavior is realized, the
locus differs from the prose.

### D-E1 — `wait`/`arm`/`disarm` carry no refcount-soundness obligation

**Class D1 · Severity medium · Disposition follow-on** *(grouped with D-F2: the leaf-op
`refcount_sound` export gap)*
**Spec anchor:** §3.6 (event primitive, directly signalable/armable from userspace);
§2.2/§4.1 resource-ancestry teardown depends on the census being sound so
`revoke`/`destroy_*` run correctly.
**Code anchor:** kcore/src/notification.rs:343–367 (`wait` ensures), timer.rs:253–264
(`arm`), timer.rs:69–106 (`disarm`) — none mention `refcount_sound`/`census_delta_frozen`.
Contrast `signal` (notification.rs:84,89) and `remove_waiter` (519,523), which carry both.
`arm` does not even export its refs delta; `wait` and `disarm` export the exact delta but
not the soundness bridge.
**Spec says vs code proves:** each op perturbs `refs` (`wait` +1 on block, `arm` +1,
`disarm` −1) with a matching census term, yet none proves the `refcount_sound`
preservation its sibling leaf ops carry. The census IS net-zero for each (provable), so
this is a missing obligation, not unsoundness. But the syscall-driven `wait`/`arm` path
has **no verified kcore caller**, so a verified `revoke`/`delete`/`destroy_timer` invoked
after a syscall `wait`/`arm` has its `refcount_sound(old)` precondition undischarged by any
verified entity. (`disarm`'s gap is covered inside kcore: `destroy_timer`/`check_expired`
re-prove soundness.) (Re-verified by hand: `wait`/`disarm` ensure exact refs deltas but
omit the bridge; `arm` omits even the delta.)
**Disposition text:** add the `census_delta_frozen` (and conditional `refcount_sound`)
`ensures` to `wait`/`arm`/`disarm`, matching `signal`/`remove_waiter`, closing the
preservation chain at the syscall boundary.

### D-E2 — `check_expired` excludes N-timers→1-notification

**Class D2 · Severity medium · Disposition follow-on**
**Spec anchor:** §3.6 (line 170): "timer objects bind identically" — a (notification cap,
bit) pair, no uniqueness stated; the event model multiplexes many sources onto bits of one
notification word ("bits identify *groups*," "one channel per session").
**Code anchor:** kcore/src/timer.rs:566 `requires
cspace::timer_notif_injective(old(store).timer_view())` on `check_expired`; def
cspace.rs:2791 (armed timers bind pairwise-distinct notifications). The proof rides
`lemma_signal_ok_after_fire` (timer.rs:493), which uses injectivity to frame each fire.
**Spec says vs code proves:** the sweep is verified only under the precondition that all
armed timers bind distinct notifications — excluding the spec-admitted, IPC-common N→1
shape. The excluded case is **reachable and unhandled**: `arm` (timer.rs:243) neither
requires nor ensures injectivity and binds an arbitrary caller notif with no distinctness
check; `timer_notif_injective` is not part of `timer_wf`, `refcount_sound`, or any global
invariant; and the trusted IRQ shell calls `check_expired` without discharging it. So if
userspace arms two timers on one notification, the IRQ sweep runs with an unverified
precondition.
**Disposition text:** generalize `lemma_signal_ok_after_fire` and `check_expired` to the
shared-notification case (the census phase the code defers at timer.rs:558–562), removing
the injectivity `requires`. Until then, an honest note that the verified expiry sweep
covers only the one-timer-per-notification configuration.

### D-E3 — `signal` does not wake a waiter when the resulting word is zero

**Class D3 · Severity low · Disposition accepted-documented**
**Spec anchor:** §3.6 (line 170): "Signalers OR bits in; a waiter receives the accumulated
word, which clears." Silent on a zero-bit signal with a waiter queued and word still zero.
**Code anchor:** kcore/src/notification.rs:48–50 `signal_wakes := (nv[n].word | bits) != 0
&& wait_head is Some`; wake path gated on it (body `if word == 0 || head.is_none()` takes
the accumulate branch).
**Spec says vs code proves:** a signal leaving the word at 0 is treated as an accumulate
(no-op), not a wake. The case is reachable at the ABI (`notif_signal(slot, bits)` accepts
arbitrary bits) but carries no information; the IPC reactor always signals a nonzero
single-bit mask. The chosen reading is consistent with "a waiter receives the accumulated
word, which clears" (delivering 0 is vacuous) and the poll-once-then-wait discipline.
**Disposition text:** §3.6 should note the chosen behavior: a signal whose post-OR word is
zero does not wake a queued waiter (no event is conveyed).

### D-E4 — Timer expiry is a `>=` tick sweep, not the exact CNTV compare

**Class D4 · Severity low · Disposition accepted-documented**
**Spec anchor:** §1 (line 28) / §2.6 (line 117): "program a deadline that signals a bound
notification"; "programs deadline interrupts."
**Code anchor:** kcore/src/timer.rs:620 `if store.timer_deadline(c) <= now`; module
comment timer.rs:6–7 "Expiry is checked on the periodic tick, so deadline resolution is one
tick at MVP." CNTVCT/CNTV access lives in the unverified kernel crate.
**Spec says vs code proves:** the deadline→signal *coupling* the spec cares about IS proven
(`arm` binds deadline+bits+notif; `check_expired` fires `signal` on `deadline <= now`). The
spec does not quantify timing resolution; a tick-driven sweep is consistent with §5.4's
10 ms tick + the QEMU generic-timer environment. The CNTV-compare timing fidelity is an
honest trusted-base boundary in the kernel crate, not a verified-core defect.
**Disposition text:** §2.6/§3.6 should note that MVP deadline resolution is one scheduler
tick (the CNTV compare and tick programming are unverified kernel-crate concerns).

### D-F1 — Suspend-not-destroy: the suspended-state half is unverified

**Class D1 · Severity medium · Disposition accepted-documented**
**Spec anchor:** §5.3: "A faulting thread is **suspended, not destroyed**"; §5.1:
"suspend-on-fault means no second fault"; §5.3 "under suspend-not-destroy, an inducible
fault is a wedge."
**Code anchor:** kcore/src/thread.rs:161–175 `report_terminal` ensures constrain only
`report` and frame views; **nothing** on `tcb_view()[t].state`. `state = Faulted` is set in
the unverified exception path, `state = Halted` in the unverified syscall path; the
never-rescheduled property lives in the unverified scheduler. No wf invariant links a
terminal Report to a Halted/Faulted state (cspace.rs `state` invariants are all about
`BlockedNotif`).
**Spec says vs code proves:** `report_terminal` proves the report record transitions
Running→terminal at most once (absorbing) and frames the TCB domain (the "not destroyed"
half). It does **not** prove the thread is suspended — the model permits `report=Faulted ∧
state=Runnable`. The load-bearing safety property (no second fault) is delivered
independently by the report-field guard and does hold; what is unverified is the literal
"suspended" state assertion. A buggy shell leaving a faulted thread Runnable would re-fault
and hit the absorbing path (second report dropped), but kcore would not catch the
rescheduling.
**Disposition text:** document that the "suspended (never rescheduled)" half of
suspend-not-destroy is realized in the unverified architectural shell (exception entry,
syscall exit, the scheduler), outside the Verus boundary; only the at-most-one-report half
is in kcore. Note for 9e.

### D-F2 — `bind` drops `refcount_sound` from its `ensures`

**Class D1 · Severity low · Disposition fix-now** *(grouped with D-E1)*
**Spec anchor:** §5.1: binding slots are "CDT-visible like queue slots"; a CDT-visible cap
participates in refcount/CDT bookkeeping, so rebinding must keep accounting sound.
**Code anchor:** kcore/src/thread.rs:232 `bind` `requires cspace::refcount_sound(old(store))`;
ensures block (250–279) has **no** `refcount_sound`/`refs_view` clause. Contrast
`destroy_tcb` (336), which both requires (340) and ensures (380) it.
**Spec says vs code proves:** the displaced-notification −1 refcount delta is **actually
proven inside `delete`** (cspace.rs:8990 ensures `refcount_sound(final)` unconditionally;
obj_census counts the bind-slot reference via `slot_refs`), and `slot_move`/`set_tcb_bind_bits`
ensure `refs_view == old` — so `refcount_sound` holds at `bind`'s exit and is *provable*
there. `bind` simply drops it from its own `ensures`, a contract weaker than the code's own
guarantee and than sibling `destroy_tcb`. (`bind` has no verified kcore caller, so this
breaks no current obligation.)
**Disposition text — fix-now:** add `ensures cspace::refcount_sound(final(store))` to
`thread::bind`; it should discharge directly from `delete`'s postcondition plus the
`refs_view`-preserving `ensures` of `slot_move` and `set_tcb_bind_bits`. Small,
faithfulness-restoring. The self-deprecating doc comment ("the precise ref delta rides the
host test, not the verified contract") is then superseded and should be updated.

### D-F3 — Report-mechanism access control is unverified shell

**Class D1 · Severity medium · Disposition accepted-documented**
**Spec anchor:** §5.1: "configured by the holder of the thread cap"; "a child holds no cap
to its own threads, [so] a child cannot silence or forge its own death notice."
**Code anchor:** kcore/src/thread.rs:224 `bind` takes `t: ObjId, which, notif_src` with
**no** caller identity or rights; the "only the holder" gate is the unverified
`BIND_REPORTS` check in the syscall layer; `read_report`'s `READ_REPORT` gate likewise;
the structural "child holds no own-thread cap" is a spawn convention in the loader and boot
shell (no `verus!` blocks).
**Spec says vs code proves:** the verified `bind` proves only the *mechanical* effect of a
binding edit; the security predicate (who may invoke it, that the dying thread cannot) is
enforced by a rights bit and cap-ownership structure in the unverified kernel. kcore is not
entirely rights-blind (Rights/`BIND_REPORTS`/`READ_REPORT` are in the verified Cap model
and `derive`'s rights-monotonicity is verified), but the *binding of a specific right to
the syscall entry* and the cap-distribution structure are unverified — an inherent
boundary, since kcore's Store-seam functions operate on `ObjId`s and cannot see caller
authority.
**Disposition text:** document that the anti-forgery / anti-suppression guarantee rests on
the unverified `BIND_REPORTS`/`READ_REPORT` rights gates and the spawn-time
cap-distribution convention, not on a kcore `ensures`. Note for 9e.

### D-F4 — `thread_exit`/`read_report` syscall plumbing unverified

**Class D4 · Severity low · Disposition accepted-documented**
**Spec anchor:** §5.1: "thread_exit(status) … recorded by the kernel so a child can neither
lie about nor forget its own death; and read_report(thread cap)"; "Exit status persists in
the TCB until the parent reclaims the thread."
**Code anchor:** no `fn thread_exit`/`fn read_report` in kcore; both are in the unverified
syscall layer (the former sets `state=Halted` and calls verified `report_terminal` with
`Exited(status)`; the latter reads `(*tcb).report`). Persistence rides `destroy_tcb`'s
report-unchanged ensures (thread.rs:398).
**Spec says vs code proves:** the load-bearing properties DO have kcore contracts —
`report_terminal` ensures first-write-wins/absorbing (thread.rs:167–175), `set_tcb_report`
(cspace.rs:777) is the sole `.report` mutator and is called only from `report_terminal`,
`tcb_report` (cspace.rs:773) is a faithful read, `destroy_tcb` (thread.rs:398) ensures the
report survives destruction. What is genuinely unverified is only the thin syscall
dispatch/register-marshalling shell. §6's tiered policy places exactly this outside the
verified core by design; the spec's "recorded by the kernel" framing could read as a
mechanized-syscall claim, but the record-mutation IS verified.
**Disposition text:** §5.1 should note that only the report record-mutation and persistence
are mechanized in kcore; the `thread_exit`/`read_report` syscall dispatch and register
marshalling are best-effort trusted shell per §6's tiering.

### D-G1 — Per-record WAL content-acceptance is uninterpreted

**Class not-a-drift (vs spec; documentation-hygiene residue) · Severity low · Disposition
accepted-documented**
**Spec anchor:** §4.5 (line 256, tool-agnostic): "Replay the WAL from the recorded head to
rebuild per-ref overlay state"; §6 row (a) assigns the "recovered state = committed roots +
replay" invariant to **TLA+**, with the Verus row deferred.
**Code anchor:** cas/src/store.rs:583 `uninterp spec fn content_ok_spec(rec: Seq<u8>) ->
bool`; store.rs:570–578 `#[verifier::external_body] fn wal_content_ok` (body =
`WalOp::decode_record(...).is_some()`, behind blake3 + WalOp decode); `replay_bound`
(651–656) ensures `end_off <= wal.len()` and `count == run_len(...)` (acceptance decided
through the uninterpreted `content_ok_spec`); `apply_to_overlay` (store.rs:1114/1361) is
plain Rust outside the `verus!` blocks.
**Spec says vs code proves:** the proof establishes WHICH records are replayed
(structurally, in-bounds, maximal contiguous run) but proves nothing about WHAT each record
does, nor the row-(a) state equality. This is **not a drift against spec rev2** — the spec
never assigns Verus mechanization of recovery content-coverage; §6 routes the protocol
content to TLA+ (`CommitProtocol`) and totality to §4.5 + fuzz, and the code faithfully
implements that split. The seams are documented honestly in-code. (Independently confirmed:
`content_ok_spec` is `uninterp`; the replay proof is the structural/arithmetic half.)
**Disposition text:** the only residue is overclaim risk in *downstream* artifacts. Any
plan/results doc reading §6 row (a) as "fully mechanized in Verus" must be corrected: the
content half is TLA+, and on the real code it is not mechanized anywhere. Feed to 9e.

### D-G2 — `pick_survivor` equal-generation tie-break

**Class D3 · Severity low · Disposition accepted-documented**
**Spec anchor:** §4.5 (line 256): "take the survivor with the **higher generation**. Its
ref table defines reality. … a checksum-valid superblock proves a complete write, not a
write by this system" (the protocol's `GenerationsDistinct` makes equal valid generations
impossible for honest commits, but §4.5 expands scope to arbitrary/forged bytes).
**Code anchor:** cas/src/store.rs:337–360 `pick_survivor` ensures `(valid_a && valid_b) ==>
((r is SlotA) <==> gen_a >= gen_b)` — a `>=` tie-break, **no `requires`** excluding equal
valid generations.
**Spec says vs code proves:** `pick_survivor` is total over all (gen, valid) and
deterministically picks slot A when two valid slots have equal generations — a state honest
commits cannot produce but a forger can. The chosen reading ("equal ⇒ slot A, never
refuse") is safe: whichever slot wins is checksum-valid and then run through the
Verus-verified total `validate_geometry_fields` (disk.rs:174) + checked birth_gen
arithmetic, so a forged tie cannot panic, over-read, or unbounded-alloc; it merely picks
one of two attacker-supplied complete superblocks, no worse than the single-forged-slot
case §4.5 already concedes. Matches the TLA+ `LiveSlot` `>=` tie-break exactly (collapses
to strict `>` under `GenerationsDistinct`).
**Disposition text:** §4.5 should note that, in the forged-bytes regime, two checksum-valid
slots with equal generation resolve deterministically to slot A (not refused), and that
this is safe because the survivor is fully geometry-validated before use.

### D-H1 — §6 verification table is stale (Kani named, Verus deferred)

**Class D4 · Severity low · Disposition fix-now**
**Spec anchor:** §6 table: "~~Verus~~ — deferred … that did not happen … Kani is now the
mechanized tier for the kernel implementation … Pinned cargo-kani 0.67.0; CI job `kani`."
**Code anchor:** `verus!` macros across kcore (cspace/channel/notification/timer/thread/
untyped/aspace/sysabi/lib), cas, ipc, urt, dma-pool; kcore/Cargo.toml declares the vstd
dependency + `[package.metadata.verus]`; CI runs `cargo verus verify -p kcore` (+ ipc, urt,
dma-pool, cas); **no `#[kani::proof]` harnesses exist anywhere**, and the `kani` job is
retired.
**Spec says vs code proves:** direct factual contradiction. The implementation went the
opposite way from the table: Verus is the mechanized tier, the `kani` job is retired, and
no Kani harnesses remain. The §6 "later Verus port shape" note (host-buildable kcore,
explicit `wf()` predicates, Env/Hal seam, no int→ptr casts) describes **exactly what was
built** — confirming the port happened, contradicting "that did not happen." Code correct;
spec stale.
**Disposition text — fix-now:** rewrite the §6 Proof-carrying-code and Bounded-model-checking
rows: name **Verus** as the mechanized tier for `kcore`/`cas`/`ipc`/`urt`/`dma-pool` (via
`cargo verus verify`), mark the Kani row retired, and replace "that did not happen" with the
record that the Verus port was completed. (This is the §6 reconciliation the deliverable's
9e edits are explicitly meant to carry.)

### D-H2 — §6 row (a) silent on the real cas Verus proofs

**Class D4 · Severity low · Disposition follow-on**
**Spec anchor:** §6 row (a): "Protocol models | TLA+ | storage commit/recovery protocol …
invariant: after any crash, recovered state = committed roots + replay …" (assigns
commit/recovery to TLA+ only; Verus deferred).
**Code anchor:** cas/src/store.rs is Verus-verified: `commit_target` (381),
`frame_at`/`decode_frame` (492/520), `run_len`/`replay_bound` (595/651), `laid_out` (764) +
`lemma_gap_freedom` (843), `commit` (1561 wires the flip). CI still model-checks
`CommitProtocol` in TLA+ AND verifies cas under Verus.
**Spec says vs code proves:** verification of commit/recovery is **split**: TLA+
model-checks the protocol-level crash-recovery invariant, and Verus now proves the
structural layout half (WAL framing, run-length coverage, slot A/B selection) on the real
cas code. §6 mentions neither half explicitly. **No overclaim** — no `ensures` asserts the
full TLA+ replay invariant (`lemma_gap_freedom` honestly labels itself the "shadow"/"half";
content stays behind the `content_ok_spec` seam).
**Disposition text — follow-on:** reconcile §6's tool-assignment with this finding: record
that cas carries Verus structural proofs over WAL framing / run-length coverage / slot
selection (the in-bounds/totality shadow), distinct from the TLA+-owned protocol content
invariant. Coordinate the exact Verus-vs-TLA+ division of labor with the D-G1 disposition.

## Refuted candidates (checked and dismissed)

- **`reset` only checks immediate `first_child`, relies on revoke having run (claimed
  D3).** Refuted: the cited anchor is a *code comment* (untyped.rs:6–8), not spec text. §2.2
  assigns the exclusivity proof to revoke; `revoke`'s `ensures first_child is None` +
  cspace_wf's `parent_has_first_child` is exactly the whole-subtree-empty fact `reset`
  consumes. Faithful.
- **`frame_paddr`/phys-read gate is not a verified gated function (claimed D2).** Refuted:
  §2.5 explicitly designates the DMA/PA boundary as **trusted** ("the driver is trusted —
  there is no third stance"). The gate living in the unverified syscall shell is faithful to
  the spec's trust model; the half the §2.3 discipline requires by construction — PHYS never
  reaching ordinary caps — IS verified (untyped.rs + `bit_vector`). Faithful match.
- **`destroy_channel` whole-object teardown firing only indirect (claimed D3).** Recorded as
  the milder D-D2; the *severity* refutation stands — the firing chain `delete →
  endpoint_cap_dropped → fire EV_PEER_CLOSED` is machine-proven through the refcount chain
  for the only spec-admitted destruction path.
- **"Zero allocation on any event path" has no proof obligation (claimed D1).** Refuted:
  kcore links no `alloc` crate (`#![no_std]`, only `vstd`; no `Box`/`Vec`/global allocator).
  Allocation is a *compile-time impossibility* — a stronger mechanism than any `ensures`.
- **Commit (A/B flip, barriers, deferred-reuse) has no Verus ensures (claimed D1).**
  Refuted: §6 assigns commit atomicity to **TLA+**; `commit()` being plain Rust over verified
  pure decisions (`commit_target`, `advance_head`) is exactly the spec's mandated tiering.
- **Geometry chokepoint split / index-entry bounds outside the verified predicate (claimed
  D2).** Refuted: all four device-offset superblock geometry fields are inside the single
  verified `validate_geometry_fields`; index-entry/free-extent bounds are
  self-verifying-frame contents (hash-authenticated, then checked against the
  already-validated `chunk_tail`) — the spec-sanctioned validated-ground-truth→derived-bound
  chain.
- **Canonical tree shape not Verus-verified (claimed D1).** Refuted: §6 routes tree-shape
  history-independence to the Miri+proptest **baseline** tier and entry-TLV canonicality to
  fuzz; Verus is deferred there. The entry-TLV canonical-form `ensures` is delivered
  faithfully; the spec never promised Verus would prove tree shape.
- **`refcount_sound` never a system invariant / no base case (claimed D1).** Refuted *as a
  spec drift*: §2.2's retype-exclusivity is delivered by the proven **watermark**, not by
  `refcount_sound`; the base case (init's cspace) is established in the trusted boot shell per
  §1/§6. (The genuine residual — leaf-op non-export — is captured as D-E1/D-F2.)
- **`refcount_sound` chain not closed across arm/disarm/wait/send/recv/bind (claimed D1).**
  Refuted *as a spec drift*: the spec frames the guarantee in CDT/slot terms and never names
  a census/refcount preservation invariant; the *teardown* family (the closure "revoke sees
  through queues" needs) IS closed. (The leaf-op postcondition gap is D-E1/D-F2 — proof
  hygiene, not a spec violation.)
- **Monotonicity proven per-field, not against a unified authority lattice (claimed D1).**
  Refuted *as the stated thesis*: §2.3's own table decomposes monotonicity per cap kind, so
  per-axis proofs (rights subset, range containment) are faithful — no unified `cap_le`
  ordering is owed. The genuinely-uncovered axis (priority) is the separate D-B1.
- **revoke under-states postcondition by not exporting `refcount_sound` / CDT-exclusivity
  (claimed D1).** Refuted: `refcount_sound` is internal reference-count integrity, not the
  spec's "no caps reference the region"; the actual exclusivity bridge (`first_child is
  None`) IS exported by revoke and IS exactly reset's precondition. Faithful.

## Faithful matches (the audit's positive findings)

**§2.2 revocation / CDT (A).** Descendant-destruction is structurally complete — the revoke
loop deletes leaves until `first_child is None`, and cdt_wf (parent_has_first_child
contrapositive 1241, empty_slots_detached 1247, acyclic) transitively forces every live
descendant gone, not just direct children (cspace.rs:9314–9350). Queue slots are modeled as
genuine CDT nodes carrying the parent edge (slot_move), so a queued cap is a real CDT
descendant. Revoke terminates on unbounded subtrees (`decreases count_nonempty`, 9327) and
preserves cspace_wf across the walk (9305).

**§2.3 attenuation (B).** Rights-mask attenuation for channel/thread/cspace/notification/
timer/frame caps is monotone, proven as a bitwise subset for **all** masks (∀, not sampled)
— `derive` ensures `dst.rights == src.rights & mask` and the subset relation
(cspace.rs:7213–7217), reached by the production CapCopy syscall. Derivation cannot change
the designated object (`derived_kind` 1158; a Frame copy starts unmapped). Untyped sub-range
derivation is page-aligned (4096) and strictly within the parent range (untyped.rs:313–317).
PHYS provably never flows down an ordinary derivation chain (∀-proven by `bit_vector`). Kill
is not a thread right (THREAD_ALL = READ|WRITE|BIND_REPORTS|READ_REPORT).

**§2.5 memory (C).** Carve arithmetic is total/overflow-safe with in-untyped non-overlap
(untyped.rs:299–445). `map_in` installs a leaf PTE equal to `spec_pte_encode` of the
requested perms with a no-overwrite frame and NEED_MEMORY on exhaustion (aspace.rs). Pool-at-
creation allocates strictly from the donated pool. The frame cap carries its mapping inline;
delete drives unmap with the cap's own coordinates. `unmap_in` clears exactly the requested
range, frames everything else, preserves pt_wf, and emits exactly one TLBI per page in
ascending order (aspace.rs:2090–2117).

**§3.1–§3.4 channels (D).** `send` returns FULL and never drops a message (Err ⇒ store
unchanged, channel.rs:679–682). `ring_fifo` is a faithful FIFO `Seq` (push on send,
drop_first on recv). Send-side cap conservation is fully exported (source emptied AND
cap-in-queue, channel.rs:700–701 + 692–697). The "no free slots ⇒ receive fails, message
stays queued" hard case is implemented with read-only two-pass atomicity (channel.rs:1077–
1118). Null-slot tolerance is genuinely handled without panic. Queue slots are real
CDT-visible slots in the shared arena; `destroy_channel` empties all queued caps.
Single-endpoint peer-closed firing is faithful. MSG_CAPS=4 / MSG_PAYLOAD=256 are structural.

**§3.6 notifications/timers (E).** Notification = word + intrusive FIFO waiter queue, a
faithful deterministic encoding with proven uniqueness (cspace.rs:1460–1492). Signalers OR
bits in (final word == old|bits on accumulate). A waiter receives the whole accumulated
word, which clears to 0 — proven on both wake and consume paths. Genuine FIFO wake order
(push at tail, drop_first at head). Bindings are real (notif cap, bit) pairs; channel fire
and thread report_terminal both call `signal(n, bits)`. `arm` binds deadline+bits+notif and
holds a ref while armed; `check_expired` fires `signal` on expiry while preserving timer_wf.
The teardown family (destroy_notif/destroy_timer) is fully refcount-sound — its preconditions
are discharged from `refcount_sound` at `obj_unref`.

**§5.1/§5.3 threads (F).** At-most-one terminal report with absorbing terminal states —
`report_terminal` proves the report record transitions Running→terminal at most once,
grounded on the report **field** guard (thread.rs:166–180), independent of the unverified
scheduler. The preallocated report is a fixed Tcb field written at most once (no heap).
Binding slots are CDT-visible cap slots that teardown empties; the dying thread's own report
provably survives the fire. `destroy_tcb` proves report-unchanged-on-destroy with
refcount_sound preserved. Faults and exits are one mechanism (single Report enum, single
report_terminal, single read decode).

**§4 storage (G).** Mount geometry validation is total over arbitrary field/device-length
values, all `checked_add`, accepting iff `geometry_ok` (disk.rs:174–211). Superblock decode
is total ∀ bytes behind a magic+checksum gate. `commit_target` writes the non-live/older
slot (store.rs:381). `advance_head` computes the new head as the first non-flushed record.
The WAL replay walk is total, terminating, in-bounds, and accepts the maximal contiguous
seq-run (store.rs:651). `decode_frame` is total and bounds-carrying. Gap-freedom — every
unflushed record's index lies in the replayed span (lemma_gap_freedom store.rs:843). The
directory-entry TLV is canonical (decode accepts only canonical_bytes; encode produces
exactly canonical_bytes — hash-is-identity at the entry layer). The §4.5 "no panic / no
unbounded alloc / no read past end" totality is genuinely *proven* for the decode/replay-
bound chokepoints, with downstream allocations bounded by geometry-validated fields.

**§2.2/§6 cross-cutting (H).** `obj_census` faithfully sums all six reference-bearing slot
kinds the spec demands (cspace.rs:3326–3333). The CDT single-cap delete preserves
refcount_sound unconditionally (cspace.rs:8990). `slot_move` holds `refs_view` literally
unchanged, so in-flight cap relocation preserves per-object slot_refs. Reference-adding ops
(derive, channel bind, endpoint_cap_added) correctly carry the conditional refcount_sound
implication. `signal` exports the requires-free `census_delta_frozen` plus the conditional
implication. The kcore crate exists exactly as §6's deferred-Verus note predicted the "later
port shape" (host-buildable, explicit wf() predicates, Env/Hal seam, no int→ptr casts) — and
is now Verus-verified.

## Inputs to 9e

The accepted-documented and fix-now dispositions feed these concrete spec/CLAUDE.md/code
edits, generalizing the §4.8 phase-8 discipline of stating proof boundaries honestly:

1. **§6 table rewrite (D-H1, D-H2 · fix-now/follow-on):** name Verus as the mechanized tier
   for `kcore`/`cas`/`ipc`/`urt`/`dma-pool`; mark the `kani` row retired; record the Verus
   port as completed (delete "that did not happen"); and add to row (a) that cas carries Verus
   structural proofs over WAL framing / run-length coverage / slot selection (the
   in-bounds/totality *shadow*), distinct from the TLA+-owned protocol content invariant —
   reconciled with the D-G1 wording.

2. **Phase-8-style proof-boundary note in §6 / a new "verified vs trusted" subsection (D-C1,
   D-C2, D-C3, D-F1, D-F3, D-G1):** state the trusted-base seams explicitly — (a)
   physical-region exclusivity is established by construction (only the Frame-creation path
   sets `parent == Some(ut_slot)`), not by a Store invariant, since the Store has no
   physical-memory model; (b) cross-root untyped non-overlap is a boot-setup axiom (static
   base/size literals + int→ptr seam); (c) the cap↔PTE correspondence and actual PTE clearing
   are a trusted join in the kernel store, with `aspace_unmap`'s contract framing object state
   only; (d) the suspended-state half of suspend-not-destroy and the report-mechanism access
   control live in the unverified shell; (e) storage recovery content-coverage is TLA+-owned
   and mechanized nowhere on the real code — any artifact reading §6 row (a) as "fully
   Verus-mechanized" is wrong.

3. **§2.3/§5.4 honest priority note (D-B1):** state that the priority axis of the monotone
   derivation lattice is **not verified** — there is no priority value on the cap, and the
   ceiling is enforced by an unverified kernel-shell guard on the caller thread's live
   run-priority, not a cap-carried MCP. This is the one place where "monotone like every other
   derivation" is currently prose, not proof.

4. **§2.2/§2.5 revocation caveats (D-A1, D-A2, D-A3):** note that (a) the current verified
   `revoke` contract is unreachable from the CapRevoke syscall (always a homed target) and the
   descendant-deletion on the real path is witnessed only by an executable test; (b)
   root-survival across revoke is guaranteed for un-homed donated-untyped targets, with the
   seL4-zombie self-empty of a homed root admissible; (c) "revoke sees through queues" is
   currently inferred from cdt_wf + slot_move's body, not a named Verus obligation. Item (a) is
   the highest-priority follow-on of the whole audit — a contract split would make the
   spec-mandated descendant-deletion reachable from the real call path.

5. **Smaller spec notes:** §2.3 — untyped rights mask is a fixed READ|WRITE strip (D-B2); §3.3
   — whole-object teardown fires via per-cap delete during the revoke walk (D-D2); §3.6 —
   zero-word signals do not wake a queued waiter (D-E3), and MVP timer resolution is one tick
   (D-E4); §4.5 — forged equal-generation slots resolve to slot A, validated before use
   (D-G2); §3.6 — the verified expiry sweep currently covers only one-timer-per-notification
   (D-E2).

6. **Code fix-now (D-F2 · recommended, not yet applied):** add `ensures
   cspace::refcount_sound(final(store))` to `thread::bind` (dischargeable from `delete`'s
   postcondition + the `refs_view`-preserving ensures of `slot_move`/`set_tcb_bind_bits`) and
   update the self-deprecating "rides the host test, not the verified contract" comment. The
   companion `wait`/`arm`/`disarm` exports (D-E1) are follow-on, closing the leaf-op
   refcount-soundness chain at the syscall boundary.

---

*Audit conducted as a fresh-eyes re-derivation against `doc/spec/2_spec_rev2.md` and the
verified source only. Headline findings (D-A1, D-B1) and all three watch-list items were
independently re-verified against the actual `requires`/`ensures` and the kernel call path.
No drift is closed silently; dispositions are recommendations feeding the 9e spec/CLAUDE.md
reconciliation.*
