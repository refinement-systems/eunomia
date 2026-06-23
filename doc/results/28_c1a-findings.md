# C1A — the startup-block codec, host tests, and fuzz target

Phase **C1A** of `doc/plans/17_c1-detail.md` (itself the detailed decomposition
of parent-plan C1, `doc/plans/0_address_audit_rev0.md:662-679`). C1A delivers
the unified, versioned **startup-block codec** behind a host-testable, fuzzed
`loader::startup` seam — **with no producer/consumer rewired**. The three
hand-rolled blocks (`SD02`/`SH01`/`ST01`) and `hello`'s magic-string check keep
working untouched; C1A is a pure, low-risk addition that unblocks the
independent rewiring pairs C1B/C1C/C1D.

## What landed

- **`loader/src/startup.rs`** (new) — the codec. `no_std`/`core`-only (no
  `alloc`), beside `loader::elf`, **not** under the `target_os = "none"` gate so
  it is host-buildable and reusable by the `no_std` user binaries unchanged
  (verified: `cargo build -p loader --no-default-features` is clean).
- **`loader/src/lib.rs`** — `pub mod startup;`.
- **`loader/fuzz/fuzz_targets/startup.rs`** + a `[[bin]]` in
  `loader/fuzz/Cargo.toml` — the decode-totality fuzz target, mirroring
  `elf_parse`.
- **`loader/fuzz/corpus/startup/`** — 99 seeds (7 hand-named documentation
  blocks + the coverage-minimized fuzzer output).
- **`loader/tests/fuzz_corpus.rs`** — a `startup` replay test (joins
  `elf_parse`/`segment_layout` under the documented `--test fuzz_corpus` Miri
  pass; no CLAUDE.md edit needed since `loader` is already in the Quickest-UB
  sweep).
- **`loader/tests/fuzz_regressions.rs`** — `startup1_oversized_counts_refused`,
  the totality regression (the `elf1` analog).

No `user/*` file, no spec, no ledger, no Verus/TLA touched — exactly as scoped.
Ledger tally stays **14**.

## The format — `b"EUS1"`, one versioned block

```text
Startup block  (≤ MAX_BLOCK = 256 bytes = kcore MSG_PAYLOAD)
  Header (7 bytes):
    [0..4] magic = b"EUS1"   [4] ngrants:u8   [5] nargv:u8   [6] nenv:u8
  Grants (ngrants entries; each tagged, size = f(kind)):
    name:u8  kind:u8  then:
      KIND_CAP_SLOT(1)       slot:u32                  (entry 6 B)
      KIND_STORAGE_HANDLE(2) handle:u32                (entry 6 B)
      KIND_REGION(3)         va:u64 len:u64 pa:u64     (entry 26 B)
  Argv (nargv entries):  each  len:u16, then len bytes
  Env  (nenv  entries):  each  len:u16, then len bytes
```

Well-known name ids (`u8`): `ROOT=1 STDIN=2 STDOUT=3 TMP=4 STORAGE=5 TIME=6`,
device names `VIRTIO_MMIO=16 DMA=17`, and `NAME_STRING=0` reserved as a future
string-name escape (so the eventual public ABI, rev1§8.3, can widen to
byte-string names without a format break).

**Three kinds, two from the spec.** `CAP_SLOT` and `STORAGE_HANDLE` are
rev1§5.1's literal two (caps → cspace slots, storage grants → handle numbers).
`REGION` generalizes the one grant the system already delivers — `time` is a
pre-mapped VA in every block today, and the old `SD02` MMIO/DMA fields are the
same shape (VA + len, plus a device PA for DMA). It carries **no new
authority**: the parent maps the page before start exactly as today; only the
VA travels. This subsumes every field the three current blocks carry:

| old block | fields | new representation |
|-----------|--------|--------------------|
| `SD02` (init→storaged, 44 B) | mmio_va, dma_va, dma_pa, dma_len, time_va (5×u64) | 3 `REGION` grants (`TIME`, `VIRTIO_MMIO`, `DMA`) |
| `SH01` (init→shell, 12 B) | time_va | `TIME` `REGION` + (C1C) `STORAGE` cap-slot, `ROOT` handle |
| `ST01` (shell→child, 13 B) | mode:u8, time_va | `TIME` `REGION` + (C1D) `argv` |

