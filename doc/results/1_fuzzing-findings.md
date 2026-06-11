# Fuzzing findings

Findings surfaced while standing up the cargo-fuzz harnesses. OVL-1 and
ELF-1 were initially recorded here unfixed (only the harnesses were in
scope) and pinned by `#[should_panic]` reproducers; **all are now fixed**,
and the reproducers were flipped into positive regression tests that assert
the hardened behavior (rejection, not panic). MNT-1 began as a deferred
observation in the same vein and was reclassified as a finding after review
— see its section for that argument.

Every finding here is the same class the harness profile is built to catch:
**arithmetic on an untrusted length/offset/counter** that traps under
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

## MNT-1 — `mount` trusted forgeable superblock geometry (fixed)

**Severity:** nil as an exploit today (writing raw device bytes in the MVP
means you are already the QEMU host, and the payoff is a zero-initialized
allocation abort — a boot-loop DoS, not corruption), structural as a bug
class. The argument for fixing now was never urgency: it is that the patch
is smaller than the deferral note was, that trust boundaries move under
systems and nobody re-audits old mount paths when they do, and that
recovery code runs when everything else has already gone wrong — the last
place to economize on paranoia.
**Where:** `Store::mount` (`cas/src/store.rs`) — every offset/length field
of the winning superblock, and everything those fields vouched for.
**Found by:** harness stand-up. Originally recorded here as an observation
"outside the seeded raw-image threat model"; reclassified as a finding
after review (below). The `mount_reseal` target added with the fix tore
the pre-fix code apart — three distinct overflow sites in successive runs.

The original observation: mount sizes `vec![0u8; ilen]` from the index
frame's length header, but `ilen` is gated by `sb.index_off + frame_len <=
sb.chunk_tail`, and `chunk_tail` lives in the checksummed superblock body,
which `mount_recovery`'s 2.7M mutation execs could never enlarge without
invalidating the slot — so the allocation was called unreachable and a
re-sealed superblock out of scope. That reasoning was wrong, in two ways
the review made precise and we accept in full:

- **The checksum is integrity, not authenticity.** It distinguishes torn
  or corrupted writes from complete ones — there is no secret in it, so
  anyone who can place bytes on the device re-seals it in microseconds.
  "Outside the threat model" drew a *fuzzer-reachability* boundary (the
  mutation fuzzer can't get past the checksum) and mislabeled it a security
  boundary (an adversary trivially can). §4.5's no-fsck story makes mount a
  parser of arbitrary device states — "recovery is a parser of hostile
  disks" is our own harness rationale — and a checksum-valid-but-malicious
  image is not exotic for a system whose images get copied, restored from
  backup, and eventually exchanged. It is the USB-stick problem, the single
  richest historical source of filesystem CVEs, every one beginning with a
  kernel trusting metadata that only the filesystem was supposed to write.
- **The gate was self-referential — the actual bug shape.** `ilen` was
  validated against `chunk_tail`; `chunk_tail` was validated against
  nothing. Both live in the same untrusted block: untrusted data vouching
  for untrusted data. The one piece of ground truth mount holds — the
  device length from the block layer — was never in the chain. And the gate
  as written, `sb.index_off + frame_len <= sb.chunk_tail`, is exactly the
  shape of the two confirmed overflow finds: if that sum wraps, the gate
  passes spuriously even against an honest `chunk_tail`.

**Fixed** with the chokepoint the diagnosis dictates, not a use-site
one-liner: `Superblock::validate_geometry(device_len)` (`cas/src/disk.rs`)
runs immediately after the winning slot is chosen and checks every
offset/length field — WAL region, WAL head, committed chunk region, index
frame header — against the device length, with checked arithmetic
throughout. After the chokepoint, downstream code legitimately trusts those
fields: the index-frame gate and the index-entry/free-extent bounds become
checked adds (a wrap is rejection, not a spurious pass), and the sibling
sites the original note never reached are covered by construction — the
free-extent frame's length header, and the WAL scan that walks
device-derived offsets, which now also validates each record's extent under
the same rule as `Store::write` (a fully checksummed record with an
un-appliable extent cannot be a torn tail — corruption, not log-end).
Geometry violations are `Corrupt`, never a fall-back to the other slot: a
torn write cannot pass the checksum, so checksum-valid-but-impossible
geometry was never honest-written.

The `grep` for "every `vec![0;`/`with_capacity` fed by device bytes" came
back clean afterwards: each mount-time allocation (`ilen`, `wal_len`,
`read_object`'s `len`) is now bounded by a validated field, and the
count-driven decoders (`chunk_list_entries`, `parse_node`/`load_node` under
`MAX_NODE_ENTRIES`, the WAL path decode) were already pre-validated against
their input length. `MemDev`/`CrashDev` also now honor their own
`OutOfRange` contract on a wrapping offset instead of trapping.

Geometry is not the only forgeable scalar, and the fuzzer proved it: two
non-geometry fields feed unchecked arithmetic the chokepoint doesn't own.
`generation` derives `birth_gen = generation + 1` at mount (and `+ 1` at
every commit); `wal_next_seq` is the replay loop's start counter. A
re-sealed `u64::MAX` in either overflows — 2^64 past anything honest — so
both are rejected (`superblock generation exhausted`, `wal sequence
exhausted`) rather than wrapped.

**The carve-out is retired, not refined.** The new `mount_reseal` fuzz
target re-seals a mutated image the way a disk-writing adversary would —
both superblock slots, the index frame, the ref-table object's content
hash, the WAL chain restamped seq-continuous; every checksum recomputed, no
geometry field ever repaired or clamped — so the fuzzer's mutations land on
the fields mount actually consumes. Re-sealing turns the checksum from a
wall into a door, which is the point: be clear-eyed that the prior 2.7M
clean execs proved the checksum works, *not* that the bound is safe — they
never explored the gated region at all, and coverage on the gate's far side
was dark. The target's contract is now total — **mount returns `Ok` or
`Err` over arbitrary device contents, sealed or not; never an abort, never
an allocation unbounded by the device length** — and a total-function
contract does not rot the way a threat-model exclusion does. The raw
`mount_recovery` target stays for the rejection path of an unsealed image.

Verified red then green at each step: against pre-fix code `mount_reseal`
crashes in ~2 minutes (and twice more as each fix landed); with all three
fixes a 600 s hunt at `-malloc_limit_mb=128` is clean. Regression tests
(`cas/tests/fuzz_regressions.rs::mnt1_*`) promote each find to a unit
assertion of the specific `Err` *before any sized allocation*: the named
case (`chunk_tail = u64::MAX`-ish geometry, both the too-big and the
gate-wrapping variant), forged `wal_len` / `wal_head` / `index_off`, a
wrapped index entry behind a fully re-sealed frame (the spurious-pass
demonstration), a forged WAL record at the OVL-1 site, and the two scalar
overflows.
