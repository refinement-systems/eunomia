# Verus findings 16 — Phase 5a: sysabi `decode` + `decode_prio` + `ObjType::from_u64`

Plan: `doc/plans/3_verus-rewrite.md` (§4.6 + §7 step 5) and its decomposition
`doc/plans/3_verus-rewrite_phase5-detail.md` (§5a). Prior increments: `21`…`25`
(phase 2 — the cspace/CDT core), `26`…`30` (phase 3 — untyped remainder + channel),
`31`…`35` (phase 4 — notification/thread/timer). This is the **first** sub-phase of
phase 5, the slice-free **confidence-builder**: the §4.6 syscall decoder is pure data —
no `Store`, no slices, no recursion — so it re-banks the Verus workflow on a familiar
surface before the genuinely new page-table partial-map model arrives in 5c–5e (detail
§1.1). It is the §5a analog of phase 3's 3a opener.

**Outcome.** `cargo verus verify -p kcore`: **127 verified, 0 errors** (was 120 after 4e;
`+7`). The `+7` spans the new `verus!{}` items in this change: the `spec_from_u64` ghost
model, the proven exec bodies `from_u64`/`decode_prio`/`decode`, the
`lemma_from_u64_roundtrip` round-trip proof, and the `Sys`/`SysError` datatypes now inside
the macro. `cargo test -p kcore`: **43 passed** (unchanged — 5a adds **no** new host
tests; the pre-existing `sysabi::tests` differential checks — `known_calls_decode`,
`validation_rejects`, `prio_is_masked_then_bounded` — already cover the surface and stay
green). The aarch64 `kernel` cross-build is unchanged (ghost erasure; confirmed `cd kernel
&& cargo build`), and the `kernel::thread` re-export of `NUM_PRIOS` is intact because the
const erases to a plain `pub const`.

**5a adds no `external_body`.** `decode`, `decode_prio`, and `from_u64` are **fully
proven** — nothing is assumed, so no `test_store` contract check is needed for them. This
continues phase 5's property (detail §0): aspace + sysabi are the first modules since
phase 2 to add zero trusted residue (3e left `destroy_channel`/`signal`, 4e left
`destroy_tcb`).

**The round-trip enum-cast was not the risk the plan flagged it might be.** Detail §5a
named `from_u64`'s `from_u64(ty as u64) == Some(ty)` round-trip as the one "attempt full,
fall back" candidate (the doc-35 `check_expired` discipline), uncertain whether Verus
would reason about `ty as u64` on a field-less enum. It does: `lemma_from_u64_roundtrip`
closed with a trivial eight-arm `match ty { … }` (each arm empty), the SMT solver
evaluating `ty as u64` to the declaration-order discriminant `0..8` directly. No fallback
needed; the encode/decode pairing is a theorem.

---

## 1. What closed

