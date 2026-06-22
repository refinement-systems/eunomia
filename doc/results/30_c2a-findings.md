# C2A — re-key the overlay on an ephemeral `FileId`

Phase **C2A** of `doc/plans/18_c2-detail.md` (the detailed decomposition of
parent-plan C2, `doc/plans/0_address_audit_rev0.md:714-729`). C2A is the
**foundation** of the C2 wave: it re-keys the per-ref in-memory overlay from
**path** to an ephemeral, server-runtime **file id** (rev1§4.3/§4.9), behind an
`id → name` indirection — but with **identical external behavior**. No rename, no
wire change, no new semantics; a pure internal refactor that unblocks C2B
(`Store::rename` + `WalOp::Rename`) and C2C (unlink-while-open).

It makes rev1§4.3's already-blessed *"keyed by (file id, …)"* claim true: the
old overlay was path-keyed (`cas/src/overlay.rs`) and its own comment said the
file-id indirection was *"deferred until a rename operation exists."* That
comment is now the realized design.

## What landed

- **`cas/src/overlay.rs`** — the re-keying core. `Overlay` goes from
  `{ files: BTreeMap<Path, FileOverlay>, unlinks: BTreeSet<Path>, bytes }` to
  four indices keyed through `FileId` (a new `pub type FileId = u64`):
  - `by_id: BTreeMap<FileId, FileOverlay>` — the interval maps, re-keyed.
  - `by_name: BTreeMap<Path, FileId>` — live name → id; the name-ordered index
    that does name→id resolution **and** the prefix-scan directory listings.
  - `names: BTreeMap<FileId, Option<Path>>` — id → current name (rev1§4.9's
    *"ID → current-path map"*); the inverse of `by_name` for live files. `None`
    is reserved for unlink-while-open (C2C) and **never produced in C2A**.
  - `tombs: BTreeSet<Path>` — the old `unlinks`, unchanged in role.
  `FileOverlay` gains `origin: Option<Path>` (DD3) — set at first write but
  **inert** in C2A (see below). `write`/`unlink`/`state`/`is_empty`/`files`/
  `files_in_dir`/`unlinked_in_dir`/`unlinks` are rewritten over the indices.
- **`cas/src/store.rs`** — the allocator + threading only. A store-global
  `next_file_id: FileId` field (init `0` in both constructors — `format` and
  `mount`, the latter before WAL replay); `apply_to_overlay` threads
  `&mut self.next_file_id` into the `write` arm via disjoint field borrows.

No `disk.rs`, `tree.rs`, Verus, or `storage-server` change. `SB_VERSION` stays
**4** (the bump is C2B). The verified WAL structural decoder is untouched, so
`cargo verus verify -p cas --no-default-features` stays **80/0**. No spec edit,
no ledger change, no new seam.

## The key insight — keep the public method signatures stable

The substance of C2A is preserving the path-keyed semantics exactly through the
indirection. The cheapest way to guarantee that — and to keep the diff a single
module plus three lines of `store.rs` — was to **keep every public overlay
method's signature and return type unchanged**, so the `Store` glue that reaches
the overlay (`flush_ref`, `read`, `list`, `validate_mutation_path`) needs **no
change at all**:

- `files()` / `files_in_dir()` still yield `(&Path, &FileOverlay)`, now resolved
  `name → id → interval-map` by iterating `by_name` (which is in the same
  path-sorted order as the old `files.iter()`), so flush walks the **same names
  in the same order** → the committed tree and the flush I/O sequence are
  byte-for-byte identical.
- `unlinks()` / `unlinked_in_dir()` range-scan `tombs` exactly as before.
- `state()` resolves `name → id → FileOverlay`; `is_empty()` becomes
  `by_id.is_empty() && tombs.is_empty()`.

Only `Overlay::write` changed signature — it gained `next_id: &mut FileId`,
because allocating an id is the one thing the overlay can't do alone (the
allocator is store-global).

### Allocator: store-global, never persisted, re-derived on replay

`next_file_id` lives on `Store` (DD1's recommendation over per-ref counters: one
counter, less state, ids are opaque). It restarts at `0` on construction and is
re-derived deterministically by WAL replay — `mount` runs the same path-keyed op
stream through `apply_to_overlay`, so a `Write` to a name not yet seen allocates
the next id in the same order it did live. Ids never touch disk (rev1§4.9), so
the restart is sound — there is no allocation-order-in-the-hash hazard (the very
reason persistent inodes were rejected). The disjoint-field-borrow in
`apply_to_overlay` (`let next = &mut self.next_file_id;` then
`self.overlays.entry(...)`) compiles because the borrow checker sees the two
fields as independent paths.

### `origin` is set but inert (DD3 scaffolding for C2B)

