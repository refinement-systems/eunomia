# Plan — Part C2 detail: ephemeral file-id indirection + rename (re-key the per-ref in-memory overlay from **path** to an ephemeral, server-runtime **file id** with an `id → current-name` map that updates O(1) per rename regardless of dirty volume — rev1§4.9 — so open files follow renames and an unlinked-while-dirty file is discarded at flush; add a `WalOp::Rename` so an acknowledged-but-unflushed rename replays after a crash, extending the **verified** WAL structural-decode mirror — the B7B split — and re-establishing `cargo verus verify -p cas` ≥ 80/0; bump `SB_VERSION` 4→5 (refuse-old at mount, the B5 discipline); add a single wire `Request::Rename` op — postcard-appended, header `[0x45,0x51,0x02]` unchanged — whose cross-subtree-target denial is **structural** (both paths are `full_path`'d under the handle's subtree, `..` already rejected by `validate_path`); and surface it as a `mv` shell built-in for the QEMU smoke. Ids never touch disk (rederived on replay). The file-id *layer* stays server-internal; the spec's "open handle keeps working" is realized at the `Store` API where the rename/unlink interleaving proptest proves it — a client-visible open/fd wire surface is reserved for a follow-on, not built here. Coordinate with **B5** (both append to the `Request` enum and bump the on-disk format). No new trusted seam — the WAL decode extension stays *verified*, not trusted; the ledger tally is unchanged.)

Detailed, separately-implementable decomposition of **Phase C2** from
`doc/plans/0_address_audit_rev0.md` (parent-plan C2 at `:714-729`). C2 is **Wave-6**
work (`:790`), the last of the spec-deferred Part-C gaps that does **not** gate the
console track. It depends only on **Part A** being blessed (the rev1§4.3/§4.9 text it
conforms to) and on nothing else in Part B/C structurally; the parent plan's one
sequencing note is *"Coordinate with B5 (both touch the storage-server protocol
surface)"* (`:722-723`). It unblocks nothing downstream — **C4** (concurrent GC) depends
on B6, not C2 — so it is scheduled late for its own sake, not as a prerequisite.

The framing that shapes the whole phase: the persistent format is **purely path-keyed**
and stays that way (rev1§4.9 `:349`: *"The persistent format is purely path-keyed"*).
C2 changes only the **in-memory** overlay's key — from a path to an ephemeral file id —
and adds the one operation (`rename`) the overlay's own deferral comment says it is
waiting for. The audit found the gap honestly disclosed, not a violation; so C2 is
**conformance work against a correct spec**, with three load-bearing pieces beneath a
deceptively small surface (one new wire op, one `mv` command): (1) the overlay re-keying
and the `id → name` indirection that makes rename O(1) over dirty state; (2) a new WAL op
that drags the **verified** WAL structural-decode mirror along with it (the only place C2
touches the proof boundary); and (3) the rev1§4.9 unlink-while-open semantics, realized
and proven at the `Store` API. Everything else — the wire op, the cross-subtree denial,
the `mv` built-in — is mechanical once those three land.

**Closes (from the parent plan / audit).** Parent plan C2 `:715-716`; audit
`doc/results/0_audit_rev0.md` §3.2 `:386-387` (verbatim):

> **Ephemeral file-id indirection and rename** (rev0§4.9): no rename op exists in
> `cas`; the overlay is path-keyed. M2 debt.

Plus the parent-plan C2 obligations (`:724-727`): *"introduce ephemeral server-runtime
file IDs with an ID→current-path map (O(1) per rename); key the overlay on file id; add a
rename op; implement unlink-while-open (open handle keeps working; data discarded at flush
if the ID resolves to no path); deny cross-subtree-handle rename targets (unnameable)."*

Three scope notes, all load-bearing for C2's boundary:

