# Findings 24 — errno precision for over-long fs requests (C1.2, review finding 10)

Task **C1.2** of `doc/plans/3_plan-std-correction.md`, closing the errno half of
finding 10 of the independent review (`doc/results/22_std-port-review.md`): the fs
client collapsed an over-long-but-nameable path to an opaque internal error instead
of the honest `ENAMETOOLONG`-shaped errno.

**Headline:** the fs client's request round-trip now maps `WireError::TooLarge` to
`ERR_FS_BAD_PATH` (→ `Kind::InvalidFilename`, the kind Unix gives `ENAMETOOLONG`)
while keeping `WireError::Body` on `ERR_FS_INTERNAL`. One `map_err` site changes; no
wire/ABI change, no `verus!{}` change, and `io_error.rs`'s oracle table is untouched.
A new smoke marker proves the mapping fires end-to-end at EL0.

## The problem (finding 10, errno half)

`eunomia_sys::fs::request()` — the single round-trip every fs op routes through —
collapsed *every* `encode_request` failure to `ERR_FS_INTERNAL`:

```rust
let bytes = wire::encode_request(req, ver).map_err(|_| ERR_FS_INTERNAL)?;
```

`wire::encode_request` (`storage-server/src/wire.rs:103-113`) has only two failure
modes: `WireError::Body` (a postcard serialization fault) and `WireError::TooLarge`
(the framed message exceeds `MAX_MSG` = 256, rev2§3.1). The client caps the data
payload of every request (`WRITE_CHUNK` = 128, `READ_CHUNK` = 192), so **the encoded
path is the only remaining input that can push a `Request` past `MAX_MSG`**. That is
a *nameable* path — `path::resolve` accepts components ≤ 255 bytes and depth ≤ 64, a
product far larger than 256 bytes on the wire — that is simply too long to frame in
one message. Reporting it as an internal fault is dishonest: the fault is the path.

## Decision — map `TooLarge` to a bad-path errno, keep `Body` internal

```rust
let bytes = wire::encode_request(req, ver).map_err(|e| match e {
    wire::WireError::TooLarge => ERR_FS_BAD_PATH,
    _ => ERR_FS_INTERNAL,
})?;
```

- `ERR_FS_BAD_PATH` already classifies to `Kind::InvalidFilename` (`io_error.rs`),
  and the `(ERR_FS_BAD_PATH, InvalidFilename)` oracle row already exists, so the
  policy table needed **no** edit — the mapping is honest against the existing
  decision table, not a new one.
- `request()` is the one choke point (`read`/`write`/`stat`/`metadata`/`rename`/
  `unlink`/`sync`/`readdir_open` all route through it), and its `encode_request` call
  is the client's only one, so a single edit covers the whole fs surface.
- Semantic soundness rests on the payload caps: because `WRITE_CHUNK`/`READ_CHUNK`
  bound the data, `TooLarge` from `encode_request` ⟺ over-long path. If a future
  request variant carried an uncapped non-path field, this equivalence would need
  re-checking; it holds for every variant the client constructs today.

*Rejected:* a dedicated name-too-long error code + `Kind` variant. It would need the
full appended-discriminant lockstep (const pins + PAL `decode_error_kind`) for
marginal errno fidelity over `InvalidFilename` — deferred (see the plan's Deferred
work). `InvalidFilename` is the precise POSIX analog and needs no ABI churn.

## Anti-vacuity teeth (the plan's optional half, adopted)

Without a test the new arm is unexercised. `user/stdfs` gains a deep-path negative
case: a path of two 255-byte components (nameable — depth 2 ≤ 64, each ≤ 255) whose
encoded `Request::Stat` is ≈ 520 bytes ≫ 256. `fs::read` on it flows
`File::open` → `stat` → `request()` → `WireError::TooLarge` → `ERR_FS_BAD_PATH` →
`InvalidFilename`, asserted with `ErrorKind::InvalidFilename` and a new
`[stdfs] toolong->invalid` marker awaited by `scripts/fs-smoke-test.sh` (a wrong
result prints `fs-bad` / exits `12`). The script's op-list header describes the new
op in current-state terms.

## Gate — commands and result lines

- `cargo test -p eunomia-sys` → `35 passed; 0 failed` (lib, incl.
  `io_error::tests::abi_table_is_exact` and
  `io_error::tests::fs_band_is_exact_and_disjoint_from_syscall_band`) plus the
  `fuzz_corpus`/`fuzz_regressions`/`path_proptest` suites green. The io_error oracle
  table is confirmed unchanged (the file is not edited).
- `scripts/fs-smoke-test.sh` under QEMU → `FS SMOKE TEST PASS`; the boot log shows
  `[stdfs] toolong->invalid` between `dotdot resolves` and `readdir found smoke`,
  then `STD4 PASS`, with no `fs-bad`/panic/fault. The marker firing is the live
  witness that `TooLarge` → `InvalidFilename` works at EL0.
- `scripts/std-smoke-test.sh` under QEMU → `STD SMOKE TEST PASS` (unaffected; run
  because the gate names "fs + std smoke").
- `cargo fmt --check` clean in the root workspace and in `user/stdfs` (its own
  manifest). No `verus!{}` touched, so no `verusfmt`.

## Surface left trusted

`fs.rs` is `#![cfg(bare_metal)]` — outside the host verify graph, so no verified
count moves and this change is not mechanized. The `TooLarge`-⟺-over-long-path
equivalence is a reasoned invariant over the client's request constructors (guarded
by the payload caps), witnessed by the smoke marker, not a theorem.

## Follow-ups

None new. The dedicated name-too-long code + `Kind` variant remains deferred (plan's
Deferred work), warranted only if `InvalidFilename` proves too coarse.
