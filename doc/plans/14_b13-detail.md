# Plan — Part B13 detail: prolly-tree canonical-form verification (lift the directory **node decoder** into the verified, total-∀-bytes surface like the other on-disk decoders — extending the single-entry TLV core to the whole `[level][count][items…]` node and its leaf canonical-round-trip; extract `build_level`'s node-cutting into a verified **partition core** that proves conservation + boundary-discipline over an *opaque* split predicate, the Hash-free half of "tree shape is a function of the contents"; and raise the hash-*dependent* headline canonical-form property — same logical contents ⇒ same root regardless of edit order — from light sampling to a **verification-grade proptest + fuzz** sweep across multi-level shapes × edit orders × churn with the decode-then-reencode oracle. The mechanizable kernel goes to Verus; the irreducibly BLAKE3-dependent tree *shape* stays test-routed, exactly as rev1§6 routes "the chunker and prolly tree especially" to the baseline tier and as B6 test-routes GC mark-set sufficiency — and the ledger says which bar each piece meets)

Detailed, separately-implementable decomposition of **Phase B13** from
`doc/plans/0_address_audit_rev0.md`. B13 is **Wave-4** work. It is **self-contained**: it
depends on nothing else in Part B and nothing depends on it. It is *adjacent to but distinct
from* **B12E** (neighborhood-only re-chunk), which already landed and *uses* the canonical form
as its correctness oracle (neighborhood-re-chunk root hash == whole-file-re-chunk root hash);
B13 *proves and stress-tests the canonical form itself*, the property B12E's oracle assumes.
The two are complementary and named so in the B12 plan (`13_b12-detail.md`, lines 682–684).

It closes the one remaining *baseline-tier* gap the audit found in the storage layer: the
prolly-tree shape / canonical-form is the load-bearing rev1§4.1 property the whole store rests
on, yet today only the *single-entry* TLV codec under it is verified — the node-building and
node-decoding code (`is_boundary`, `build_level`, `Dir::save`, `parse_node`, `load_node`) is
plain Rust guarded by light-sampling proptests that never even build a multi-level tree.

**Closes (from the parent plan).** Verbatim from `doc/results/0_audit_rev0.md` §4.2:

- **Prolly-tree shape / canonical-form unverified [medium]** (audit §4.2). The audit's table
  routes the prolly tree to the baseline tier but flags that the *headline* property is only
  sampled: "`cas/src/prolly.rs` (`is_boundary`, `build_level`, `Dir::save`, `load_node`) — today
  plain Rust; only the single-entry TLV codec is verified." The parent-plan B13 work line
  (`0_address_audit_rev0.md` lines 596–600): "prove (Verus) or, if a full proof is out of reach,
  substantially strengthen the proptest, the central rev1§4.1 property: the same logical contents
  produce the same tree regardless of edit order. The split rule (`is_boundary`) and level
  construction are the load-bearing pieces; the headline property deserves more than sampling."
- Parent-plan B13 **acceptance** (lines 601–603): "canonical-form property proven, or a documented
  strong proptest (many shapes × edit orders) with the decode-then-reencode oracle, both green
  under Miri."

The parent plan's "**prove (Verus) *or* … substantially strengthen the proptest**" is the open
decision B13 resolves. B13 reads the "or" as **"each where it is proportionate"**: the
mechanizable, Hash-free kernel goes to Verus (the node decoder, the partition machinery); the
irreducibly BLAKE3-dependent *concrete tree shape* stays at the baseline tier — but a
verification-grade one — because the spec routes it there (rev1§6: "Baseline | Miri + proptest |
everything, **the chunker and prolly tree especially**") and because mechanizing "which entry
lands in which node" would drag interpreted BLAKE3 into the proof, the same wall B6 hit with GC
reachability (Design decision 1).

---

## Spec target — Part A is blessed; B13 makes no spec edits

Every citation below is `rev1§` against the already-blessed text; B13 changes no spec. The
load-bearing claims B13 conforms to:

- **rev1§4.1 — Structure (the headline).** "Node split boundaries are a function of the hash at
  the boundary key, so tree shape is **history-independent (canonical)**: the same logical
  contents always produce the same tree, regardless of edit order. Canonical form is what makes
  structural sharing, dedup, and diffing work across histories, and it is what makes this layer
  tractable to specify formally." This is the property B13 raises above sampling.
- **rev1§4.9 — Tree schema / entry encoding.** "Entry encoding: deterministic TLV … **exactly
  one encoding per logical entry**, so canonical form survives extension and new tags never
  perturb old entries' hashes." (Already verified at the *entry* grain — `decode_raw`/`encode_raw`;
  B13A lifts it to the *node* grain.) "Identity is bytewise and ordering is memcmp … any equality
  coarser than byte equality would make the stored bytes depend on insertion history and break
  canonical form."