## Codec shape & the totality argument

- `decode(&[u8]) -> Option<Startup<'_>>` — total. A bounds-checked `Reader`
  cursor (every read is `slice.get(..)` guarded by `checked_add`, mirroring
  `elf::u16le/u32le/u64le`) validates the magic, then each count against its
  arena cap (`MAX_GRANTS`/`MAX_ARGV`/`MAX_ENV = 8`), then for every grant
  body / argv / env entry checks the declared length against the **remaining**
  slice *before* reading. Any shortfall, unknown `kind`, bad magic, or over-cap
  count → `None`; never a panic, an OOB read, or an unbounded allocation
  (rev1§2.7). argv/env decode as **borrowed** `&[u8]` into the input buffer (no
  alloc in `_start`); the grant table is a fixed-size arena. Trailing bytes are
  tolerated (the `elf`/`parse_config` precedent) — the recv buffer is always a
  padded `[u8; 256]`.
- `encode(&Startup, out: &mut [u8]) -> Result<usize, EncodeError>` — total the
  other way. A bounds-checked `Writer`; `Err(Overflow)` if the block would
  exceed `out` or `MAX_BLOCK`, `Err(TooManyEntries)` past an arena cap (or a
  byte-string longer than `u16::MAX`). Never panics, never truncates — the
  producer (init/shell, in C1B/C/D) maps either to a clean boot/spawn failure.
- `Startup<'a>` carries fixed arenas + counts; `PartialEq` compares only the
  meaningful `[..n]` prefixes, so a producer-built block and the result of
  decoding its encoding compare equal regardless of arena filler. Builders
  (`push_grant`/`push_argv`/`push_env`, all bounded) and a `grant(name)` lookup
  are provided for the C1B/C/D consumers.

## Tests & verification (all green)

- `cargo test -p loader` — 12 `startup` lib tests + the unchanged `elf`,
  `layout_props`, `fuzz_corpus` (now incl. `startup`), `fuzz_regressions` (now
  incl. `startup1`). The lib tests are: golden layout (encode bytes pinned at
  every offset + decode-back), trailing-byte tolerance, malformed rejection
  (bad magic / truncated header / truncated body / unknown kind / over-cap
  count / over-long argv), over-budget refusal, builder-arena refusal, the
  **negative-control** "round-trip oracle has teeth" (perturb a field → the
  equality oracle distinguishes it), the encode→decode **round-trip** proptest,
  and the **totality** proptest (`decode` over arbitrary bytes never panics;
  every returned slice lies inside the input). Proptests use the
  `cfg!(miri){4}else{256}` case-count idiom.
- **Fuzz**: `cargo +nightly fuzz run startup -- -max_total_time=60` — ~49.7M
  executions, **0 crashes**. `cargo fuzz cmin` minimized the corpus to a
  coverage-preserving set.
- **Miri**: `MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p
  loader --test fuzz_regressions --test fuzz_corpus` — UB-clean (the startup
  corpus replays through the interpreter in ~5 s).
- **aarch64 cross-build**: `cd kernel && cargo build` links the full `user/*`
  stack with the new module compiled into `loader` (no behavior change).
- **Integration smoke**: `scripts/run-demo.sh` boots green on the **old** blocks
  (C1A wired nothing onto the new codec) — storaged mounts and serves,
  `date`/`write`/`cat`/`ls`/`df`/`run` behave, no panic/`Corrupt`.

## Notes for the rewiring pairs (C1B/C1C/C1D)

- The codec is shared by *both* ends of each channel, so the per-pair tests
  drive the real `encode`/`decode` rather than mirrored hand-parsers — the
  B15C round-trip pattern, now with one codec instead of two.
- A within-arena block can still exceed 256 bytes (8 region grants ≈ 215 B
  before argv): producers must treat `encode`'s `Err` as a spawn failure. The
  real blocks are far under budget (storaged ≈ 85 B; shell ≈ 50 B + argv).
- `decode`'s arena caps are 8/8/8; revisit only if a future block needs more.
- Naming convention reused: `loader/fuzz/corpus/startup/` keeps human-readable
  named seeds (`storaged_regions`, `shell_named`, `child_argv`, `minimal`,
  `max_grants`, `truncated_count`, `truncated_argv`) alongside the hash-named
  fuzzer output, the way `elf_parse` does.