`FileOverlay.origin` records the committed-tree path to read pre-edit base bytes
from: `Some(name)` at first write to a file with a base, `None` for a
fresh/resurrected file. It is **written but not read** in C2A — flush still reads
the base at the current name, which equals `origin` because no rename exists yet.
Carried `#[allow(dead_code)]` with a forward-pointer so C2B is a smaller diff
(flush switches its base lookup onto `origin`). A unit test pins the contract
(`origin_fixed_at_first_write`) so the field isn't untested scaffolding.

## Behavior-preservation: a verbatim oracle + an equivalence proptest

The re-key is the risk, so the headline test is an equivalence check against the
**pre-C2A overlay itself**. `overlay_matches_path_model` (in `overlay.rs` tests)
drives random `write`/`unlink` streams over a small fixed name set (two at the
root, two under `d/`, to exercise both root and subdirectory listings) through
both the re-keyed `Overlay` and a `RefOverlay` — a verbatim copy of the old
path-keyed struct + methods. After **every** op it asserts:

- `bytes()` and `is_empty()` agree;
- per-name `state()` agrees (collapsed to a comparable `(tag, content, fresh,
  mtime, extent)` snapshot via `snap()`);
- `files_in_dir` / `unlinked_in_dir` agree (sorted) for root and `d/`;
- the **id invariants** hold (`check_invariants`): `by_name`/`names` are mutual
  inverses for live files, `by_id` and `names` cover the same id set, no name is
  both live and tombstoned.

Capped to 4 cases under `cfg(miri)` like the existing proptest.

**Negative control with teeth** (`negative_control_resurrect_fresh`): a reference
that forgot resurrect-as-`fresh` would splice the new write into the stale base
(`"OLD"` + write `x` at 1 → `"OxD"`); the test asserts the real overlay's result
(`[0, 'x']`, base ignored) **differs** from that wrong oracle — so wiring the
broken model into the equivalence proptest would fail it. Plus two focused unit
tests: ids are shared across writes to one name (`overlapping_writes_last_wins`,
`next == 1` after three writes) and a fresh id is minted on resurrect
(`resurrect_after_unlink_reallocates_id`, `next == 2`).

## Verification (all green)

- `cargo test -p cas` — **110 passed, 0 failed** (incl. the new proptests +
  negative control and the **entire crash-recovery family**, unchanged across the
  re-key), plus the 9 fuzz-corpus + 10 fuzz-regression tests.
- `cargo test -p storage-server` — **19 passed** (overlay is reached only through
  the `Store`; the `cas::overlay::Path` alias storage-server imports is
  unaffected by the internal re-key).
- `cargo build -p cas -p storage-server` — clean, **no warnings** (the otherwise-
  dead `origin` field carries `#[allow(dead_code)]`).
- `cargo verus verify -p cas --no-default-features` — **80 verified, 0 errors**
  (unchanged; no WAL/decode touch).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p cas -j4`
  — clean (new proptests capped under `cfg(miri)`).
- `scripts/run-demo.sh` under the CLAUDE.md Perl process-group timeout harness —
  green: `[storaged] store mounted` → `serving`, `write`/`cat`/`ls`/`df` behave,
  no panic/`Corrupt` (no observable behavior change, as designed).

### Environment note — stale `target/miri` cache

The first Miri invocation failed in cargo-miri's `util.rs` with `No such file or
directory`, re-invoking a compile with `CWD=/Users/mjm/repo/eunomia-miri` — a
**since-deleted sibling worktree** whose absolute paths were baked into the
cached miri build fingerprints under `target/miri/`. There is no longer any
`.cargo/config.toml` or worktree referencing it. `rm -rf target/miri` and a fresh
run resolved it (clean rebuild of the miri std + deps, then the sweep). This is a
pre-existing cache artifact, independent of the C2A change; flagged here so a
future Miri failure with an `eunomia-miri` path is recognized as stale cache, not
a regression.

## Notes for C2B / C2C

- **C2B (`Store::rename` + `WalOp::Rename`)**: the O(1) rename is a `by_name` /
  `names` pointer swap with `origin` left untouched; flush then reads base from
  `origin` and writes at the current name, `tree::remove`-ing the old origin when
  they differ. `origin` is already populated correctly — flush just needs to
  consume it (drop the `#[allow(dead_code)]` then). The verified WAL decoder grows
  a tag-3 arm and `SB_VERSION` bumps 4→5 there.
- **C2C (unlink-while-open)**: `names[id] = None` is the orphan marker the
  structure already reserves; `unlink` will set it (keeping the `by_id` overlay)
  instead of reaping the id, and flush will discard `None`-named ids. The
  `flushable` iteration may switch from walking `by_name` to walking `by_id` +
  filtering `names[id]` to surface orphans for discard.
- The equivalence-proptest pattern (drive the real type and a reference model in
  lockstep, assert per-op) extends naturally to C2C's rename/unlink interleaving
  proptest — there the reference model gains explicit open-handle tracking.
