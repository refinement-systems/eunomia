# Verus findings 43 — Phase 7g: `cas::tlv` canonical-form codec (full exec-level)

Plan: `doc/plans/3_verus-rewrite.md` (§4.7) and
`doc/plans/3_verus-rewrite_phase7-detail.md` (§7g). Prior increment: `62`
(phase 7f — `cas::disk`, which **retired the `kani` CI job entirely**). 7g is the
final §4.7 target: the directory-entry TLV codec `cas::tlv` (spec §4.9).

**Scope deviation from the §7g plan text, per the user.** The detail doc
*recommended deferring* a Verus proof here — `cas::tlv` has no Kani harness to
delete, and the canonical-form oracle is already a cargo-fuzz target
(`cas/fuzz/fuzz_targets/tlv_entry.rs`). The project's direction is to use Verus
extensively — *"verify unless Verus isn't the correct tool"* — with fuzzing
filling the gaps Verus can't reach. So 7g **proves the canonical-form property in
Verus** instead, at the **full exec-level** (the user's explicit choice between a
spec-level theorem and modelling the real `Vec`-building encoder): the literal
`encode(decode(b)) == b` over the real exec codec, not a spec-only relation. The
master plan §4.7 already assigned `cas::tlv` canonical-form to Verus ("the
canonical-form oracle as a theorem"); 7g cashes it.

`cargo verus verify -p cas --no-default-features`: **45 verified, 0 errors** (was
11 in 7f — the +34 is the new TLV core: helpers, lemmas, `decode_raw`,
`encode_raw`). `cargo test -p cas`: green — the canonical oracle
`tests/fuzz_corpus.rs::tlv_entry` and the prolly/tree/store proptests all run the
**rewritten** codec (the `verus!{}` block erases). `cargo test --workspace
--exclude kernel`: green. `cd kernel && cargo build` (+ `--release`) and a clean
`user/storaged` aarch64 rebuild: green — vstd (now with the **`alloc`** feature,
see §3) erases into storaged's no_std cross-build. No Kani change: Kani was fully
retired in 7f.

---

## 1. The two theorems, and the property they realize

`cas::tlv::{decode,encode}` are thin wrappers over `prolly::{decode_entry,
encode_entry}`; an `Entry` is variable-length:

```
[name_len u8][name][kind u8][size u64 LE][mtime u64 LE]
[content_tag u8][content…][opt_len u16 LE][opt TLV…]
```

The format is **canonical** (absent fields = zero bytes, zero flags spelled as
absence, sorted optional tags, no slack), so `encode(decode(b)) == b` for every
accepted `b` — the invariant that makes "same contents ⇒ same hash" hold. The
verified core proves, ∀ bytes:

1. **Decode totality** — `decode_raw(buf)` never panics for any `buf` (verifying
   its body *is* the no-panic theorem: every read bounds-checked, every cast
   non-overflowing, the optional-TLV `while` loop given a `decreases`).
2. **Canonical-form round-trip** — `decode_raw(buf) == Ok((e, k)) ==>
   canonical_bytes(e) == buf[..k]` (the decoder accepts **only** canonical
   encodings — the hard direction), and `encode_raw(e)` appends exactly
   `canonical_bytes(e)`. Composed: `encode_raw(decode_raw(b)) == b[..k]`, and with
   `tlv::decode`'s whole-buffer check (`k == b.len()`), `encode(decode(b)) == b`.

`spec fn canonical_bytes(RawEntry) -> Seq<u8>` is the ghost serializer the two
theorems pivot on; `encode_raw` is proven equal to it, `decode_raw` proven to
invert it.

---

## 2. The `Hash`-free core (the 7f `RawSuperblock` discipline, extended)

The round-trip couples encode and decode, so both live in the verified core; an
`Entry` carries `Hash` and `Vec<u8>`. To keep `Hash` out of the proof surface
(7f's rule — no `external_type_specification`), the core carries a **`Hash`-free
image**:

```
pub struct RawEntry  { name: Vec<u8>, kind: u8, flags: u32, size: u64, mtime: u64, content: RawContent }
pub enum   RawContent { Inline(Vec<u8>), ChunkList([u8; 32]), DirRoot([u8; 32]) }
```

The 32 hash bytes are `[u8; 32]`, so they **round-trip inside the proof** (no Hash
axiom needed at all — unlike 7f's blake3 `external_body`, which 7g does not have).
`encode_entry`/`decode_entry` become thin plain-Rust `Entry ↔ RawEntry`
converters; the only `Hash` touch is `Hash::from_bytes(a)` / `*h.as_bytes()`,
which is a transparent newtype wrap — trivially total, the fuzz oracle covers the
composed path.

**The validation split.** Entry-level well-formedness (`validate_entry`:
`INLINE_MAX`, size/kind/content agreement, name rules) **stays plain Rust** and is
re-run by `decode_entry` after the verified parse. It only *shrinks* the accept
set, so it does not bear on the round-trip (every accepted input is still
canonical). The verified `decode_raw` carries exactly the **structural + optional-
section** rules (bad kind/content tag, opt-len cap, strictly-ascending tags, the
`len == 4` flags record, the zero-flags-is-absence rule) — the rules that *are*
load-bearing for canonical form. This split is what kept the proof tractable: no
`validate_entry` re-implementation in Verus.

---

## 3. `vstd` `alloc`, and the exec `Vec` encoder

Full exec-level means modelling the real `Vec`-building encoder, so `cas`'s vstd
dependency gains the **`alloc`** feature (`default-features = false, features =
["alloc"]`) for the `Vec` specs — the **chief phase-7 risk** (vstd in the
userspace cross-build, now with alloc). Cleared the 7a way: a clean aarch64
`user/storaged` build before any deep proof rested on it — vstd-with-alloc erases,
storaged links (storaged already pulled vstd transitively via `ipc`/`urt` since
7a, so this adds no new binary). `cargo verus verify -p cas
--no-default-features` is unchanged (the feature lives in `Cargo.toml`; `cas`
carries `extern crate alloc` unconditionally).

vstd's `Vec::extend_from_slice` spec uses a `cloned` predicate (awkward for clean
`u8` `Seq` equality), so the byte appends go through verified push-loop helpers
(`extend_bytes`/`push_arr32`/`push_u{16,32,64}_le`, each `ensures out@ == old@ +
…`) — the 7e precedent of replacing an unspecced/awkward std combinator with a
verified helper.

---

## 4. The opt-section loop — the one intricate invariant

The optional section is a `while` loop reading TLV records; canonical form admits
**at most one** record (the flags tag). The loop invariant captures exactly that:

```
last_tag == -1 || last_tag == 1,
last_tag == -1 ==> flags == 0 && p == opt_start,                       // no record yet
last_tag == 1  ==> flags != 0 && p == opt_start + 7                    // the one canonical record
                && buf[opt_start..p] == [1] ++ u16_le(4) ++ u32_le(flags),
```

Only tag `1` is known (any other → reject), and tags must strictly ascend, so a
second iteration is impossible. At loop exit (`p == opt_end`) a two-case split
gives `opt_bytes(flags) == buf[opt_len_field .. opt_end]` — the section is exactly
its canonical image. The whole entry then assembles left-to-right by a single
`lemma_cat` (subrange concatenation) chain.

---

## 5. Gotchas (for the next port)

- **External enums can't be constructed inside `verus!{}`.** `decode_raw` cannot
  return `FormatError` (its `MissingNode(Hash)` variant would drag in `Hash`, and
  `#[verifier::external_type_specification]` makes the type *opaque* — "constructor
  for an opaque datatype" is disallowed). Use an in-block error enum (`TlvErr`,
  `{Truncated, BadEntry(&'static str)}`) mapped 1:1 to `FormatError` by the
  converter — exact messages preserved (the 7f `SbError` pattern).
- **New mut-ref postconditions need `final(out)@`, not `out@`** (the 7b form).
- **`pub open spec` bodies may name only public items** (7d) — `opt_bytes` spells
  the flags tag as the literal `1u8`, not the private `OPT_TAG_FLAGS`.
- **`content_bytes` includes the content-tag byte** — an off-by-one fence-post:
  the content segment starts at `p_ctag`, not `p_ctag + 1`. (Caught only because
  the per-branch `content_bytes == buf[…]` assert failed; worth stating the byte
  ranges explicitly.)
- **usize overflow in spec invariants.** `opt_start + 7` inside an `invariant`
  is overflow-checked — restate as `p as int == g_opt_start + 7` (int). And a
  fresh `let end = off + n` needs the bound materialized against the exec
  `buf.len()` (a usize ⇒ `≤ usize::MAX`) via `assert(off + n <= buf.len())`; the
  spec `off + n <= buf@.len()` alone does not discharge it.
- **Ghost-only `let`s warn under plain `cargo build`** (the macro keeps the exec
  binding, its only use erases) — use `let ghost`, or fold the value into an
  `assert` rather than a named binding.

---

## 6. Fuzzing fills the gap

`cas/fuzz/fuzz_targets/tlv_entry.rs` (`encode(decode(data)) == data`) is **kept**
as differential/regression coverage over the full `Entry` path — including the
`Hash` newtype wrap the verified core abstracts and the whole-buffer Reader check
— and is replayed by `cargo test -p cas --test fuzz_corpus`. Verus and fuzz are
complementary here (master plan §2): the unbounded theorem on the core, the
corpus/differential oracle on the composed whole. No fuzz target is deleted.

With 7g, every §4.7 host chokepoint is on Verus and Kani is fully retired. The
remaining Verus-rewrite work is phase 8 (the §4.8 commit-recovery core) and phase
9 (the spec §6 / `0_kani-rewrite.md` doc closeout).
