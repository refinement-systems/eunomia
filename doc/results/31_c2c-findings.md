# C2C — unlink-while-open semantics + the open-handle interleaving proptest

Phase **C2C** of `doc/plans/18_c2-detail.md` (the detailed decomposition of
parent-plan C2, `doc/plans/0_address_audit_rev0.md:714-729`). C2C realizes the
rev1§4.9 **open handle** at the `Store` API and proves it against a path-keyed
reference model. It builds only on **C2A** (the `FileId` re-key, already merged
as PR #170); it is independent of **C2B** (`Store::rename` + `WalOp::Rename`),
which is not yet built.

The normative target (`doc/spec/spec_rev1.md:349`):

> **File identity at runtime.** … Unlink-while-open: the open handle keeps
> working against the overlay, but if at flush time the ID resolves to no path,
> the data is discarded — which is what unlink means here.

C2A had already seeded the hook: `Overlay.names: BTreeMap<FileId, Option<Path>>`,
with `None` "reserved for C2C, never produced in C2A." C2C is what produces it.

## What landed

- **`cas/src/overlay.rs`** — the open-handle bookkeeping. One new field,
  `open: BTreeMap<FileId, u32>` (per-id handle refcount), and:
  - `open(path, next_id) -> FileId` — resolve or allocate an id for `path` and
    bump its refcount. Allocation registers `by_name`/`names` but **no `by_id`**:
    an opened-but-unwritten file holds no dirty data.
  - `close(id) -> bool` — decrement; on the last close reap by state: an orphaned
    id (`names[id] == None`) discards its data; an opened-but-never-written name
    reverts to the tree; a dirty named id is kept to flush. Returns "fully closed."
  - `write_orphan`/`read_orphan` — the nameless-handle write/read against an
    **empty base** (its tree path is gone). Named handles instead route through
    the existing path-addressed `write`/`read`, so they stay WAL-durable.
  - `carry_open(self) -> Option<Overlay>` — at flush, produce a fresh overlay
    carrying only the open handles' id↔name bindings and refcounts, with no dirty
    data. This is how a handle survives a flush.
  - `unlink` now **orphans** an open id (`names[id] = None`, keep `by_id`) instead
    of reaping it; `write`/`state`/`files`/`files_in_dir` learned to tolerate a
    `by_name` entry with no `by_id` (the opened-but-unwritten state);
    `check_invariants` gained the open/orphan invariants.
- **`cas/src/store.rs`** — the id-addressed surface (Design decision 2) plus the
  flush carry. New field `open_files: BTreeMap<FileId, Vec<u8>>` (id → ref_name,
  routing only); new `StoreError::NoSuchHandle`; methods `open`/`write_id`/
  `read_id`/`close`; `flush_ref` re-seeds surviving handles via `carry_open`.
- **Tests** — six unit tests, two negative controls, and the headline
  `rename_unlink_interleaving` proptest, all in `store.rs`.

No `disk.rs`/`tree.rs`/Verus/`storage-server`/`shell`/`mkfs` change. `SB_VERSION`
stays **4**; the verified WAL decoder is untouched, so `cargo verus verify -p cas
--no-default-features` stays **80/0**. No spec edit, no ledger change, no new seam.

## Three insights that shaped the implementation

### 1. Discard-at-flush falls out for free

The rev1§4.9 discard needs **no special flush code**. `flush_ref` already walks
`overlay.files()`, which iterates `by_name → by_id`. An orphaned id is removed
from `by_name` at unlink (only `names[id] = None` and the `by_id` data remain), so
`files()` simply never yields it — its data is dropped when the overlay is
consumed. The only flush change C2C needed was the *opposite* concern: keeping
**open** handles alive across the flush (below).

### 2. Named handles must survive flush — because flushes happen unbidden

`log_then_apply` can flush mid-write under WAL or per-ref byte/op-count
backpressure (`store.rs:2080-2124`). So "the open handle keeps working" cannot mean
"until the next flush" — a single large write through a handle can trigger one. A
flush therefore re-seeds the open handles onto a fresh, data-free overlay
(`carry_open`): a named handle's data is committed to the tree and a later
`write_id` re-materializes over the now-committed base; an orphaned handle keeps
its (empty, post-discard) binding so it still "works," writing data that the next
flush discards again. `carry_open` returning `None` (nothing open) reproduces the
exact C2A behavior — the ref's overlay is simply gone.

### 3. Orphan writes are ephemeral, and that makes crash recovery automatic

A `write_id` to a **named** handle delegates to the durable path-addressed `write`
(one shared WAL/replay path; the id resolves to the same `by_name` entry). A
`write_id` to an **orphaned** handle has no path, so there is nothing to log — the
data is ephemeral by construction. The payoff is work item 4: an open-then-
unlinked id has no client across a crash (ids never persist), and replaying the
acked `Write{path}` + `Unlink{path}` records reproduces the *path-visible* state
(absent) with **no special replay logic** — on replay there are no open handles,
so `Overlay::unlink` takes its reap branch, exactly as the path-keyed records say.
The orphan's overlay-only data is correctly gone. (`unlink_while_open_survives_crash`
exercises this, including a post-unlink ephemeral write, through a real `CrashDev`.)

