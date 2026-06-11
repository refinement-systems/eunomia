# Fuzzing findings

Findings surfaced while standing up the cargo-fuzz harnesses. They were
initially recorded here unfixed (only the harnesses were in scope) and pinned
by `#[should_panic]` reproducers; **both are now fixed**, and the reproducers
were flipped into positive regression tests that assert the hardened behavior
(rejection, not panic).

Both confirmed findings are the same class the harness profile is built to
catch: **arithmetic on an untrusted length/offset** that traps under
`overflow-checks` (and would silently *wrap* without them).

---

## OVL-1 — `Write` at a near-`u64::MAX` offset panics the storage server

**Severity:** medium (remote DoS by an authorized writer).
**Where:** `cas/src/overlay.rs:55`, `FileOverlay::insert`:
`let end = off + data.len() as u64;`.
**Reached from:** `storage-server` `Request::Write { offset, data }` →
`Store::write` → overlay; no offset bound is checked on the path.
**Found by:** `storage-server/fuzz` target `request_dispatch` (confirmed with
a directed input; the raw mutation path needs the rare 10-byte max-varint, so
it is more reliably hit from the seeded `write` corpus entry).

A client holding a write handle can crash the server with a single message:
`Write { handle, path, offset: u64::MAX, data: [_; 1] }` overflows
`off + data.len()`. With `overflow-checks` on this panics; in a release build
without them it wraps to a tiny `end` and silently corrupts the interval map.

**Fixed:** `Store::write` now rejects the write before it reaches the WAL
(an un-appliable acked record would poison every future replay, so the check
sits alongside `validate_mutation_path`): a `u64`-overflowing
`offset + data.len()` *and* any extent beyond the chunk-region capacity
(which could never flush, and would force `FileOverlay::apply` to materialize
the whole extent) return the new `StoreError::WriteOutOfRange`, surfaced on
the wire as `ErrorCode::BadOffset`. Regression tests:
`cas/tests/fuzz_regressions.rs::ovl1_*` and, end to end,
`storage-server/tests/fuzz_regressions.rs::ovl1_dispatch_write_offset_overflow_rejected`.

---

## ELF-1 — `elf::parse` panics on a near-`u64::MAX` `e_phoff`

**Severity:** low–medium (DoS of the loader by anyone who can write a program
image into the versioned store).
**Where:** `loader/src/elf.rs:46` (`u32le`: `b.get(off..off + 4)`), and
similarly `phoff + i * phentsize` in `parse`.
**Found by:** `loader/fuzz` target `elf_parse` (≈ seconds from the seed
corpus).

The module documents itself as "Strict (untrusted input …): bounds-checked,
no panics", but `e_phoff` (and the per-entry `ph` offset) are added to small
constants with unchecked `usize`/`u64` arithmetic. A header with
`e_phoff = u64::MAX` makes `u32le(bytes, e_phoff)` compute `off + 4` and
overflow. Program images are data in the store, so this is untrusted input.

**Fixed:** the read helpers (`u16le`/`u32le`/`u64le`) do their end-offset
math with `checked_add`, and `parse` computes each program-header offset
with checked mul/add and bounds the whole entry against `bytes.len()` before
reading any field; overflow or out-of-bounds is `Err(ElfError::Truncated)`.
Regression test:
`loader/tests/fuzz_regressions.rs::elf1_phoff_overflow_rejected`.

---

## Observation (not a bug under the raw-mutation model) — mount index allocation

`Store::mount` sizes `vec![0u8; ilen]` from the index frame's length header
before the device read that would bound-check it (`cas/src/store.rs`). That
is the classic length-driven-allocation shape — but `ilen` is gated by
`sb.index_off + frame_len <= sb.chunk_tail`, and `chunk_tail` lives in the
checksummed superblock body, which a mutation fuzzer cannot enlarge without
invalidating the superblock. `mount_recovery` ran 2.7M execs at
`-malloc_limit_mb=128` with no single-allocation OOM, confirming it is **not**
reachable by mutating a real image.

It *would* be reachable by a forged/re-sealed superblock (recompute the body
checksum after setting `chunk_tail` huge). That is outside the seeded
raw-image threat model this target covers, so no harness asserts it; noted
here so a future hardening pass (bound `ilen` by remaining device length
before allocating) and a possible `mount` re-seal target have the context.

The same boundary applies to the OVL-1 fix: `Store::write` guarantees no
out-of-range `Write` record is ever *written*, so mount-time WAL replay
(which applies records to overlays without re-validating extents) only sees
sane records from any image this code produced. A forged WAL record with a
re-sealed checksum could still panic replay — same out-of-model territory as
the superblock case above, same future re-seal target.
