# Findings 20-1 — cross-bridge lockstep guard (a Findings #20 follow-up)

Task: the second of the three follow-ups left by **std-port 6.2**
(`doc/results/20_fuzz-pal-audit_findings.md`, "Follow-ups"). Status of the three:
(1) the cas verified-codec corpora enrichment — **already done** in PR #289
(`gen_cas_corpus.rs` now seeds all 12 cas fuzz targets); (2) the `bare_metal` cfg
alias — **left deferred** (cosmetic, high blast radius: a `build.rs` on four
verified crates + a re-verify of all four — the user's standing decision); (3) the
**cross-bridge lockstep drift points** — this task.

**Headline:** the three values mirrored across the `__eunomia_*` bridge that #20
flagged as "trusted-by-review" now have **compile-time pins on the seam side**, the
source of truth. A silent edit to any of them fails the seam crate's own build,
directing the developer to the vendored-std twin. Achieved with **zero edits to the
vendored std** — no forced std rebuild, no new bridge symbol, no runtime cost, no
new logic in the trusted PAL shell. No verified count moved (eunomia-sys 16,
ipc 71); **ledger tally stays 14**.

## The problem (from #20)

`STATUS_PANIC`, the `io_error::Kind` `#[repr(u8)]` discriminants, and the readdir
wire layout are duplicated on both sides of the `__eunomia_*` FFI bridge between
`eunomia-sys` (the host-tested seam) and the vendored std PAL
(`vendor/rust/library/std/src/sys/…`). std's PAL cannot import `eunomia_sys` types
— its verified deps pull `vstd`, unbuildable as a `rustc-dep-of-std` crate
(findings #7-2) — so the two sides are kept consistent **only by manual review**. An
un-mirrored edit corrupts process exit-status / errno classification / readdir
decoding with no build or test failure.

## Decision — pin the seam, don't make the PAL assert at runtime

#20's sketch was "a seam-exported constant the PAL asserts against, where the bridge
allows a scalar." The shipped approach is its inverse and strictly cheaper: **pin,
on the seam side, every value the PAL hard-codes as a literal**, using the
`const _: () = assert!(…)` compile-time idiom the tree already uses
(`eunomia-sys/src/tls.rs`, `kcore/src/thread.rs`, `urt/src/time.rs`). Because the
seam is the source of truth and the PAL is a downstream mirror, freezing the seam
value means the mirror cannot drift silently: a change fails the seam build, and the
adjacent comment names the PAL twin to update in lockstep.

*Rejected:* a new `__eunomia_status_panic()`-style export the PAL asserts against at
runtime — it would add a bridge symbol, a runtime check on the exit/abort path, new
logic in the trusted "term-for-term delegator" shell (contradicting #20's verdict),
and an edit to the vendored std that forces a full `-Zbuild-std` rebuild. The
seam-side pin gets the same drift protection for the realistic case (edits land on
the seam, the authored crate) with none of that cost.

## What shipped

- **`io_error::Kind` discriminants (drift point 2 — highest value).**
  `eunomia-sys/src/io_error.rs`: a `const _: () = { assert!(Kind::… as u8 == N); … }`
  block pinning all 13 discriminants (0..=12) to the literals the PAL's
  `sys/io/error/eunomia.rs::decode_error_kind` matches on. A reorder/insert now
  fails the build instead of silently remapping errno → `io::ErrorKind`. The enum
  doc comment names the pin.
- **`STATUS_PANIC` sentinel (drift point 1).** `eunomia-sys/src/syscall.rs` and
  `ipc/src/sys.rs` each gain `const _: () = assert!(STATUS_PANIC == u64::MAX);`,
  freezing both seam copies to the all-ones value the PAL hard-codes
  (`sys/pal/eunomia/common.rs`'s `abort_internal` sends `u64::MAX`; the
  `mod.rs`/`exit.rs` exit arms zero-extend so a clean exit never collides with it).
- **readdir wire layout (drift point 3 — partial, by design).**
  `eunomia-sys/src/fs.rs`: a named `RD_ENTRY_HEAD` constant derived from the field
  widths (`u8 + u64 + u16`) with `const _: () = assert!(RD_ENTRY_HEAD == 11)`,
  pinning the 11-byte entry head the PAL's `parse_listing` reads, plus a comment
  cross-referencing the twin. The variable-length `name` and the error buffer's
  `i64` code stay review-coupled across the `Vec<u8>` seam — #20's "one genuine
  wire-format twin", whose full fix needs the bridge to carry structured data.

## Verification record

| Gate | Command | Result |
|---|---|---|
| verus — eunomia-sys (cold) | `cargo clean -p eunomia-sys && cargo verus verify -p eunomia-sys` | **16 verified, 0 errors** (unchanged) |
| verus — ipc (cold) | `cargo clean -p ipc && cargo verus verify -p ipc` | **71 verified, 0 errors** (unchanged) |
| host tests | `cargo test -p eunomia-sys -p ipc` | **all pass** (`io_error` proptests incl.; the pins compile-check) |
| target build | `cd kernel && cargo build` | **builds** in ~16 s incremental (no std rebuild — vendored std untouched; the target-only `fs.rs` pin compiles) |
| QEMU boot smoke | `scripts/run-demo.sh` (perl process-group harness) | `[storaged] store mounted` → `serving`; shell `write`/`sync`/`cat`/`ls`/`df` echo; no panic |
| formatting | `cargo fmt --check` + `scripts/verusfmt.sh --check` | **clean** |

## Surface left trusted / review-coupled (and why)

- **The readdir `name`/error-code wire bytes and the `fs::Meta` `#[repr(C)]` mirror**
  (a fourth drift point of the same class, outside #20's three) remain review-coupled
  across the `Vec<u8>`/`#[repr(C)]` seam. Hardening them mechanically needs the bridge
  to carry structured data rather than opaque bytes — the change #20 named as out of
  reach without a shared codec. `RD_ENTRY_HEAD` pins the fixed head only.
- **PAL-only drift** (an edit to `decode_error_kind`/`abort_internal` that isn't
  mirrored back) is not caught by a seam pin; it would need an on-target test of the
  PAL tables. The seam pins cover the realistic direction (edits on the authored
  crate) at zero cost; an on-target end-to-end check is a possible future strengthening.

## Ledger changes (`doc/guidelines/verus_trusted-base.md`) — tally stays 14

No new seam, no Baseline count moves: the change is plain-Rust `const _` assertions
outside every `verus!{}` block, adding no verification obligation. **Tally stays 14.**

Per `CLAUDE.md`, this `doc/results` report is a temporary intermediate record and is
not referenced from code, specs, or guidelines.