- **The on-disk format stays path-keyed; only the in-memory overlay re-keys.** rev1§4.9
  (`:349`) and the rev1§4.9 "rejected alternatives" (`:463`: *"Persistent inodes and hard
  links … runtime file identity is provided by ephemeral server-side IDs instead"*) are
  explicit: ids are a *runtime* construct and *"never touch disk."* The WAL records stay
  path-bearing (`Write{path}`, `Unlink{path}`, and the new `Rename{from,to}`); the id ↔
  name maps are **rederived** by replaying those path-keyed records in order (Design
  decision 5). So C2 adds **no** persistent-inode layer and changes no tree-on-disk key.
- **The file-id *layer* is server-internal; the wire stays path-addressed + one `Rename`
  op.** The namespace model is openat-shaped — *"Every storage operation is `openat`-shaped
  — relative to an explicitly named handle"* (rev1§4.9 `:351`) — and no client today holds
  a file open across an unlink. So C2 realizes the rev1§4.9 open-file handle (*"the open
  handle keeps working against the overlay"* `:349`) at the **`Store` API**, where the
  rename/unlink interleaving proptest proves it, and adds only `Request::Rename` to the
  wire. A client-visible `Open`/file-descriptor wire surface is **reserved** (Design
  decision 2, Out of scope) — not built here.
- **Cross-*ref* rename (a copy with new lineage) is out of scope.** rev1§4.9 (`:349`):
  *"Rename across refs is a copy with new lineage."* A single `Rename` request carries one
  `handle`, which denotes one ref (`HandleTarget::Ref`, `storage-server/src/lib.rs:76-80`),
  so a cross-ref rename is **unexpressible** in the op C2 adds — it would need two handles
  and a copy path (overlaps **B5**'s protocol surface). C2 delivers *within-ref, within-
  subtree* rename only; the cross-ref copy is recorded out of scope (Design decision 4).

---

## Spec target — Part A is blessed; C2 makes one small edit on landing

Every citation is `rev1§` against the already-blessed text. C2 touches the verified
surface in exactly one place (the WAL structural-decode mirror, rev1§6.1(e)), but
**flips no seam line** — the extension stays *verified*, the lone trusted BLAKE3 record
seam is unchanged — so the only spec edit is to mark the deferral resolved.

- **rev1§4.3 — the mutation path & memtable keying** (`spec_rev1.md:230-249`, keying at
  `:234`): *"Each write lands in a per-ref in-memory **overlay** … keyed by (file id,
  offset range), also recording creates, deletes, and renames."* The blessed text already
  specifies **file-id** keying; today's code is path-keyed (`cas/src/overlay.rs:109`) and
  its own comment (`overlay.rs:9-11`) says rename + file-id indirection are *"deferred
  until a rename operation exists."* C2 makes §4.3 true. **No text change** — C2 conforms
  to the already-correct claim.
- **rev1§4.9 — runtime file identity, rename, unlink-while-open** (`spec_rev1.md:349`),
  C2's normative core, blessed verbatim as the design:
  > *"the 'file id' in the memtable keying (§4.3) is an ephemeral, server-runtime ID
  > assigned per open file. The overlay keys on it; an ID → current-path map updates O(1)
  > per rename regardless of how much dirty state exists, so open handles follow renames.
  > IDs never touch disk. Unlink-while-open: the open handle keeps working against the
  > overlay, but if at flush time the ID resolves to no path, the data is discarded …
  > Rename across refs is a copy with new lineage; rename targeting outside a subtree
  > handle is unnameable and therefore denied."*
  C2 implements exactly this. **No text change**; C2 conforms.
- **rev1§4.9 — directory trees & the subtree-handle property** (`:337`): *"Directory moves
  are O(depth) — detach a hash, reattach it — and the subtree-handle property (§2.3) holds
  literally."* C2's directory rename is precisely this detach/reattach
  (`tree::remove` returns the removed `Entry`; `tree::put` reattaches it — `cas/src/tree.rs:
  83-87,:42`); the *"unnameable and therefore denied"* cross-subtree property holds **for
  free** because both paths are resolved under the handle's subtree (Design decision 4). No
  text change.
- **rev1§4.5 — crash recovery / WAL replay** (`:268-280`, replay at `:274`): *"Replay the
  WAL from the recorded head to rebuild per-ref overlay state for acknowledged-but-
  unflushed writes."* C2 adds a third WAL op (`Rename`) to this replay; an acked-but-
  unflushed rename must reconstruct the same overlay it produced live (Design decision 5).
  Recovery stays **total over arbitrary device contents** (`:276`) — the new op's decode is
  bounded and fuzzed like the other two. No text change.
- **rev1§6 / §6.1(e) — the WAL structural-decode seam** (`:401`, `:419`): the per-record
  *structural decode* is *"split out of its hash wrapper and verified like the other on-disk
  decoders (§3.7)"* (the B7B work; the verified mirror is `s_payload_ok`/`e_payload_ok`/
  `wal_struct_ok`, `cas/src/store.rs:735,:873,:954`, with BLAKE3 the *"only part of the
  record seam left uninterpreted"*, `:419`). Adding `WalOp::Rename` **extends this verified
  decoder**: the spec mirror and its exec twin both grow a tag-3 arm, re-proving the ∀-bytes
  equality (Design decision 5). **Edit on landing:** rev1§6.1(e) and the trusted-base ledger
  Baselines row record the new `cargo verus verify -p cas --no-default-features` total
  (≥ 80, up from 80 by the tag-3 walk); the BLAKE3-only-trusted statement is unchanged
  (C2 adds **no** seam). This is the one spec/ledger touch.
- **rev1§4.7 — the on-disk format & versioning** (`:305-327`): B5 bumped `SB_VERSION` 3→4
  for the `edit_version` field (`cas/src/disk.rs:141`). C2's WAL gains a new op tag, a
  forward-only format change; **bump `SB_VERSION` 4→5** so a pre-C2 binary refuses a store
  that may carry tag-3 WAL records (the exact-match refuse at `disk.rs:368`) rather than
  silently dropping a renamed-but-unflushed write as a torn tail (Design decision 5). Same
  migration discipline as B5; coordinate the version number with B5's landing.
- **rev1§2.7 / §3.7 — decode discipline** (`spec_rev1.md:125-131`): both new decoders —
  the `WalOp::Rename` payload (`cas`) and the `Request::Rename` wire variant (storage-
  server, postcard) — are total over arbitrary bytes (refuse-not-crash), fuzzed alongside
  the existing WAL and request corpora. No text change; C2 conforms.

---

## What is actually true today — a path-keyed overlay, two WAL ops, a Rename-shaped hole

The inventory that shapes the phase.

### The overlay is path-keyed, with creates/deletes but no renames (`cas/src/overlay.rs`)

- **`Overlay`** (`:107-112`) holds `files: BTreeMap<Path, FileOverlay>` (the path **is** the
  key, `:109`), `unlinks: BTreeSet<Path>` (tombstones, `:110`), `bytes: usize` (the dirty-
  byte budget, `:111`), where `Path = Vec<Vec<u8>>` is a component list (`:16`).
- **`FileOverlay`** (`:18-26`) is the per-file interval map: `writes: BTreeMap<u64, Vec<u8>>`
  (offset → bytes, non-overlapping, last-write-wins via `insert`, `:67-94`), `fresh: bool`
  (*"the file was (re)created inside this overlay window"*, `:22-24`), and `mtime`.
- **Creates** are implicit (a `write` to an absent path creates the `FileOverlay`, `:123-
  133`); **unlink-then-write** resurrects as `fresh` (`:124-129`, the proptest at `:204-216`
  pins it). **Deletes** record a tombstone and drop the dirty bytes (`unlink`, `:135-141`).
  **Renames: none** — the deferral is stated outright at `:9-11`.
- **Listings** range-scan by path prefix: `files_in_dir(dir)` / `unlinked_in_dir(dir)`
  (`:162-178`) filter `files`/`unlinks` to direct children of `dir`. Re-keying must preserve
  this (Design decision 1 keeps a name-ordered index).
- The crate exposes the module (`cas/src/lib.rs:39 pub mod overlay;`); `storage-server`
  aliases the type (`use cas::overlay::Path as TreePath`).

### The `Store` is path-addressed; the overlay is reached only through it (`cas/src/store.rs`)

- **`Store`** (`:~1451`) holds `overlays: BTreeMap<Vec<u8>, Overlay>` (one per ref),
  `dirty_refs`, the WAL ring/records, and the committed `RefTable`. There is **no file
  handle** anywhere: every method names `(ref_name, path)`.
- **Write** (`:1980-2003`): builds a `WalOp::Write{ref_name,path,offset,mtime,data}`, calls
  `log_then_apply`, which appends to the WAL and `apply_to_overlay` (`:2311-2325`) →
  `overlay.write(path, …)`. **Unlink** (`:2010-2019`) → `WalOp::Unlink` →
  `overlay.unlink(path,…)`. **Read** (`:2331-2352`) consults `overlay.state(path)` first
  (Dirty/Unlinked/Clean) and falls through to `read_from_tree(root, path)`.
- **Flush** (`flush_ref`, `:2467-2556`) removes the ref's overlay, then **iterates by path**:
  for each tombstone `path` in `overlay.unlinks()`, `tree::remove(root, comps)`; for each
  `(path, fo)` in `overlay.files()`, `old = tree::lookup(root, comps)`, `content =
  fo.apply(old)`, `tree::put(root, dir, entry)`. The base for the interval-map apply is read
  from **the same path** — the assumption C2's rename breaks (Design decision 3).
- **Tree primitives** (`cas/src/tree.rs`): `lookup` (`:16`), `put` (`:42`, insert/replace,
  creates intermediate dirs), `remove` (`:83-87`, returns `(new_root, Option<Entry>)` — **the
  removed entry**, which carries `Content::{Inline|ChunkList|DirRoot}`). So a clean tree-
  level rename is `remove(from)` → reattach the returned `Entry` at `to` via `put` — O(depth),
  the rev1§4.9 detach/reattach, for files **and** directories (a dir's `DirRoot(h)` rides along).

### The WAL has two ops and a *verified* structural decoder (`cas/src/disk.rs`, `store.rs`)

- **`WalOp`** (`disk.rs:482-495`): `Write` and `Unlink` only. `encode_payload`/`decode_payload`
  (`:514-600`) are a tag-dispatch (tag 1 = Write, 2 = Unlink) + length-prefixed walk;
  `encode_record`/`decode_record` (`:613-645`) wrap them in a BLAKE3-checksummed,
  seq-bound frame.
- **The structural decode is verified** (the B7B/T-5 split). `store.rs:735-794` is the spec
  mirror `s_payload_ok` (tag 1/2 arms, `s_take`/`s_path` bounded walks); `:873` is the exec
  twin `e_payload_ok`, **proven equal ∀ bytes**; `:954` `wal_struct_ok` carries it into
  `recover_records` (`:1297`). The trusted-base ledger (`verus_trusted-base.md:125`) records
  `wal_checksum_ok` as *"the lone uninterpreted part of the record seam after B7B split the
  `WalOp` structural decode into the verified surface."* **Adding a WAL op extends this
  verified decoder** — the headline verification obligation of C2 (Design decision 5).
- **Baseline:** `cargo verus verify -p cas --no-default-features` → **80 verified, 0 errors**
  (ledger `:188`; the parent plan's "58/0" at `:48` is the pre-B-wave rev0 number).
  `SB_VERSION = 4` (`disk.rs:141`), exact-match refuse at `:368`. WAL fuzz targets
  `wal_replay_scan{,_fixup}.rs` (`cas/fuzz/fuzz_targets/`). Crash-injection tests use
  `crash_opts()` (`store.rs:2955`).

### The wire has a `Rename`-shaped hole (`storage-server/src/lib.rs`, `wire.rs`)

- **`Request`** (`:112-238`) has 23 variants after the B-wave — including B5's `Apply`
  (`:211-215`), `Tag` (`:220`), `Untag`, `ListTags` (`:235-237`) — and **no `Rename`**.
  `Write{handle,path,offset,data}` (`:119-124`) and `Unlink{handle,path}` (`:125-128`) are
  the file mutators; both are gated `R_WRITE` and ref-only (snapshots → `ReadOnly`) in
  `dispatch` (`:541-565`). `Response` (`:259-301`) has `Ok`/`Err(ErrorCode)`/`NotFound` etc.
- **The wire is postcard behind a fixed 3-byte header** `[0x45,0x51,0x02]` (`wire.rs:16`),
  strict (rejects trailing bytes), ≤ 256 bytes (`MAX_MSG`). B5 established the discipline:
  **append** new variants so existing discriminants stay stable and the header version does
  **not** bump (B5 detail, Design decision 1). C2 appends `Request::Rename` after `ListTags`.
  Request fuzz targets: `request_dispatch.rs`, `structured_request.rs`.
- **Cross-subtree confinement is already structural.** Every path-bearing request is
  `validate_path`'d (`:492-497`, rejecting `.`/`..`) then resolved under the handle's subtree
  by `full_path(subtree, path)` (`:484-488`). A `Rename` that `full_path`s **both** `from`
  and `to` cannot name anything outside the subtree — the rev1§4.9 *"unnameable and therefore
  denied"* property holds with no extra check (Design decision 4).
- **The shell** (`user/shell/src/runtime.rs`) has `rm` → `Request::Unlink{handle:0,path}`
  (`:578`) and `write` → `Request::Write` (`:607-617`); **no `mv`**. `parse_path` turns a
  shell arg into a component list. C2 adds `mv` (Design decision 4 / sub-phase C2D).

---

## Primary files (current line numbers)

- **`cas/src/overlay.rs`** — the re-keying core (C2A). `Overlay` `:107-112`, `FileOverlay`
  `:18-26`, `write` `:123-133`, `unlink` `:135-141`, `state` `:143-151`, `files`/`files_in_dir`/
  `unlinked_in_dir` `:153-178`, the deferral comment `:9-11`, the proptest `:218-254`.
- **`cas/src/store.rs`** — the `Store` glue + the verified WAL mirror. `overlays` field
  `:~1465`, `write` `:1980-2003`, `unlink` `:2010-2019`, `apply_to_overlay` `:2311-2325`,
  `read` `:2331-2352`, `flush_ref` `:2467-2556`, the WAL replay in `mount` `:1729-1736`,
  the verified mirror `s_take`/`s_path`/`s_payload_ok` `:699-794`, `e_payload_ok` `:873`,
  `wal_struct_ok` `:954`, `recover_records` `:1297`, `crash_opts` `:2955`,
  `wal_struct_ok_has_teeth` (`:2445`, per the ledger).
- **`cas/src/disk.rs`** — the WAL op + version. `WalOp` `:482-495`, `encode_payload` `:514-553`,
  `decode_payload` `:555-600`, `encode_record`/`decode_record` `:613-645`, `SB_VERSION` `:141`,
  the version refuse `:368`, `RefEntry` `:672-685`.
- **`cas/src/tree.rs`** — the rename primitive. `lookup` `:16`, `put` `:42-79`, `remove`
  `:83-115` (returns the removed `Entry`).
- **`cas/fuzz/fuzz_targets/`** — `wal_replay_scan.rs`, `wal_replay_scan_fixup.rs`: extend the
  corpus with tag-3 (Rename) records (C2B).
- **`storage-server/src/lib.rs`** — the wire op (C2D). `Request` `:112-238` (append after
  `ListTags` `:237`), `dispatch` `:499-924` (the new arm beside `Write`/`Unlink` `:541-565`),
  `full_path` `:484-488`, `validate_path` `:492-497`, the `R_WRITE` gate, `ErrorCode` `:305-320`.
- **`storage-server/src/wire.rs`** — header `:16` (**unchanged**), encode/decode `:27-47`,
  `MAX_MSG` 256.
- **`storage-server/fuzz/fuzz_targets/`** — `request_dispatch.rs`, `structured_request.rs`:
  cover `Rename` (C2D).
- **`user/shell/src/runtime.rs`** — the `mv` built-in (C2D). `dispatch` `:566-620`, `rm` `:578`,
  `write` `:607-617`, `parse_path`.
- **`doc/spec/spec_rev1.md`** — the single edit on landing (rev1§6.1(e) verify total `:419`).
- **`doc/guidelines/verus_trusted-base.md`** — the Baselines row (CAS verify total `:188`);
  **no seam row changes** (C2 adds no trusted seam).
- **`scripts/run-demo.sh`** — the QEMU smoke gate; C2D adds a `mv` witness.

---

## Verification tier & baseline (applies to all sub-phases)

C2 spans two tiers, and the split is the discipline (parent plan `:32-34`, *"no logic change
lands without its verification tier"*):

- **The overlay re-keying, `Store::rename`, the flush logic, and unlink-while-open are
  Baseline tier (Miri + proptest).** The applier and the in-memory maps stay plain Rust over
  the verified decision cores (rev1§6.1(e): *"the commit routine itself stays plain Rust over
  the verified decisions"*). The headline is the **rename/unlink interleaving proptest**
  (parent acceptance `:728`) against a path-keyed reference model, plus a **crash-injection**
  proptest (the `crash_opts` family) that an acked-unflushed rename replays. Both carry a
  **negative control** (a deliberately-wrong oracle must fail — the project's anti-theater
  habit). The existing overlay proptest (`interval_map_matches_model`, `overlay.rs:218-254`)
  and the whole crash-recovery family stay green across the re-key.
- **The new `WalOp::Rename` *decoder* is Verus tier (it joins the verified WAL structural
  decode) AND fuzzed.** This is the **one** place C2 touches the proof boundary: the spec
  mirror `s_payload_ok` and its exec twin `e_payload_ok` each grow a tag-3 arm, the ∀-bytes
  equality is re-proven, and `cargo verus verify -p cas --no-default-features` re-establishes
  ≥ 80/0 (higher, with the tag-3 walk). The `wal_replay_scan` fuzz corpus gains tag-3 seeds
  (rev1§3.7 "decoders get fuzz targets"). The `Request::Rename` wire variant is fuzzed via
  the existing request corpus. **No new seam:** the extension is *verified*, not trusted; the
  lone uninterpreted record-seam part stays BLAKE3 (`wal_checksum_ok`); the ledger tally is
  unchanged.
- **The `cargo fmt` workspace-split trap applies.** `cas`/`storage-server` format via the
  root; `user/shell` formats via its own manifest (`cargo fmt --manifest-path
  user/shell/Cargo.toml`); `cas/fuzz` and `storage-server/fuzz` via their fuzz manifests
  (CLAUDE.md "Formatting").

**Baseline to re-establish at end of C2:**

- `cargo test -p cas -p storage-server` green: the re-keyed overlay tests, the rename +
  interleaving + crash-injection proptests, the wire round-trip.
- `cargo verus verify -p cas --no-default-features` ≥ **80/0** (higher with the tag-3 decode
  arm); `-p kcore` / `-p dma-pool` and the three TLA models **unchanged** (C2 touches none).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p cas -j4` clean,
  including the new proptests (capped under `cfg(miri)` like the rest, `overlay.rs:219`).
- The `wal_replay_scan` and `request_dispatch` fuzz corpora replay UB-free under Miri
  (`--test fuzz_regressions --test fuzz_corpus`); tag-3 seeds added.
- A pre-C2 (`SB_VERSION = 4`) image is **refused** at mount with a clean error (not a panic),
  matching B5's refuse-old test; `mkfs` writes v5.
- **`scripts/run-demo.sh` boots green** under the CLAUDE.md timeout-harness: `[storaged] store
  mounted` → `serving`, and the new `mv` witness (`write a x; mv a b; cat b` → `x`; `ls`
  shows `b` not `a`) behaves, no panic/`Corrupt`.

---

## Design decision 1 — the overlay re-key: `id → name` indirection with a name-ordered index *(resolve in C2A)*

The overlay must key on an ephemeral file id (rev1§4.3/§4.9) while preserving everything the
path key does today: per-file interval maps, tombstones, the resurrect-on-write `fresh`
flag, and the prefix-scan listings.

- **Adopted — key the interval maps by `FileId`, keep a name-ordered index for resolution
  and listings, and a tombstone set.** The shape (finalize field names in C2A):
  ```
  type FileId = u64;                         // ephemeral, runtime-only; never on disk
  struct Overlay {
    by_id:   BTreeMap<FileId, FileOverlay>,  // the interval maps, re-keyed
    by_name: BTreeMap<Path, FileId>,         // current live names → id (range-scannable)
    names:   BTreeMap<FileId, Option<Path>>, // id → current name; None = unlinked-while-open
    tombs:   BTreeSet<Path>,                 // names to read-as-absent + remove from tree at flush
    bytes:   usize,
  }
  struct FileOverlay { writes: …, fresh: bool, mtime: u64,
                       origin: Option<Path> }   // base tree path to read pre-edit bytes (DD3)
  ```
  - **`FileId` allocator: one monotonic `u64` on the `Store`, store-global, never persisted.**
    Global (not per-ref) is simplest and sufficient — ids index a per-ref overlay but need no
    cross-ref meaning, and *"IDs never touch disk"* (rev1§4.9) so there is no allocation-order-
    in-the-hash hazard (the rev0§4.9 reason persistent inodes were rejected, `:463`). On WAL
    replay the counter restarts and ids are reassigned deterministically (Design decision 5).
  - **`by_name` is the name-ordered index** that replaces direct path-keying: `write`/`read`/
    `unlink` resolve `name → id` through it, and `files_in_dir`/`unlinked_in_dir` become range
    scans over `by_name`/`tombs` (the `overlay.rs:162-178` prefix filter, preserved exactly).
  - **`names` (`id → Option<Path>`) is the rev1§4.9 *"ID → current-path map."*** Rename
    updates it (and `by_name`) in O(1) — the interval map in `by_id` never moves. `None` marks
    an id whose name was unlinked while the id is still open (Design decision 2 / unlink-while-
    open, sub-phase C2C); a `None` id is discarded at flush (*"if at flush time the ID resolves
    to no path, the data is discarded"*).
  - **`tombs` is today's `unlinks` set, unchanged in role**: a `read` of a tombstoned name
    returns absent *before* flush (the tree still holds the old entry until flush removes it),
    and flush issues `tree::remove` for each. A write to a tombstoned name resurrects it
    (`fresh = true`), exactly as `overlay.rs:124-129` does now.
  - **Flush re-keys its iteration**: walk `by_id`; for each id with `names[id] = Some(name)`,
    resolve base from `origin` (DD3), apply, `tree::put` at `name`, and if `origin != name`
    (a rename happened) `tree::remove` the old `origin`; for `names[id] = None`, discard (and
    `tree::remove` `origin` if it was a committed file); then process `tombs`.
- **Rejected — keep path-keying and add a side "rename map" path→path.** Renaming a file with
  a large dirty interval map would still mean re-keying that map (move the `BTreeMap` entry),
  which is the O(dirty) cost rev1§4.9 forbids; and a chain of renames would compose path
  rewrites. The id indirection is the *point* — one pointer update regardless of dirty volume.
- **Rejected — per-ref `FileId` spaces.** Works, but adds a counter and a reset per ref for no
  benefit; one store-global counter is less state and the ids are opaque.

**Recommendation: re-key the interval maps by a store-global ephemeral `FileId`; keep
`by_name` (name-ordered, for resolution + prefix-scan listings), `names` (the rev1§4.9
`id → current-name` map), and `tombs` (today's `unlinks`); flush walks `by_id` and resolves
`id → name`.**

---

## Design decision 2 — the file-id surface: server-internal ids + a Store-level open handle; reserve client fds *(resolve across C2)*

rev1§4.9 describes an *"open handle [that] keeps working against the overlay"* after an
unlink. Realizing that needs a handle decoupled from the path; the question is whether that
handle is **client-visible** (a wire `Open`/file-descriptor op) or **server-internal** (a
`Store` file id the proptest holds).

- **Adopted — the file-id layer is server-internal; the rev1§4.9 open handle is realized at
  the `Store` API; the wire gains only `Request::Rename`.**
  - The `Store` grows id-addressed methods *alongside* the path-addressed ones:
    `open(ref, path) -> FileId`, `write_id(id, off, data, mtime)`, `read_id(id, …)`, and the
    existing path-addressed `write`/`read`/`unlink` (which resolve `name → id` internally,
    allocating on first touch). The path-addressed wrappers are what the wire and the shell
    use; the id-addressed methods are what the **rename/unlink interleaving proptest** (C2C)
    drives to hold a file open *across* an unlink — exactly the rev1§4.9 semantics, proven
    where the proptest lives.
  - **The wire stays path-addressed** (the openat-shaped namespace, rev1§4.9 `:351`) and gains
    only `Request::Rename` (the parent plan's one named op, `:716` *"the storage-server
    `Request` enum (no rename op)"*). No client today holds a file open across an unlink, so a
    client-visible `Open`/fd surface would be speculative.
  - **Honest scope:** C2 satisfies all three acceptance criteria (`:728`) *at the `Store`
    level* — rename is O(1) over dirty state, open handles (Store file ids) follow renames,
    unlink-while-open behaves per rev1§4.9. A **remote client** observing unlink-while-open
    (POSIX-fd flavor) is the reserved follow-on (Out of scope).
- **Rejected — a full client-visible fd protocol now** (`Open`/`Close`/`Read`/`Write` by file
  id on the wire). It is real protocol surface (≥4 new ops + a per-session open-file table +
  the shell rewired off path-addressed I/O), speculative (no consumer), and against the grain
  of the openat namespace. Reserve it; the `Store`-level mechanism is forward-compatible (a
  future `Open` op just returns the same `FileId`).
- **Rejected — fd-only ops** (drop path-addressed write/read). Rewrites the shell and every
  caller for no MVP gain; the common case *is* path-addressed.

**Recommendation: keep file ids server-internal; realize the rev1§4.9 open handle at the
`Store` API (`open`/`write_id`) where the proptest proves unlink-while-open; add only
`Request::Rename` to the wire; reserve a client-visible fd surface (Out of scope).**

---

## Design decision 3 — renamed-dirty-file base origin: track where to read pre-edit bytes *(resolve in C2B)*

Today flush reads a dirty file's base content from **the same path** it writes back to
(`flush_ref`: `old = tree::lookup(root, comps)` then `tree::put(root, dir, …)`,
`store.rs:2467-2556`). A rename breaks that identity: a non-`fresh` file renamed `a → b`
has its committed base at `a` but must be written at `b`.

- **Adopted — the `FileOverlay` records an `origin: Option<Path>`: the committed-tree path to
  read pre-edit bytes from, fixed at first-write, distinct from the current name.**
  - On the first write to a name with a committed base, `origin = Some(that name)`; on a
    `fresh` create (write after unlink, or a brand-new file), `origin = None` (no base — the
    apply starts from empty, `overlay.rs:49-53`).
  - **Rename moves the current name (`by_name`/`names`), never `origin`.** So after `a → b`,
    the id's `origin` stays `a` and its current name is `b`.
  - **Flush** reads base from `origin` (unless `fresh`), applies the interval map, `tree::put`s
    at the **current name**, and if `origin` is `Some` and `≠` current name, `tree::remove`s
    the old `origin` (the rename's source disappears from the tree). For a `None` current name
    (unlinked-while-open, DD2/C2C), discard and `tree::remove` `origin` if it was committed.
  - This keeps the write path lazy — no base I/O at rename time, only at flush, exactly as
    today (rev1§4.3: re-chunk the affected neighborhood at flush, not on every write).
- **Rejected — materialize the base into the overlay on rename** (read `a`, store full content
  under the id). Defeats the laziness rev1§4.3 buys and turns an O(1) pointer update into an
  O(file) read on the write path.
- **Rejected — eagerly move the base in the tree on rename** (`tree::remove(a)` + `tree::put(b)`
  at rename time). Puts tree I/O and a path-copy on the rename request path; flush already
  does the path-copy once, batched (rev1§4.3 `:240`). Defer the tree move to flush.

**Recommendation: add `origin: Option<Path>` to the per-file overlay; rename moves the name,
not the origin; flush reads base from origin, writes to the current name, and removes the old
origin from the tree when they differ.**

---

## Design decision 4 — rename semantics: within-subtree only, directories by detach/reattach, overwrite is last-write-wins *(resolve in C2B/C2D)*

The op's boundaries, all anchored in rev1§4.9.

- **Adopted:**
  - **Cross-subtree target denial is structural and free.** The wire `Rename{handle,from,to}`
    `validate_path`s both (`..` rejected, `lib.rs:492-497`) and `full_path`s both under the
    handle's subtree (`:484-488`); a target outside the subtree is **unnameable**, satisfying
    rev1§4.9 *"rename targeting outside a subtree handle is unnameable and therefore denied"*
    with no extra check — the same confinement `OpenChild` relies on.
  - **Directories rename by detach/reattach (rev1§4.9 `:337`).** `tree::remove(from)` returns
    the removed `Entry` (`tree.rs:83-115`) — for a directory that is `Content::DirRoot(h)` —
    and `tree::put(to_parent, entry_with_new_name)` reattaches it: O(depth), the spec's literal
    *"detach a hash, reattach it."* Files work identically (`Content::Inline`/`ChunkList`).
  - **A directory with *dirty descendants* flushes the ref first, then does the clean tree
    move.** The simple, correct MVP: a directory rename forces a synchronous flush of the ref
    (joining the rev1§4.4 explicit-flush family alongside `sync`/`snapshot`), draining the
    overlay so the move is a pure tree detach/reattach with no dirty-prefix re-pathing. The
    optimization — prefix-update every `by_name`/`origin` entry under the old prefix (O(dirty
    descendants)) and skip the flush — is recorded as a follow-on, not built now.
  - **Rename onto an existing target is last-write-wins:** unlink the destination id first
    (mirroring the overlay's resurrect-on-write LWW, rev1§4.4 `:255`, `overlay.rs:124-129`),
    then attach the source there.
- **Rejected — refuse directory rename.** rev1§4.9 makes O(depth) directory moves a *feature*
  (subtree caps move with the subtree); refusing them is a real regression from the spec.
- **Rejected — cross-ref rename in this op.** rev1§4.9 (`:349`): *"Rename across refs is a copy
  with new lineage"* — a different operation (two handles, a content copy) overlapping B5; a
  single-handle `Rename` cannot express it (Out of scope, scope note 3).

**Recommendation: within-ref, within-subtree rename only (cross-subtree unnameable by
construction); directories by `tree::remove`+`tree::put` detach/reattach; a dirty-descendant
directory rename flushes the ref first (prefix-update optimization deferred); overwrite is
last-write-wins.**

---

## Design decision 5 — `WalOp::Rename` (tag 3): extend the verified decoder; rederive ids on replay; bump `SB_VERSION` *(resolve in C2B)*

An acknowledged-but-unflushed rename must replay (rev1§4.5 `:274`). The rename touches only
in-memory maps live, so without a WAL record a crash would lose it (the bytes would replay
under the *old* name). This is the one change that reaches the verified surface.

- **Adopted — a third WAL op, decoded by the *verified* mirror, with ids rederived on replay
  and the on-disk version bumped.**
  - **`WalOp::Rename { ref_name, from: Vec<Vec<u8>>, to: Vec<Vec<u8>>, mtime }`** as tag 3.
    Extend `encode_payload`/`decode_payload` (`disk.rs:514-600`) with a tag-3 arm (two
    length-prefixed paths + `mtime`), and `ref_name`/`mtime` accessors (`:498-512`).
  - **Extend the verified structural decoder.** Add a tag-3 arm to the spec mirror
    `s_payload_ok` (`store.rs:735-794`, two `s_path` walks) and the exec twin `e_payload_ok`
    (`:873`, two `e_path` walks), and **re-prove `e_payload_ok == s_payload_ok` ∀ bytes** —
    the `decode_frame`/`wal_struct_ok` shape is unchanged, only the payload arm grows.
    Re-establish `cargo verus verify -p cas --no-default-features` ≥ 80/0 (higher). This keeps
    the rev1§6.1(e) property true: the record's **structural** decode is verified; BLAKE3 stays
    the only uninterpreted part. **No new seam** — `wal_checksum_ok` is untouched.
  - **Ids are rederived on replay, so the WAL stays path-keyed.** Replay (`mount` →
    `recover_records` → `apply_to_overlay`) processes records in seq order: `Write{path}`
    allocates/looks up an id for `path`; `Rename{from,to}` does the O(1) map swap;
    `Unlink{path}` tombstones. The id ↔ name maps that existed live are reconstructed exactly,
    because the records are deterministic (paths + server-assigned `mtime` captured in the
    record, `disk.rs:479-481`). Ids never persist (rev1§4.9), so the restart counter is fine.
  - **`apply_to_overlay`** (`store.rs:2311-2325`) gains a `WalOp::Rename` arm calling
    `overlay.rename(from, to, mtime)` — the single code path shared by live ops and replay
    (today's invariant, preserved).
  - **Bump `SB_VERSION` 4 → 5** (`disk.rs:141`). A store may now carry tag-3 WAL records; the
    exact-match refuse (`:368`) makes a pre-C2 binary reject such a store cleanly at mount
    rather than decode a tag-3 record as a torn tail and **silently drop** a renamed-unflushed
    write. `mkfs` writes v5; a v4 image is refused with a clean error (the B5 refuse-old test
    pattern). Coordinate the number with B5's v4 landing.
- **Rejected — log rename as a `Write`+`Unlink` pair (no new op).** Non-atomic on replay — a
  crash between the two records leaves a half-rename (content at both names, or neither) — and
  it loses the dirty file's *identity* (the new-name write would lose the old name's
  unflushed interval map). A single atomic `Rename` record replays as one map swap.
- **Rejected — leave the WAL structural decode trusted for the new op.** Re-widens the trusted
  record seam that B7B deliberately shrank (rev1§6.1(e)); the verified mirror is cheap to
  extend (one more bounded arm) and keeps the honesty win.

**Recommendation: add `WalOp::Rename` (tag 3) decoded by the extended *verified* mirror
(re-prove ∀-bytes equality, ≥ 80/0, no new seam); rederive ids on replay so the WAL stays
path-keyed; bump `SB_VERSION` 4→5 with refuse-old.**

---

## Sub-phase C2A — re-key the overlay on `FileId` *(must-do; the foundation; no behavior change)*

The overlay re-keying (Design decision 1) with **identical** external behavior: no rename, no
wire change, no new semantics — `write`/`unlink`/`read`/`flush`/listings produce the same
results, now routed through `name → id → interval-map`. A pure internal refactor behind the
`Store` API that unblocks C2B/C2C.

- **Touches:** `cas/src/overlay.rs` — re-shape `Overlay` to `by_id`/`by_name`/`names`/`tombs`
  (DD1), add `FileOverlay.origin` (DD3, set but inert until rename exists), rewrite
  `write`/`unlink`/`state`/`files`/`files_in_dir`/`unlinked_in_dir` over the indices; replace
  the `:9-11` deferral comment with the realized design. `cas/src/store.rs` — the `FileId`
  allocator on `Store`; `flush_ref` (`:2467-2556`) iterates `by_id` and resolves `id → name`
  (DD1/DD3); `apply_to_overlay`/`read` resolve through `by_name`. **No** `disk.rs` change
  (WAL unchanged), **no** Verus change (the verified decoder is untouched).
- **Depends on:** Part A blessed (rev1§4.3/§4.9 text). No intra-C2 dependency.
- **Work:**
  1. Re-shape `Overlay` + `FileOverlay` (DD1/DD3); the `FileId` allocator on `Store`.
  2. Route `write`/`unlink`/`state` through `by_name`; preserve `fresh`-on-resurrect and the
     `bytes` budget arithmetic byte-for-byte (`overlay.rs:131-138`).
  3. Re-key `flush_ref` iteration (`by_id` + `id → name` resolution); `origin == name` for
     every file (no rename yet), so flush behaves identically.
  4. Preserve listings as range scans over `by_name`/`tombs`.
  5. Port the overlay proptest (`interval_map_matches_model`, `:218-254`) and add an id-level
     invariant check (every `by_name` value is a live `by_id` key; every `Some(name)` in
     `names` round-trips through `by_name`).
- **Acceptance:**
  - `cargo test -p cas` green; the overlay proptest and the **entire crash-recovery family**
    pass unchanged (the re-key is behavior-preserving).
  - `cargo verus verify -p cas --no-default-features` = 80/0 (unchanged — no WAL/decode change).
  - `scripts/run-demo.sh` boots green (no observable behavior change); Miri sweep clean.
- **Effort/Risk:** M / low–medium. The substance is preserving the budget/`fresh`/listing
  semantics exactly through the indirection; the surface is one module + flush.

---

## Sub-phase C2B — `Store::rename` + `WalOp::Rename` + the verified-decode extension + crash recovery *(must-do; the headline; the only verified-surface touch)*

The rename mechanism end to end: O(1) dirty rename via the map swap (DD1), base-origin
handling at flush (DD3), the atomic WAL op decoded by the extended **verified** mirror (DD5),
and crash recovery of an acked-unflushed rename. The highest-risk sub-phase (it reaches the
proof boundary and the on-disk version).

- **Touches:** `cas/src/overlay.rs` — `fn rename(&mut self, from, to, mtime)` (the O(1) swap +
  LWW-overwrite, DD4). `cas/src/disk.rs` — `WalOp::Rename` tag 3 (DD5), `encode_payload`/
  `decode_payload` arms, accessors, `SB_VERSION` 4→5. `cas/src/store.rs` — `pub fn rename(&mut
  self, ref_name, from, to, mtime)` (log-then-apply, the dirty-descendant-directory flush-
  first, DD4); the `apply_to_overlay` Rename arm; `flush_ref` base-origin handling (DD3); the
  **verified mirror** tag-3 arms in `s_payload_ok`/`e_payload_ok` + the re-proof. `cas/fuzz/
  fuzz_targets/wal_replay_scan*.rs` — tag-3 seeds. `mkfs/src/main.rs` — writes v5 (via
  `format`).
- **Depends on:** C2A. (Independent of C2C/C2D.)
- **Work:**
  1. `Overlay::rename` (O(1): `by_name`/`names` swap, `origin` untouched, destination LWW).
  2. `WalOp::Rename` + codec; extend the verified `s_payload_ok`/`e_payload_ok` tag-3 arms and
     re-prove the ∀-bytes equality (re-run `cargo verus verify`).
  3. `Store::rename`: log-then-apply (shared replay path); the dirty-descendant-directory
     flush-first (DD4); `apply_to_overlay` Rename arm.
  4. `flush_ref` base-origin: read base from `origin`, write at current name, `tree::remove`
     the old `origin` when it differs (DD3).
  5. `SB_VERSION` 4→5 + the refuse-old mount test; `mkfs`/`format` write v5.
  6. **Crash-injection proptest** (the `crash_opts` family, `store.rs:2955`): a sequence
     ending in an acked-unflushed rename, crash, replay → the recovered overlay equals the
     live one (a rename reattaches the dirty bytes under the new name). **Negative control:** a
     replay that drops the Rename record must make the equality fail.
  7. An **O(1)-witness** test: a rename of a file with a large dirty interval map performs a
     constant number of map operations (no interval-map move) — assert via instrumentation or
     by structural argument in the test (the `by_id` entry object identity is unchanged).
- **Acceptance:**
  - Rename is O(1) over dirty state (the witness test); a renamed dirty file flushes correctly
    (base from origin, written at the new name, old name gone from the tree).
  - The crash-injection proptest passes; its negative control fails.
  - `cargo verus verify -p cas --no-default-features` ≥ 80/0 (higher with the tag-3 walk);
    `wal_replay_scan` fuzz covers tag 3 and replays UB-free under Miri.
  - A v4 image is refused cleanly at mount; `mkfs` produces v5.
- **Effort/Risk:** M–L / medium. The Verus re-proof (one more bounded arm in a proven walk)
  and the on-disk version bump are the risk; the patterns exist (tags 1/2, B5's v3→v4).

---

## Sub-phase C2C — unlink-while-open semantics + the rename/unlink interleaving proptest *(must-do; the rev1§4.9 semantic)*

The rev1§4.9 *"open handle keeps working … data discarded at flush if the ID resolves to no
path"* semantic, realized at the `Store` API (DD2) and proven by the headline interleaving
proptest (parent acceptance `:728`).

- **Touches:** `cas/src/store.rs` — id-addressed `Store` methods (`open(ref,path) -> FileId`,
  `write_id`/`read_id`); `unlink` of an *open* id transitions `names[id] = None` (keep the
  `by_id` overlay, drop the name from `by_name`/`by_name`-resolution, add the old name to
  `tombs` so reads see absent and flush removes the tree entry); flush discards `None`-named
  ids (DD1). `cas/src/overlay.rs` — the open-holder bookkeeping (which ids are open) so a
  closed-and-unnamed id is reaped. The headline proptest (in `store.rs` or `overlay.rs`).
- **Depends on:** C2A (the id mechanism). Independent of C2B (no WAL/rename change needed —
  pure in-memory + flush-discard), though naturally sequenced after it.
- **Work:**
  1. `Store::open`/`write_id`/`read_id` (DD2): the id-addressed surface the proptest drives.
  2. Unlink-while-open: an open id whose name is unlinked keeps accepting `write_id`, reads via
     the id keep working against the overlay, and at flush — `names[id] = None` — the data is
     **discarded** (never written to the tree), per rev1§4.9.
  3. The **interleaving proptest**: random sequences of create/write/rename/unlink/open/close/
     flush over a small fixed path set, checked against a reference model (a path-keyed naive
     semantics with explicit open-handle tracking — the model the indirection optimizes).
     Run under `cfg(miri){4..}` caps like the rest. **Negative control:** a model that forgets
     to discard an unlinked-while-open id (or to follow a rename) must make the test fail.
  4. Confirm crash-recovery interaction: an open-then-unlinked id has no client across a crash
     (ids are ephemeral); replay of the underlying `Write`+`Unlink` records reproduces the
     discard (no special replay logic — falls out of C2B's path-keyed records).
- **Acceptance:**
  - Unlink-while-open behaves per rev1§4.9 (an open id keeps working; its data is discarded at
    flush when it resolves to no name); a renamed open id follows the rename.
  - The rename/unlink interleaving proptest is green under Miri; its negative control fails.
- **Effort/Risk:** M / medium. The care is in the discard-at-flush bookkeeping and a reference
  model with teeth.

---

## Sub-phase C2D — wire `Request::Rename` + dispatch + `mv` shell + QEMU smoke *(must-do; the client surface)*

Surface rename over a session and in the shell; the cross-subtree denial is structural (DD4).
Mechanical once C2B exists.

- **Touches:** `storage-server/src/lib.rs` — append `Request::Rename { handle, from: TreePath,
  to: TreePath }` after `ListTags` (`:237`); a `dispatch` arm (`:499-924`) gated `R_WRITE`,
  ref-only (snapshot handle → `ReadOnly`), `validate_path` + `full_path` both paths (cross-
  subtree unnameable), calling `store.rename(name, full_path(from), full_path(to), now)`.
  `storage-server/fuzz/fuzz_targets/{request_dispatch,structured_request}.rs` — cover `Rename`.
  `user/shell/src/runtime.rs` — a `mv` built-in (`:566-620`): `mv from to` →
  `Request::Rename{handle:0, from: parse_path(from), to: parse_path(to)}`, and add `mv` to the
  `help` string (`:572-574`). `storage-server/src/wire.rs` — **unchanged** (header stays
  `[0x45,0x51,0x02]`; the append keeps discriminants stable, the B5 discipline).
- **Depends on:** C2B (`Store::rename`). Independent of C2C.
- **Work:**
  1. Append `Request::Rename` (postcard discipline — discriminant-stable, header unchanged;
     **coordinate with B5**: B5's `Apply`/`Tag`/`Untag`/`ListTags` already occupy their
     discriminants, so `Rename` appends after them; if C2 and B5 ever land out of the
     Wave-2→Wave-6 order, the only rule is *append, never reorder*).
  2. The dispatch arm: `R_WRITE` gate (the `Write`/`Unlink` precedent `:541-565`), ref-only,
     both paths validated + subtree-scoped (DD4), map `StoreError` → `ErrorCode` as the
     siblings do.
  3. The `mv` shell built-in + `help` text; a `report`-style reply like `rm` (`:578`).
  4. Extend the request fuzz corpus with `Rename`; promote any crash to a regression test.
  5. The `scripts/run-demo.sh` witness: `write a x; mv a b; cat b` → `x`; `cat a` → absent;
     `ls` shows `b` not `a`; `mv` of a missing source fails cleanly.
- **Acceptance:**
  - `cargo test -p storage-server` green (wire round-trips `Rename`; dispatch gates/denies
    correctly); a cross-subtree target is unnameable (structural — no path expresses it).
  - `scripts/run-demo.sh` boots green and the `mv` witness behaves; the request fuzz corpus
    (incl. `Rename`) replays UB-free under Miri.
  - B5's variants and the storage wire header are unaffected.
- **Effort/Risk:** S–M / low. Mechanical given C2B; the value is the end-to-end witness.

---

## Execution order

```
C2A  re-key overlay on FileId            [foundation; no behavior change]
       │
       ├─► C2B  Store::rename + WalOp::Rename + verified decode + crash recovery
       │         │
       │         └─► C2D  wire Request::Rename + dispatch + mv shell + QEMU smoke
       │
       └─► C2C  unlink-while-open + rename/unlink interleaving proptest
```

**C2A is the prerequisite.** **C2B and C2C both depend only on C2A and are mutually
independent** (C2B is the WAL/rename/verified-decode track; C2C is the in-memory open-handle
semantics). **C2D depends on C2B** (it surfaces `Store::rename`). C2C can land in parallel
with C2B/C2D. C2 as a whole depends only on Part A being blessed and coordinates with B5 (the
append discipline + the `SB_VERSION` number).

The cleanest **landing discipline**: C2A behind the `Store`/overlay API first (full green, no
behavior change, Verus still 80/0); then C2B (the verified-decode extension + version bump,
re-run `cargo verus verify` and the crash-injection proptest before moving on); then C2D
(the wire op + `mv`, re-run `scripts/run-demo.sh`); C2C any time after C2A. The single spec
edit (rev1§6.1(e) verify total) and the ledger Baselines-row update land with C2B (the
verified-surface change); no seam row changes.

## Out of scope for C2 (recorded so it is not mistaken for a gap)

- **A client-visible open/file-descriptor wire surface.** C2 realizes the rev1§4.9 open
  handle at the `Store` API (where the proptest proves unlink-while-open) and adds only
  `Request::Rename` to the wire (Design decision 2). A remote client holding a file open
  across an unlink (POSIX-fd flavor) is a forward-compatible follow-on — the `Store` `FileId`
  is the socket a future `Open` op returns.
- **Cross-ref rename (a copy with new lineage).** rev1§4.9 (`:349`) defines it as a *copy*,
  not a move; it needs two handles and a content-copy path overlapping **B5**, and a single-
  handle `Rename` cannot express it (scope note 3).
- **The dirty-descendant directory-rename prefix-update optimization.** C2 flushes the ref
  first for a directory rename with dirty descendants (Design decision 4, simple + correct);
  the O(dirty-descendants) prefix-update of `by_name`/`origin` that would skip the flush is a
  recorded optimization, not built now.
- **Persistent inodes / hard links.** rev1§4.9 (`:463`) rejects them outright — *"runtime file
  identity is provided by ephemeral server-side IDs instead."* C2's ids never touch disk; it
  adds no persistent identity layer.
- **Any Verus over the rename *applier* / flush logic.** The applier stays plain Rust over the
  verified decision cores (rev1§6.1(e)); only the WAL *decode* is verified. C2 adds **no new
  trusted seam** (the decode extension is verified, not trusted; the ledger tally is
  unchanged) and **no** TLA/Loom (the storage model is unchanged — the new op replays through
  the existing WAL machinery).
- **Shell niceties beyond `mv`** (recursive move, `-i`, completion). C2 adds the one `mv`
  built-in for the smoke witness; richer UX is shell-level later work.