- **`ObjType::from_u64` ported into `verus!{}`, mirroring `spec_align`/`align`**
  (`untyped.rs`). It previously sat in a plain-Rust `impl ObjType` wedged between two
  `verus!{}` blocks (detail §0's flagged cross-block move). The port adds:
  - `pub open spec fn spec_from_u64(v: u64) -> Option<ObjType>` — the `match v { 0 =>
    Some(CSpace), …, 7 => Some(Untyped), _ => None }` ghost mirror; `Some` for exactly the
    eight valid discriminants `0..8`.
  - `#[verifier::when_used_as_spec(spec_from_u64)] pub fn from_u64(v: u64) -> (r:
    Option<ObjType>) ensures r == spec_from_u64(v)` — the §4.6 "`from_u64` total" obligation
    as a theorem, and the `None`-iff-`v >= 8` characterization `decode` consumes.
  - `pub proof fn lemma_from_u64_roundtrip` (the round-trip, above).

  The move is contained: `carve`/`retype_check`/`retype_install`/`reset` consume the
  `ObjType` *type*, not `from_u64`, so they are unchanged and stay green.

- **`sysabi::decode` + `decode_prio` ported into `verus!{}`** (`sysabi.rs`), with
  `NUM_PRIOS`, `Sys`, and `SysError` moved inside the macro so the contracts can name them
  (the `channel::MSG_PAYLOAD`-inside-`verus!{}` idiom — a const must be in a `verus!{}`
  block to be spec-visible). `decode` is proven **total** — `Ok`/`Err` for any `(u64,
  [u64;6])`, never a panic/overflow/UB — the spec §3.7 "unknown `nr` is an error, never a
  crash" as a theorem rather than a review convention. The §4.6 shape-validation rides as
  `ensures`:
  - `nr >= 24 ==> Err(UnknownCall)` (all of `0..=23` are defined arms, so unknown is
    exactly `nr >= 24`);
  - `nr == 3 && a@[1] >= 8 ==> Err(BadObjType)` (discharged by `from_u64`'s contract);
  - **`Ok(ChanSend { len, .. }) ==> len <= MSG_PAYLOAD`** — the load-bearing one: the cap
    that precedes `channel::send`'s `as u16` truncation, so send's existing `data.len() <=
    MSG_PAYLOAD` precondition (`channel.rs`) is discharged **at the decode source**;
  - `Ok(ChanBind { event, .. }) ==> event < 3`, `Ok(ThreadBind { which, .. }) ==> which <
    2`, and `Ok(ThreadStart{,As} { prio, .. }) ==> (prio as usize) < NUM_PRIOS`.
  - `decode_prio` carries `Ok(p) ==> (p as usize) < NUM_PRIOS`.

---

## 2. Verus mechanics worth keeping

- **Consts gate on `verus!{}` membership, not visibility.** `NUM_PRIOS` was already `pub`
  and already used by `decode_prio`'s range check; what made it invisible to the *contract*
  was sitting outside any `verus!{}` block. Moving it inside (the `MSG_PAYLOAD` precedent,
  `channel.rs:37`) is the whole fix. The erased output is a byte-identical `pub const`, so
  `kernel::thread`'s `pub use kcore::sysabi::NUM_PRIOS` and the aarch64 build are
  untouched.

- **`[u64; 6]` is the first array exec-indexing in the verified core, and it just works.**
  Every prior kcore proof reasoned over `Map`/`Seq` views; `channel.rs`'s `[T; N]` params
  were read only in specs (`caps@[c]`). `decode`'s body exec-indexes `a[0]..a[5]` with
  literal indices (statically in-bounds, discharged automatically); the contracts name the
  spec view `a@[1]`. No `vstd::array` ceremony was needed — a useful data point for 5b/5c,
  where the slice reasoning is the flagged new surface (detail §3), though arrays-by-value
  are the easy end of it.

- **`?`/`ok_or` avoided in the verified fragment by design.** No existing kcore `verus!{}`
  code uses the `?` operator or `Option::ok_or`, so to keep the first run clean `decode`'s
  Retype/ThreadStart/ThreadStartAs arms were rewritten from `…ok_or(BadObjType)?` /
  `decode_prio(..)?` to explicit `match`/early-return. The erased control flow is
  identical; the proof of the BadObjType / prio `ensures` becomes direct (the `None`/`Err`
  branch is syntactically present). A small, deliberate style trade for proof robustness.

- **The narrowing casts are proven by the guards, not by bit-vector gymnastics.** The
  `a[1] as usize` (event/which) and the `(raw & 0xFF) as u8` (prio) casts needed no
  `assert … by (bit_vector)`: `as u8` is a total truncating cast (no obligation), and the
  preceding guards (`a[1] <= 2`, `a[1] <= 1`, `prio as usize < NUM_PRIOS`) bound the value
  small enough that the u64→usize cast is its own value, so the `< 3`/`< 2`/`< NUM_PRIOS`
  postconditions fall out directly. The §4.6 "length/event/which/prio bounded before use"
  is thus a checked fact with no manual proof scaffolding.

---

## 3. What 5a does **not** touch (carried forward)

Per detail §1.5 / §4, 5a is the aspace/sysabi **opener** and adds no `external_body` and
no cross-object work. Still ahead in phase 5:

- **5b** — `pte_encode`/`pte_output_pa`/`va_range_ok` (the §2.5 PTE isolation theorem,
  device-never-executable / the AS-1 fix; the user-L1-never-touches-kernel corollary).
- **5c** — `range_mapped_in` + the new `pt_lookup`/`pt_wf` page-table partial-map model
  (the one genuinely new proof model and the chief design risk; Verus slice reasoning lands
  here).
- **5d** — `map_in` (the two-pass walk-alloc; the tree-shape no-aliasing frame lemma).
- **5e** — `unmap_in` + the TLBI/barrier effect-ordering ghost log + the phase-5 closeout
  (the `CLAUDE.md` `### Verus` / §6-tier-table update covering 5a–5e at once; the
  already-discharged §7-step-5 clauses; the reaffirmed cross-object-teardown phase).

The cross-object teardown and the full `refcount_sound` census remain the recommended
dedicated phase **after** phase 5 (now unblocked once aspace's walker is ported).