- **rev1§6 — verification tiering (the routing that decides B13's bars).** Two rows bear on B13,
  and they are deliberately *different tiers*:
  - **Baseline | Miri + proptest | "everything, the chunker and prolly tree especially."** The
    spec routes the prolly tree *as a whole* to the baseline tier. So the hash-dependent
    canonical-form property is, by the spec's own routing, a **proptest + Miri** obligation — and
    B13's job there is to make that proptest verification-grade, not to invent a Verus tier the
    spec never asked for.
  - **Proof-carrying code | Verus | "the host-side chokepoints (… the CAS layer)."** The CAS
    Verus surface is the *decoders and recovery cores* (rev1§3.7/§6.1(e)) — and a directory node
    is an on-disk decoder. So lifting `parse_node` into the verified, total-∀-bytes surface
    (B13A) is squarely the Verus tier the spec *does* ask for, the same routing that already put
    the single-entry TLV codec and the superblock/WAL decoders there.
  - rev1§6 baseline paragraph names B13's oracle exactly: "Round-trip and canonical-form
    properties are the natural proptest targets: the same contents produce the same tree,
    regardless of edit order. The **canonical-form oracle for decoders is decode-then-re-encode
    reproducing the input bytes**, since accepting a non-canonical encoding would silently break
    hash-is-identity."
- **rev1§6.1(e) discipline (the honesty rule B13D obeys).** "a property routed to trust is not
  mistaken for a mechanized one." B13's ledger entry must say, in the same spirit as B6's
  GC-sufficiency note, exactly which half is *mechanized* (node decode; the partition core, if it
  lands) and which half is *test-routed* (the BLAKE3-dependent concrete shape), and why the latter
  cannot be Verus without dragging interpreted hashing in.

---

## What is actually true today — and why the headline property is *almost free* but its load-bearing core is not

The crisp decomposition that shapes the whole phase. The rev1§4.1 claim — *same logical contents
⇒ same root, regardless of edit order* — factors into three layers of decreasing triviality:

1. **Edit-order independence is structural and nearly free.** `Dir` stores entries in a
   `BTreeMap<Vec<u8>, Entry>` keyed by name (`prolly.rs:335`). After *any* sequence of
   `upsert`/`remove`, the map equals the same map for the same final logical set, regardless of
   order — `upsert` is insert-overwrite, `remove` is delete, and the BTreeMap iterates in memcmp
   key order. So `d1.entries == d2.entries` whenever the two edit histories end at the same
   contents. The module doc already states the enabler (`prolly.rs:20-25`): "**Incremental
   node-level surgery is deliberately absent: `Dir::save` rebuilds one directory's node tree from
   its full entry list.**" Full rebuild from a canonicalized map ⇒ the "regardless of edit order"
   clause reduces to *BTreeMap canonicalization*, which is by construction.
2. **`save` well-definedness is a function fact, but its *value* is hash-dependent.** `Dir::save`
   (`prolly.rs:370-399`) is a pure deterministic function of `self.entries` — no clock, no
   randomness, no order dependence beyond the sorted iteration. So *equal maps ⇒ equal bytes ⇒
   equal root* holds for **any** hash function. The fact that it is a function is provable; the
   *shape* of the tree it produces — which entry lands in which node — is determined by
   `is_boundary`, hence by **BLAKE3**, which is interpreted and out of SMT scope (verus.md:117,
   rev1§6).
3. **The genuinely load-bearing core the audit names** ("the split rule and level construction
   are the load-bearing pieces") is the *machinery around the hash*: that `build_level`
   (`prolly.rs:304-328`) **partitions** its input losslessly and in order (no entry dropped,
   duplicated, or reordered), that the partition points are determined by the item sequence given
   the split predicate, and that the node decoder accepts *only* the canonical encoding. These are
   first-order, Hash-free, and **provable** — they hold for *any* boundary predicate.

So the honest verification split is forced by where BLAKE3 sits:

- **Mechanizable (Verus):** the node **decoder** (B13A — totality + leaf canonical round-trip,
  Hash-free, composes on the existing `decode_raw`); and the **partition machinery** (B13B —
  conservation + boundary discipline over an *opaque* split predicate, needing only that
  `is_boundary` is a deterministic total function).
- **Irreducibly test-routed (verification-grade proptest + fuzz):** the **concrete tree shape** —
  which entries cluster into which node, hence the actual root-hash equality across edit orders.
  This *is* the headline rev1§4.1 property, and it is hash-dependent: proving it in Verus would
  require an injective model of BLAKE3 over arbitrary item bytes, which verus.md (117) and rev1§6
  put out of scope and which B6 already declined for the structurally identical GC-reachability
  case. The spec routes it to the baseline tier; B13C makes that routing's test verification-grade.

This split is the spine of Design decisions 1–3. The plan's deliverable is **layered**: a clean
must-do (B13A node decoder + B13C strengthened sweep), a recorded stretch (B13B partition core),
and an honest ledger that does not over-claim (B13D).

---

## Primary files (current line numbers — parent-plan citations predate code drift)

- `cas/src/prolly.rs` — the whole prolly tree:
  - **Format constants** (`:45-47`): `SPLIT_BITS = 5` (average fanout 2^5 = **32**),
    `SPLIT_MASK`, `MAX_NODE_ENTRIES = 128` (the forced-boundary cap). These bound the shapes the
    proptest must reach (B13C): with ≤ 64 entries the tree is 1–2 levels and the 128-cap **never
    fires** — the current sampling's central blind spot.
  - `is_boundary` (`:291-294`): `u64::from_le_bytes(BLAKE3(item)[..8]) & SPLIT_MASK == 0` — the
    split rule. A **pure per-item function** (depends only on the item's bytes, not its neighbors
    or position), which is exactly what makes the boundary set a function of the item sequence and
    gives the "an edit perturbs only the node holding the edited entry plus the spine" locality.
    Its body is BLAKE3 — interpreted, out of SMT scope. In B13B it becomes the one *trusted-total*
    seam the partition core is proven *around*.
  - `build_level` (`:304-328`): cuts a level's items into nodes at `is_boundary(bytes) ||
    count == MAX_NODE_ENTRIES || i+1 == items.len()`, encodes `[level][count u32][items…]`, stores
    each, returns `(first_key, hash)` per node. The **partition machinery** B13B extracts and
    verifies; the I/O (`store.put`) stays exec.
  - `Dir::save` (`:370-399`): empty-dir special case (`:371-373`), then `build_level` at level 0
    and the **`while nodes.len() > 1`** spine loop (`:386-397`) climbing levels until one root
    remains. The loop's backstop is `level.checked_add(1).expect("tree deeper than 255 levels")`
    (`:387`) — **a panic path**: the loop has *no hard termination bound*, relying on hash
    boundaries thinning each level (avg fanout 32); an adversarial/degenerate hash that boundaries
    the first item at every level would not provably terminate. This is the concrete reason
    full-`save` Verus is disproportionate (Design decision 2) and a thing B13C must probe (deep
    shapes) and B13D must disclose.
  - `Dir::load` (`:402-419`) + `load_node` (`:461-508`): the recursive node **decoder**. Today
    fuzzed (`tree_node`, `mount_recovery`) and proptested (`roundtrip`, `decoder_rejects_garbage`)
    but **not** in the verified surface, unlike every sibling on-disk decoder. `load_node` carries
    a real structural metric — it recurses with `expected_level = Some(level-1)` and rejects level
    mismatch, so depth ≤ level ≤ 255 — and enforces the separator-key discipline ("the separator
    key must be the first key under the child," `:497-501`), the internal-node half of canonical
    form. B13A's target.
  - `parse_node` (`:433-459`): the *shallow* one-node decoder the GC mark walk runs below the
    fetch-time hash check (so it must be total on hostile bytes). Already a fuzz target
    (`tree_node`); B13A lifts it (and the leaf round-trip) into Verus.
  - **The verified TLV core** (`verus!{}` block `:563-1138`): `decode_raw`/`encode_raw` with the
    `canonical_bytes` spec, the `*_le` byte readers/writers, `lemma_cat`, `fits`. Proven:
    `encode_raw` produces exactly `canonical_bytes`, and `decode_raw` is **total ∀ bytes** and on
    `Ok` consumes exactly `canonical_bytes` (the decode-accepts-only-canonical direction). This is
    the **pattern and the host** B13A extends from one entry to a whole node — *no new block,
    same idioms* (explicit byte indexing, accept-iff specs, `lemma_cat` concatenation).
  - The `tests` module (`:1142-1432`):
    - `canonical_form` proptest (`:1342-1385`): `arb_entries(64)` × shuffle(order_a/b) × churn
      `arb_entries(16)` removed. The headline guard — but **64 entries never reaches a second
      level or the 128-cap**, and 256 native / 4 Miri cases lightly sample. B13C's main rewrite
      target.
    - `roundtrip` (`:1389-1400`): `save → load == identity` and re-save reproduces the root.
    - `structural_sharing_on_small_edit` (`:1224-1243`): one-entry edit over 1000 entries rewrites
      `≤ 8` nodes — the **locality** property, today a single fixed case; B13C promotes it to a
      proptest over many shapes/edit sites.
    - `decoder_rejects_garbage` (`:1425-1430`): `Dir::load` over arbitrary bytes never panics.
    - `arb_entry`/`arb_entries` (`:1281-1332`): the strategies B13C widens (entry counts; name and
      content diversity to spread `is_boundary` outcomes).
- `cas/src/tlv.rs` — the standalone single-entry `encode`/`decode` (`cas::tlv`) the `tlv_entry`
  fuzz oracle drives; thin wrappers over `prolly::{encode_entry, decode_entry}` (the verified
  core). B13A's node oracle is the node-grain analogue.
- `cas/fuzz/fuzz_targets/tree_node.rs` — already decodes `parse_node` and re-encodes **leaf**
  entries for the canonical oracle; internal nodes get totality-only (no lossless single-node
  internal re-encoder because `parse_node` drops separator keys into child hashes). B13C extends
  the oracle to **whole multi-level trees** (`Dir::save`→`Dir::load`→`Dir::save` root stability),
  the lossless level the single-node internal oracle can't reach.
- `cas/fuzz/fuzz_targets/tlv_entry.rs` — the single-entry canonical oracle (unchanged; the grain
  B13A verifies).
- `cas/fuzz/fuzz_targets/chunker.rs` — the FastCDC chunker already has a *strong* fuzz oracle
  (determinism, chunks-concatenate-to-input, bounds, streaming==one-shot) plus proptest. The
  "chunker" half of rev1§6's "chunker and prolly tree especially" is **already covered**; B13 is
  prolly-scoped (Design decision 4) and adds only a small symmetry proptest, not a rebuild.
- `doc/guidelines/verus_trusted-base.md` — the ledger. B13A **raises** the cas gate (65 → 65+N,
  node decoder mechanized). B13B, *if it lands*, adds **one** trusted seam — `is_boundary` as a
  3rd CAS interpreted-hash `external_body` (BLAKE3), tally 13 → 14 — and raises the gate further.
  B13D records the mechanized/test-routed split (the GC-sufficiency-note style at ledger `:55-63`)
  and updates the Baselines row.
- `doc/spec/spec_rev1.md` — **no change** (Part A blessed; B13 conforms code to it).
- `CLAUDE.md` — no change (the cas Miri sweep already names `cas`; B13C's proptests ride it).

---

## Verification tier & baseline (applies to all sub-phases)

The prolly tree is the one component the spec names in **two** tiers at once (rev1§6: Verus for
its decoders as CAS chokepoints; Baseline for its canonical-form property "especially"). B13
honors both. Five honesty notes up front:

- **B13 is *format-stable* — no `SB_VERSION` bump, no corpus regen.** It changes **no on-disk
  bytes**: the node/entry encodings, the split constants (`SPLIT_BITS`/`MAX_NODE_ENTRIES`), and
  the hashes are all untouched. B13A lifts the *existing* decoder into Verus (the bytes it accepts
  are unchanged — verifying *is* proving the current accept-set is exactly canonical); B13B
  extracts the *existing* cut logic behind a verified core (same node boundaries); B13C only adds
  tests. The fuzz corpora (`cas/fuzz`) need no regeneration — and in fact the strengthened oracles
  (B13C) *extend* what those corpora are checked against, a strict tightening.
- **B13A raises the cas gate; B13B raises it further and adds at most one trusted seam.** B13A is
  a pure verified-surface *extension* (node decoder), no new trust — gate 65 → 65+N₁. B13B adds
  `is_boundary` as a trusted-total `external_body` (BLAKE3, already out-of-scope crypto per
  verus.md:117) — the *only* new trust, paired with a verified partition core that raises the gate
  to 65+N₁+N₂. If B13B is deferred (Design decision 2's latitude), the seam is **not** added and
  the gate is just 65+N₁. Either way the gate only ever *rises*; nothing weakens.
- **The headline property's *guard of record* is the strengthened proptest + fuzz, not Verus.**
  This must be stated plainly so the ledger is honest (rev1§6.1(e)): the concrete tree-shape
  canonical-form (the actual root-hash equality across edit orders, over real BLAKE3) is
  **test-routed**, exactly as rev1§6 routes the prolly tree to the baseline tier and exactly as
  B6 test-routes GC mark-set sufficiency (ledger `:55-63`). Verus mechanizes the *machinery*
  (decoder canonicality, partition conservation/discipline) — the parts that hold for any hash —
  not the hash-determined clustering. B13D records this split; no reviewer should read the Verus
  work as proving the tree shape.
- **The tier is proptest + Miri + fuzz — the CAS-decoder + baseline convention.** The node
  decoder is an adversarial-bytes decoder, so it keeps its **cargo-fuzz** routing (`tree_node`,
  `mount_recovery`) *and* gains Verus totality (B13A) — the rev1§3.7 "decoders are fuzz targets
  *and* Verus-total" double posture the entry codec already has. The canonical-form sweep uses the
  workspace case-count convention `cases: if cfg!(miri) { 4 } else { N }` (mirroring the existing
  `prolly.rs:1336-1339` and `cas/src/file.rs:121-123`); B13C *raises* the native `N` and widens
  the strategies. No Loom/Shuttle: the tree builder is single-threaded and atomic-free.
- **No new panics; the one existing panic path is disclosed, not introduced.** `Dir::save`'s
  255-level `expect` (`:387`) predates B13 and is a probabilistically-unreachable backstop. B13
  does **not** remove it (doing so cleanly would need either a hash-cooperation argument Verus
  can't supply or a refusing `Result` signature change rippling through `tree.rs`/`store.rs` — out
  of scope, Design decision 2). B13C *probes* it with deep adversarial shapes (it must never fire
  on realistic input) and B13D *records* it as a disclosed structural backstop, the GC-mark-bound
  posture (ledger `:60`).

**Baseline to re-establish at end of B13:**
- `cargo verus verify -p cas --no-default-features` green at **≥ 65/0** — rising by the node
  decoder (B13A) and, if it lands, the partition core (B13B). Record the new total in the ledger
  Baselines row; if B13B is deferred, record the B13A-only total and the `is_boundary` seam as
  *not* added.
- `cargo test -p cas` green: the strengthened `canonical_form`, the promoted
  `structural_sharing` proptest, the new multi-level round-trip / locality / chunker-determinism
  proptests, and any unit tests B13A adds for the node decoder's rejection cases.
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas` clean across the new
  prolly proptests (4 cases under Miri — BLAKE3 is interpreted under Miri, so deep-tree cases stay
  cheap by construction at 4) and the committed `--test fuzz_regressions --test fuzz_corpus` sweep
  stays clean (format/codec corpora unaffected — B13 is format-stable).
- The `cas/fuzz` targets build and the committed corpora replay; `tree_node` now also exercises
  the whole-tree root-stability oracle (B13C) without corpus regen.
- The aarch64 cross-build links `storaged` (which pulls `cas`) unchanged and **QEMU boot stays
  green** — the live witness that the lifted-into-Verus node decoder still serves the real store
  on the boot path (the load-bearing acceptance B11B/B12 used).

---

## Design decision 1 — the verification bar: a *layered* answer (Verus for the Hash-free machinery, verification-grade proptest+fuzz for the BLAKE3-dependent shape), not "Verus xor proptest" *(resolves the parent plan's B13 open "prove or strengthen"; pin in B13A)*

The parent plan leaves the bar open: "prove (Verus) **or**, if a full proof is out of reach,
substantially strengthen the proptest." B13 resolves the "or" as **"each where it is
proportionate,"** because BLAKE3's position forces the split (the "What is actually true today"
section): the machinery is Hash-free and provable; the concrete shape is hash-determined and not.

- **Adopted — a three-track layered bar:**
  - **Track V1 (must-do, B13A): the node decoder into the verified, total-∀-bytes surface.**
    `parse_node`/`load_node` are on-disk decoders; rev1§6 routes CAS decoders to Verus, and every
    *sibling* decoder (superblock, WAL record, single-entry TLV) is already there. Verifying the
    node decoder is **Hash-free** (it parses byte structure and composes on the already-verified
    `decode_raw`), needs **no new trusted seam**, raises the gate, and directly mechanizes the
    rev1§6 "decode-then-re-encode reproduces the input" oracle at the *node* grain (the leaf
    canonical round-trip). This is the cleanest, lowest-risk, highest-certainty win — do it.
  - **Track V2 (stretch, B13B): the partition core.** Extract `build_level`'s cut logic behind a
    verified `partition`/`split_points` core over an **opaque** boundary predicate and prove
    conservation (`flatten(partition) == items`) + boundary discipline + non-empty/≤MAX. This
    mechanizes the *encode-side* structural correctness the audit names ("level construction is
    load-bearing"). It needs the one `is_boundary`-is-total seam (BLAKE3, +1 tally). Attempt it;
    fall back to V1+T-only if it proves disproportionate (the `Seq<Seq<u8>>` flatten/conservation
    proof heavier than the existing `lemma_cat` idioms support), recording *which* bar was met —
    the B11 latitude (`12_b11-detail.md` Design decision 0) applied verbatim.
  - **Track T (must-do, B13C): verification-grade proptest + fuzz for the headline shape.** The
    BLAKE3-dependent canonical-form (root-hash equality across edit orders) is the spec's baseline
    tier (rev1§6). Raise it from sampling (≤64 entries, never multi-level, 256 cases) to a real
    adversarial sweep (multi-level shapes spanning the 128-cap × many edit orders × churn × the
    decode-then-reencode whole-tree oracle × a locality proptest), all Miri-replayed. This is the
    *guard of record* for the headline property.
- **Rejected — "full Verus proof of `Dir::save`'s canonical shape."** It requires an injective
  model of BLAKE3 over arbitrary item bytes to say *which* items are boundaries, which verus.md
  (117) and rev1§6 put out of scope; it must confront `save`'s probabilistic-termination /
  255-level panic backstop (Design decision 2); and it would mechanize nothing the layered bar
  doesn't, at research-grade cost. **Rejected** — disproportionate, and the spec doesn't ask for
  it (it routes the prolly tree's *property* to baseline, its *decoders* to Verus, which is
  exactly the layered bar).
- **Rejected — "pure proptest floor, no Verus."** Acceptable by the parent plan's literal "or,"
  but it leaves the node decoder the *one* CAS decoder outside the verified surface for no reason
  (V1 is clean and Hash-free) and forgoes a real gate rise. **Rejected** as the *floor*, kept only
  as the B13B-deferred fallback (V1 + T still ship).

**Recommendation: the layered bar — V1 (node decoder, must-do, no new seam) + T (verification-grade
proptest+fuzz, must-do) + V2 (partition core, attempt; +1 seam; fall back with the bar recorded).
Mechanize the machinery, test-route the BLAKE3-dependent shape, and say which is which (B13D).**

---

## Design decision 2 — leave `Dir::save`'s recursion (and its 255-level `expect`) plain Rust; do *not* attempt to verify or de-panic the spine loop *(pin in B13A)*

`Dir::save`'s `while nodes.len() > 1` spine (`:386-397`) is the part a naïve "verify the tree
builder" reading would target, and it is precisely the part that *cannot* be cleanly verified.

- **Adopted — the spine loop stays exec plain Rust; B13 verifies the *partition* (one level), not
  the *climb* (all levels).** Three independent blockers make the climb's full verification
  disproportionate:
  1. **No hard termination metric.** The loop shrinks the node count only because hash boundaries
     thin each level (avg fanout 32); there is no *provable* decrease without modeling BLAKE3.
     verus.md §4 ("Termination: a finite quantity that strictly drops") has no finite quantity to
     offer here that doesn't route through the hash. The `expect("tree deeper than 255 levels")`
     (`:387`) is the structural backstop, and it is a **panic** — the climb is therefore not even
     total without the hash argument.
  2. **Hash opacity.** Even with termination assumed, every per-level property worth proving (the
     boundary set, the spine shape) is hash-determined, so Verus could prove only what already
     holds for any predicate — which is the *single-level* partition core (B13B), not the climb.
  3. **I/O interleaving.** The loop calls `store.put` each level; the verified content is the pure
     cut logic, which B13B extracts and proves *separately* from the I/O, exactly as B7 connected
     verified decision cores to plain-Rust I/O sequencing via `requires/ensures` rather than
     proving the orchestration end-to-end.
- **The 255-level `expect` is disclosed, not removed.** Removing it cleanly means either (a) a
  hash-cooperation termination argument Verus can't give, or (b) changing `save`'s signature to
  `Result` and threading a `TreeTooDeep` error through `tree.rs`/`store.rs`/the flush path — a
  ripple far out of proportion to a backstop that fires at probability ≈ (1/32)^255. B13C *probes*
  it (deep adversarial shapes must never trip it) and B13D *records* it as a disclosed structural
  backstop, the same posture as the GC mark-walk bound (ledger `:60`, "the bound is structural").
- **Rejected — verify the climb under an `assume`d hash thinning.** An `assume` that boundaries
  thin would be a bare in-proof assumption of the very thing in question (verus.md forbids bare
  `assume`s that aren't labeled contracts; the ledger's "none survive," `:68`), and it would prove
  a tautology. **Rejected.**

**Recommendation: verify one level's partition (B13B), leave the multi-level climb and its
255-level backstop as disclosed plain Rust, and let B13C's deep-shape sweep be the climb's guard.**

---

## Design decision 3 — how the partition core models the split predicate: an *opaque trusted-total* `is_boundary` seam (BLAKE3), proven *around*, never *through* *(pin in B13B)*

B13B's partition core must talk about `is_boundary` without modeling BLAKE3. The pattern is
settled elsewhere in the tree (the `checksum_ok`/`wal_checksum_ok` interpreted-hash seams,
ledger §2).

- **Adopted — `is_boundary` becomes a trusted-total `external_body` exec fn whose only `ensures`
  is determinism/totality, and the partition core is proven over its *result*.** Concretely: the
  verified core computes the node boundaries as a pure function of `(items, λ item. is_boundary
  call result, MAX_NODE_ENTRIES)`; it is proven to **conserve and order** (`flatten == items`),
  to cut **only** where the predicate or the cap says, and to emit non-empty ≤MAX nodes — *for any
  predicate*. `is_boundary`'s BLAKE3 body is the seam (interpreted hashing, out of SMT scope,
  verus.md:117), trusted-total exactly like `checksum_ok` (ledger `:97`): inspects a buffer,
  returns a bool, no panic. The conservation/discipline lemmas need **only** totality+determinism
  of the predicate — *not* injectivity — so the seam is minimal (no injective-hash ghost needed,
  unlike verus.md:118's general advice; the partition is correct regardless of *which* items
  boundary).
- **Consequence for the ledger (B13D):** +1 `external_body` (a 3rd CAS interpreted-hash:
  `checksum_ok`, `wal_checksum_ok`, **`is_boundary`**), tally 13 → 14, with its host test named
  (the strengthened `canonical_form`/`roundtrip` proptests + the `tree_node` fuzz oracle exercise
  it — the "names both a reason it is a boundary and the host test" rule, ledger `:11-13`).
- **Rejected — model BLAKE3 injectively to prove the *concrete* boundary set.** That is the
  out-of-scope crypto Design decision 1 already rejected; the partition core deliberately proves
  *less* (structure for any predicate) at *no* trust cost beyond the one totality seam.
- **Rejected — keep `is_boundary` exec and inline its call in a verified `build_level`.** Verus
  can't reason about a call into a BLAKE3 exec fn without a contract; the contract *is* the seam.
  There is no proof without naming the seam. **Rejected** as a non-option.

**Recommendation: a minimal trusted-total `is_boundary` seam (totality+determinism only, no
injectivity), the partition core proven around it; ledger tally 13 → 14 with the host test named.
If B13B is deferred, the seam is not added.**

---

## Design decision 4 — scope: prolly tree only; the chunker is already covered, add one symmetry proptest, do not rebuild it *(pin in B13C)*

rev1§6 couples "the chunker **and** prolly tree especially," so the scope boundary needs a
decision.

- **Adopted — B13 is prolly-scoped (the parent plan's Touches), and the chunker gets one small
  symmetry proptest, not a verification track.** The FastCDC chunker already meets the baseline
  bar handsomely: `cas/fuzz/fuzz_targets/chunker.rs` asserts determinism, chunks-concatenate-to-
  input, in-bounds cuts, and streaming==one-shot, *plus* a proptest (the fuzz header says so), and
  B12E's landed neighborhood-re-chunk oracle (`neighborhood == whole-file root hash`) is itself a
  powerful chunker-canonical-form check on the live flush path. The chunker's determinism is a
  format-constant property (gear table + masks, `chunk.rs:1-16`) already routed and covered. B13C
  adds **one** proptest asserting `boundaries(p, data)` is a pure function of `data` and that
  inline-vs-chunked content selection (`file.rs:store_file`, the INLINE_MAX rule) is
  content-determined — symmetry with the prolly sweep, cheap, no rebuild.
- **Rejected — pull the FastCDC gear loop into Verus.** verus.md (117) routes "the FastCDC gear
  loop" explicitly **out of scope** (perf inner loop), the same row as BLAKE3. **Rejected.**
- **Rejected — expand B13 to a full chunker re-verification.** Duplicates existing strong fuzz +
  proptest + the B12E oracle for no gain; out of the parent plan's prolly-scoped Touches.
  **Rejected.**

**Recommendation: prolly-scoped; one cheap chunker-determinism symmetry proptest in B13C; no
chunker Verus, no chunker rebuild.**

---

## Sub-phase B13A — lift the directory **node decoder** into the verified, total-∀-bytes surface *(must-do; the clean Verus win; resolves Design decisions 1, 2)*

Extend the existing `verus!{}` core from one *entry* to a whole *node*: verify that the node
decoder is total over arbitrary bytes and that a **leaf** node's decode round-trips canonically
(`encode_node(decode_node(b)) == b[..k]`), composing term-for-term on the already-verified
`decode_raw`. This is the node-grain of the rev1§6 "decode-then-re-encode reproduces the input
bytes" oracle and the rev1§4.9 "exactly one encoding per logical entry," extended to "exactly one
encoding per logical leaf node."

- **Touches:** `cas/src/prolly.rs` — the `verus!{}` block (`:563-1138`) gains a node-level
  `decode_node`/`encode_node` pair (or `requires/ensures` added to the existing `parse_node`
  refactored into the block); `parse_node` (`:433-459`) and `load_node`'s per-node parse
  (`:461-508`) route through it. The `Reader` helpers (`:236-276`) move into / are mirrored by the
  verified byte readers (`read_u8`/`u32` already exist as `fits`+indexing idioms in the block).
- **Depends on:** Part A blessed. The existing `decode_raw` core (the entry it calls per leaf
  item). No intra-B13 dep.
- **Work:**
  - **Define `canonical_node_bytes` (spec).** A leaf node is `seq![0u8] + u32_le(count) +
    concat_i canonical_bytes(entry_i)`; the spec function the exec decoder is proven to consume
    exactly, mirroring `canonical_bytes` at the node grain. (Internal nodes — `[level][count]` then
    `[key_len][key][child_hash]*` — get **totality only**, like the `tree_node` fuzz oracle's
    internal arm, because `parse_node` lowers separator keys into child hashes; there is no
    lossless single-node internal re-encoder. The internal *lossless* level is covered by B13C's
    whole-tree root-stability oracle, not here.)
  - **Verify `decode_node` total ∀ bytes** (the no-panic theorem, exactly as `decode_raw` is
    total): `level` (u8), `count` (u32) ≤ `MAX_NODE_ENTRIES` checked, then `count` leaf entries via
    the verified `decode_raw` loop (carrying `canonical_node_bytes` as the running concat invariant
    via `lemma_cat`), then the trailing-bytes check. On `Ok` for a leaf: consumed prefix ==
    `canonical_node_bytes`. For an internal node: total, bounded, separator-key discipline checked.
  - **Verify `encode_node`** produces exactly `canonical_node_bytes` for leaves (the encode half,
    mirroring `encode_raw`), so the round-trip theorem composes: `decode_node(encode_node(n)) == n`
    and `encode_node(decode_node(b)) == b[..k]` for accepted leaf `b`.
  - **Rewire** `parse_node`/`load_node` to call the verified decoder so the *running* code is the
    proved one (the B7 "connect verified cores to the running path" discipline), and keep the
    `tree_node`/`mount_recovery` fuzz targets pointed at it (totality is now *also* a theorem, the
    rev1§3.7 double posture).
- **Acceptance:**
  - `cargo verus verify -p cas --no-default-features` green at **65 + N₁** (record N₁); the node
    decoder is in the verified surface, totality and leaf canonical round-trip proven.
  - `cargo test -p cas` green including new unit tests for the node decoder's rejection cases
    (over-wide count, level mismatch, trailing bytes, separator-key mismatch — the `:475-505`
    checks, now with verified totality behind them).
  - `tree_node`/`mount_recovery` fuzz targets build and replay corpora; `decoder_rejects_garbage`
    still green (now backed by a totality theorem, not just sampling).
  - cas Miri leg clean over the node decode.
- **Effort/Risk:** M / low–medium. The idioms exist one grain down (`decode_raw`); the new work is
  the `count`-loop concat invariant — a `lemma_cat` accumulation the block already does for the
  opt-section. Lowest-risk high-certainty piece; do it first.

---

## Sub-phase B13B — verified **partition core**: `build_level`'s cut logic proven (conservation + boundary discipline + non-empty/≤MAX) over an opaque split predicate *(stretch; resolves Design decision 3; B11-style fall-back latitude)*

Mechanize the encode-side structural property the audit names ("level construction is
load-bearing"): that one level's node-cutting is a **lossless, ordered partition** whose blocks
are cut only at the predicate or the cap — for *any* predicate, so it holds under the real
(BLAKE3) `is_boundary` without modeling BLAKE3.

- **Touches:** `cas/src/prolly.rs` — extract `build_level`'s cut loop (`:312-326`) into a verified
  `partition`/`split_points` core inside the `verus!{}` block, parameterized over the trusted-total
  `is_boundary` seam (Design decision 3); the exec `build_level` consumes the core's cut points and
  does the `store.put` I/O (unverified, as B7 leaves I/O sequencing plain).
- **Depends on:** B13A's verified-block extensions (shared idioms) are convenient but not strictly
  required; independent of B13C.
- **Work:**
  - **Model.** `partition(items: Seq<Seq<u8>>, bnd: spec fn(Seq<u8>)->bool) -> Seq<Seq<int>>` (or
    a `Vec<usize>` cut-index list in exec with a `Seq` spec twin) — the grouping of item indices
    into nodes. The exec computes it with the trusted-total `is_boundary` seam supplying `bnd`'s
    value per item.
  - **Prove, for any `bnd`:**
    - **Conservation/order:** `flatten(partition(items)) == items` — concatenating the blocks in
      order reproduces the input index sequence exactly (no item dropped, duplicated, or
      reordered). This is *the* load-bearing lemma (an entry can't be lost or moved by chunking).
    - **Boundary discipline:** every block except possibly the last ends at an index `i` with
      `bnd(items[i]) || (i - block_start + 1) == MAX_NODE_ENTRIES`; the last block ends at
      `items.len()-1`. ⇒ the cut set is **determined by `(items, bnd)`** — equal inputs ⇒ identical
      partition (the canonical-shape lemma, modulo the opaque hash).
    - **Well-formedness:** every block is non-empty and has ≤ `MAX_NODE_ENTRIES` items; the
      partition is non-empty iff `items` is.
  - **Connect** the core to exec `build_level` so the running cut logic *is* the proved one (the
    proved cut points drive `store.put`); the node *bytes* emitted per block are the B13A-verified
    `encode_node` over that block — so encode-side canonicality composes: equal item sequence ⇒
    identical partition (B13B) ⇒ identical node bytes (B13A) ⇒ identical hashes.
  - **Ledger:** add the `is_boundary` trusted-total seam row (Design decision 3); tally 13 → 14.
- **Acceptance:**
  - `cargo verus verify -p cas --no-default-features` green at **65 + N₁ + N₂** (record N₂); the
    partition core's conservation + discipline + well-formedness proven over the opaque predicate.
  - `cargo test -p cas` green; `structural_sharing`/`canonical_form` (B13C) exercise the
    `is_boundary` seam as its named host test.
  - cas Miri leg clean.
- **Fall-back (the B11 latitude, `12_b11-detail.md` Design decision 0):** if the `Seq<Seq<u8>>`
  flatten/conservation proof proves heavier than the existing `lemma_cat` idioms support, **defer
  B13B to Out of scope** as recorded future hardening (the verified partition core is *additive* —
  the gate would only rise if later taken, nothing weakens), ship **V1 + T**, and record in the
  ledger that the encode-side partition is *test-routed* (the strengthened `canonical_form` +
  `roundtrip` proptests are its guard) rather than mechanized. State which bar was met (the parent
  plan's "state which bar is met and record it in the ledger," B13 acceptance echoed from B11).
- **Effort/Risk:** M–L / medium. The conservation proof over a `Seq` of `Seq`s is the risk
  (verus.md §2 "choose the representation that makes ops one-liners" applies — model the partition
  as a cut-index list, not nested seqs, to keep `flatten` a single `subrange` concat). De-risked by
  the fall-back: V1+T ship regardless.

---

## Sub-phase B13C — verification-grade canonical-form proptest + fuzz: multi-level shapes × edit orders × churn, the whole-tree decode-then-reencode oracle, split-locality, and chunker symmetry *(must-do; the headline property's guard of record; resolves Design decision 4)*

Raise the rev1§4.1 headline property from light sampling to a real adversarial sweep — the
spec-mandated baseline tier (rev1§6) made verification-grade. This is the guard of record for the
BLAKE3-dependent tree shape that Verus (Design decision 1) cannot reach.

- **Touches:** `cas/src/prolly.rs` `mod tests` (`:1142-1432`) — rewrite/extend `canonical_form`,
  promote `structural_sharing_on_small_edit`, widen `arb_entry`/`arb_entries`, add round-trip and
  chunker-symmetry proptests; `cas/fuzz/fuzz_targets/tree_node.rs` — add the whole-tree
  root-stability oracle.
- **Depends on:** none in B13 (independent of A/B; can land in parallel). The Verus tracks and the
  proptest tracks are mutually reinforcing but not ordered.
- **Work:**
  - **Reach multi-level trees and the 128-cap (the central blind spot).** Widen `arb_entries` to
    span counts well past `MAX_NODE_ENTRIES` (e.g. up to ~500–2000 native; bounded by `cfg!(miri)`
    to a handful) so the sweep builds **≥ 3-level** trees and actually fires the
    `count == MAX_NODE_ENTRIES` forced boundary (`:314`) and the spine climb (`:386-397`) — neither
    of which the current `≤ 64`-entry strategy ever reaches. Diversify names/content so
    `is_boundary` outcomes spread (avoid degenerate all-/no-boundary inputs *and* include a
    deliberate near-degenerate case that stresses the spine toward, but never to, the 255-level
    backstop — Design decision 2's probe).
  - **Edit-order × churn matrix (the headline).** Keep and strengthen `canonical_form`: for the
    same final logical set, build via N independent shuffled edit orders **and** interleaved churn
    (entries inserted then removed, including churn that itself crosses node boundaries), assert
    **all** roots equal. Raise native case count (e.g. 256 → 1024+; keep `cfg!(miri) { 4 }`).
  - **Whole-tree decode-then-reencode oracle (rev1§6).** Add a proptest and extend `tree_node`
    fuzz: `Dir::save → Dir::load → Dir::save` reproduces the identical root across multi-level
    trees (the lossless **internal-node** level B13A's single-node leaf oracle can't reach — this
    is where the separator-key discipline and the spine get their canonical-round-trip guard).
    `Dir::load(arbitrary bytes)` stays total (extend `decoder_rejects_garbage`).
  - **Split-locality as a proptest (promote `structural_sharing_on_small_edit`).** Over many
    shapes and many edit sites, a one-entry upsert/remove rewrites **O(depth)** nodes (assert a
    bound scaling with tree depth, not a fixed `≤ 8`), witnessing the per-item-split locality
    rev1§4.1 attributes to the pure `is_boundary` — and *contrasting* it with a rolling-window
    chunker (the module doc's `:13-16` claim, now tested not just asserted).
  - **Chunker symmetry (Design decision 4).** One proptest: `boundaries` is a pure function of the
    data, and `file::store_file`'s inline-vs-chunk selection is content-determined (the INLINE_MAX
    rule), mirroring the prolly canonical-form sweep — cheap symmetry, no chunker rebuild.
  - **All Miri-replayed** at 4 cases (BLAKE3 interpreted under Miri keeps deep cases cheap by
    construction); the committed fuzz corpora replay unchanged (format-stable).
- **Acceptance:**
  - The strengthened `canonical_form` builds **multi-level (≥3) trees that fire the 128-cap** and
    asserts root equality across many edit orders × churn — demonstrably exercising shapes the old
    `≤ 64`-entry sampling never reached (assert tree depth/`store.len()` in the test to prove the
    shapes were reached, so a future shrink can't silently regress coverage).
  - The whole-tree `save→load→save` root-stability oracle (proptest **and** `tree_node` fuzz) green
    over multi-level trees; `decoder_rejects_garbage` still total.
  - The split-locality proptest passes with a depth-scaled bound over many shapes/edit sites.
  - The chunker-symmetry proptest green.
  - `cargo test -p cas` and the cas Miri leg green; `cas/fuzz` builds + corpora replay; cas gate
    unchanged by this sub-phase (tests only — **65 + N₁ (+ N₂)** from A/B).
- **Effort/Risk:** M / low–medium. The risk is *cost*, not correctness: deep multi-level native
  cases over real BLAKE3 are slower — bound the native count and tree size so the suite stays
  CI-friendly, and lean on the `cfg!(miri) { 4 }` convention so the Miri sweep stays quick. State
  any native cap chosen (the "no silent caps — `log` what was dropped" discipline, applied to test
  coverage: a comment naming the max entry count and why).

---

## Sub-phase B13D — ledger + rev1§6.1 reconciliation + module-doc honesty *(the finishing item; obeys the rev1§6.1(e) "no trust-routed property mistaken for mechanized" rule)*

Record, in the one source of truth, exactly which half of the canonical-form property is
*mechanized* and which is *test-routed*, so no reviewer reads the Verus work as proving more than
it does — the B6 GC-sufficiency-note discipline applied to the prolly tree.

- **Touches:** `doc/guidelines/verus_trusted-base.md` (the ledger); `cas/src/prolly.rs` module
  doc (`:1-29`). **No rev1 spec change** (Part A blessed) — but confirm rev1§6.1(e)'s discipline
  is honored; rev1§6.1 has no prolly-specific `[verifying]` line to flip (the prolly tree is a
  baseline-tier component there), so the ledger carries the record.
- **Depends on:** B13A (and B13B if it landed) for the final gate totals and seam tally; B13C for
  the strengthened-test description.
- **Work:**
  - **Mechanized half (record in the verified-surface scope, ledger `:36-53`):** the **directory
    node decoder** (`decode_node`/leaf canonical round-trip, B13A) joins "the single-entry TLV
    codec" as a verified CAS decoder; if B13B landed, the **partition core** (conservation +
    boundary discipline over the opaque predicate) joins too. Update the Baselines row: cas gate
    **65 → 65 + N₁ (+ N₂)**.
  - **New trusted seam (only if B13B landed, ledger §2):** add the `is_boundary` row — BLAKE3
    split rule, interpreted hashing out of SMT scope, trusted total (totality+determinism only, no
    injectivity), host test = the strengthened `canonical_form`/`roundtrip` proptests + `tree_node`
    fuzz oracle. Update the **Tally** 13 → 14 (3rd CAS interpreted-hash) and the §2 table.
  - **Test-routed half (a GC-sufficiency-style note, ledger `:55-63` is the template):** state
    plainly that the **concrete tree shape's canonical form** — which logical contents map to which
    root hash, the hash-determined clustering — is delivered at the rev1§6 baseline tier (the
    verification-grade `canonical_form` + whole-tree round-trip + locality proptests, all
    Miri-replayed), **not** Verus-mechanized, because mechanizing it would drag interpreted BLAKE3
    into the proof (the same wall B6 hit with reachability). Note the `Dir::save` 255-level `expect`
    as a disclosed structural backstop (probability ≈ (1/32)^255), the GC-mark-bound posture. This
    is the rev1§6.1(e) "property routed to trust not mistaken for a mechanized one" entry for the
    prolly tree.
  - **Module-doc reconciliation (`prolly.rs:1-29`):** the header already states canonical form and
    "Decoders here are strict (they are cargo-fuzz targets)"; add that the **node decoder is now
    Verus-total** (B13A) and the **partition is verified** (B13B, if landed) — or, if B13B
    deferred, that the partition is test-routed — so the doc matches the ledger.
- **Acceptance:**
  - The ledger's verified-surface scope, §2 seam table (+ tally), Baselines row, and the
    test-routed note all agree with the code and with each other (the "ledger and code agree
    line-for-line" rule); `rg "external_body|assume_specification" cas/` matches the recorded tally
    (13, or 14 if B13B landed).
  - The prolly module doc names the mechanized vs test-routed split.
  - No claim anywhere reads the BLAKE3-dependent tree shape as Verus-proven.
- **Effort/Risk:** S / low. Documentation + tally bookkeeping; the load-bearing care is *honesty*
  (not over-claiming), which the GC-sufficiency note (`:55-63`) and the B11 heap-arena note
  (`:125-130`) are the templates for.

---

## Execution order

```
B13A  node decoder → verified total-∀-bytes surface (leaf canonical round-trip)   [must-do; clean Verus win; do first]
        (mechanizes the rev1§6 decode-then-reencode oracle at the node grain; gate 65 → 65+N₁; no new seam)
   │
   ├─► B13B  verified partition core (conservation + boundary discipline over opaque is_boundary)   [stretch; +1 seam; B11-style fall-back]
   │           (mechanizes the encode-side structure; gate → 65+N₁+N₂; defer to Out-of-scope if disproportionate, V1+T still ship)
   │
B13C  verification-grade canonical-form proptest + fuzz   [must-do; independent of A/B; parallel]
        (the BLAKE3-dependent headline shape's guard of record: multi-level × edit-order × churn,
         whole-tree decode-then-reencode oracle, split-locality, chunker symmetry; all Miri-replayed)
   │
B13D  ledger + rev1§6.1(e) + module-doc reconciliation   [finishing; after A (+B) gates and C's test description]
        (record mechanized half vs test-routed half; seam tally 13 → 13/14; no over-claim)
```

- **B13A is the foundational, lowest-risk, highest-certainty piece** (the node decoder into Verus,
  Hash-free, no new seam, gate rises) — do it first; it also seeds the verified-block idioms B13B
  reuses.
- **B13B is the stretch**, with explicit B11-style latitude to fall back to Out-of-scope while
  V1+T still ship the parent plan's acceptance ("documented strong proptest with the
  decode-then-reencode oracle").
- **B13C is fully independent** of A/B (it's tests) and can land in parallel; it is the *guard of
  record* for the headline property and must not be skipped just because B13A/B land — the
  BLAKE3-dependent shape is its sole guard.
- **B13D last**, once the gate totals (A, and B if it landed) and the strengthened-test
  description are final; its whole job is honesty (the mechanized/test-routed split recorded so the
  rev1§6.1(e) discipline holds).

## Out of scope for B13 (recorded so it is not mistaken for a gap)

- **Verus proof of `Dir::save`'s concrete tree *shape* (which contents → which root).** Requires
  an injective BLAKE3 model over arbitrary item bytes — out of scope per verus.md:117 and rev1§6,
  and the same wall B6 hit with GC reachability. The BLAKE3-dependent shape is **test-routed** at
  the rev1§6 baseline tier (B13C), recorded in the ledger (B13D). This is a *routing*, not a gap.
- **Verus over the multi-level spine loop and removal of the 255-level `expect`** — Design
  decision 2: no provable termination metric without modeling the hash; the backstop is a disclosed
  structural guard (the GC-mark-bound posture), probed by B13C, recorded by B13D. De-panicking it
  via a `Result` signature would ripple through `tree.rs`/`store.rs`/the flush path, out of
  proportion to a (1/32)^255 event.
- **The partition core, *if B13B is deferred*** — recorded as additive future hardening (the gate
  rises if later taken, nothing weakens), exactly as B11 recorded the `free_or_coalesce` ring core.
  If deferred, the encode-side partition is test-routed (the strengthened `canonical_form` +
  `roundtrip` proptests its guard), and B13D records *that* bar.
- **The FastCDC chunker's verification** — verus.md:117 routes the gear loop out of scope; the
  chunker already meets the baseline bar (its own fuzz oracle + proptest + B12E's neighborhood-
  vs-whole-file root-hash oracle). B13C adds only a small symmetry proptest (Design decision 4).
- **Node-level incremental surgery / sub-`Dir::save` optimization** — the module doc (`:20-25`)
  deliberately defers it past MVP (`Dir::save` rebuilds from the full entry list, which is the very
  thing that makes the canonical-form argument simple). B13 verifies/tests the full-rebuild path as
  it stands; it does not add incremental update.
- **`is_boundary` injectivity / any property of *which* items boundary** — the partition core
  (B13B) proves structure for **any** predicate (Design decision 3); it deliberately proves *less*
  than the concrete boundary set, at the cost of only one totality seam, never an injective-hash
  ghost.
- **On-disk format change / `SB_VERSION` bump / corpus regen** — B13 is **format-stable**: it
  verifies and stress-tests the *existing* node bytes, split constants, and hashes; it changes no
  encoding. The fuzz corpora replay unchanged; B13C *tightens* what they are checked against.
- **rev1 spec edits** — Part A is blessed; B13 conforms code to the existing rev1§4.1/§4.9/§6 text
  and records the verified/test-routed split in the ledger (rev1§6.1(e) has no prolly `[verifying]`
  line to flip — the prolly tree is a baseline-tier component there).