## A deliberate semantic call: orphan reads against an empty base

rev1§4.9 frames the handle as working "against the overlay." C2C takes that
literally: an orphaned handle reads its overlay writes over an **empty** base, not
over the soon-to-be-removed committed bytes. So `open(f)` [committed `"BASE"`],
`write_id(4,"xx")`, then `unlink(f)` makes `read_id` return `"\0\0\0\0xx"`, not
`"BASExx"` — the committed base is logically gone the moment the name is unlinked.
This keeps the overlay lazy (no base materialization at unlink, the cost
Design decision 3 rejects for rename) and is internally consistent; the reference
model mirrors it exactly. A POSIX-fd flavor that preserves the inode's bytes across
unlink would require materializing the base at unlink and is left to the reserved
client-fd follow-on (Design decision 2, Out of scope).

## The interleaving proptest and its model

`rename_unlink_interleaving` drives random `open`/`write`/`write_id`/`unlink`/
`close`/`flush` sequences over a small fixed name set against `OpenModel` — a
path-keyed naive store with explicit handle tracking: whole-byte files (no interval
maps), a plain `committed` map (no chunk store), and base read **lazily** at read
time so an orphaned id naturally reads over empty. After every op it cross-checks
all path reads, all live-handle reads, and `Overlay::check_invariants`; at the end
it syncs and re-reads for durability. Real `FileId`s and model ids are paired
explicitly (not assumed equal), so only observable reads are compared.

The op set is named for the family but omits `rename` — `Store::rename` is C2B.
**When C2B lands, add a `Rename` arm** to both `OpenOp` and `OpenModel` (the model
already keys files by id, so rename is a one-line name swap) to complete the
"rename/unlink interleaving" coverage the plan names.

Two pitfalls the proptest surfaced:
- **`no_autoflush_opts`** pins all flush triggers high so flushes happen only at
  explicit `Flush` ops (which the model mirrors); an unbidden auto-flush would
  discard an orphan's data and diverge from the model. The first attempt set
  `wal_len == device size` and hit `DeviceTooSmall`; 64 KiB is enough to avoid WAL
  wrap for ~50 tiny records while leaving room for the chunk region.
- A local named `real` collides with a glob-imported Verus type
  (`vstd::prelude::real`) in `store.rs`; the negative-control variable is `got`.

**Negative controls** (anti-theater): `negative_control_flush_keeps_orphan` and
`negative_control_unlink_reaps_open` assert the real behavior diverges from a
wrong oracle (keep-orphan-at-flush; reap-on-unlink) — the two failure modes the
model must not have.

## Verification

- `cargo test -p cas` — **138 green** (119 lib incl. 9 new C2C tests/proptests, 9
  + 10 integration/fuzz-regression). All existing overlay/crash-recovery tests
  pass unchanged: the re-key is behavior-preserving when nothing is open.
- `cargo verus verify -p cas --no-default-features` — **80 verified, 0 errors**
  (unchanged; C2C touches no decoder).
- `cargo build -p storage-server -p mkfs -p cas` — clean (the new `StoreError`
  variant breaks no match).
- `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri nextest run -p cas -j4`
  — clean, including the new `cfg(miri)`-capped proptests.
- `scripts/run-demo.sh` — boots green; C2C adds no wire/shell surface, so the
  QEMU smoke is a pure regression check.

## Out of scope (unchanged from the plan)

Wire `Request::Rename` + `mv` shell (C2D); `Store::rename`/`WalOp::Rename` and any
Verus/`SB_VERSION` change (C2B); a client-visible open/fd wire surface and the
POSIX base-preservation it implies; cross-ref rename.
