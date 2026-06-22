# C1B — rewire init ↔ storaged (SD02 → the named-grant table)

Phase **C1B** of `doc/plans/17_c1-detail.md` (the detailed decomposition of
parent-plan C1, `doc/plans/0_address_audit_rev0.md:662-679`). C1B migrates the
**first** producer/consumer pair — `init → storaged` — off the bespoke `"SD02"`
fixed block onto the shared `loader::startup` codec delivered by C1A (PR #166).
It is the **region-grant proof-out**: SD02's five `u64` fields become three
`REGION` grants, exercising the additive region kind end-to-end on the most
region-heavy block. The other two pairs (`SH01` init→shell, C1C; `ST01`
shell→child, C1D) are untouched and keep working — the bootstrap message is
per-channel, so the pairs migrate independently.

## What landed

- **`user/storaged/Cargo.toml`** — `+ loader = { path = "../../loader",
  default-features = false }` (the only new dependency; `loader` is `no_std`
  with default features off and its sole transitive dep is `ipc`, which storaged
  already carried).
- **`user/storaged/src/main.rs`** — the SD02 consumer. `parse_config` keeps its
  signature (`&[u8] -> Option<Config>`) and the `Config` struct keeps its five
  fields, so `_start` (which destructures `Config`) is **unchanged**; only the
  body swaps the hand-rolled `&buf[..4] == b"SD02"` + LE-offset reads for
  `startup::decode` + three name lookups via a small `region()` helper.
- **`user/init/src/main.rs`** — the SD02 producer. `build_sd02` (the 44-byte
  layout) → `build_storaged_block`, which constructs a `Startup` of three
  `REGION` grants and `encode`s it into a caller buffer, returning the length or
  a clean `EncodeError`. `_start` maps that `Err` to a boot failure
  (`[init] FAILED: build storaged block`) before `chan_send` — producer totality
  (refuse, never panic). Two informational consts added (`MMIO_LEN`, `TIME_LEN`).

No spec edit, no ledger change (no new seam, tally stays **14**), no Verus/TLA —
exactly as scoped. The two spec edits (§5.1 forward note, §8.3 split) land with
the final pair, when the table is fully in use.

## The SD02 → three-REGION mapping

| SD02 field (5×u64, 44 B) | new representation in the `b"EUS1"` block |
|--------------------------|-------------------------------------------|
| `mmio_va`                | `VIRTIO_MMIO` region: `va = mmio_va`, `len = MMIO_LEN`, `pa = 0` |
| `dma_va`, `dma_len`, `dma_pa` | `DMA` region: `va = dma_va`, `len = dma_len`, `pa = dma_pa` |
| `time_va`                | `TIME` region: `va = time_va`, `len = TIME_LEN`, `pa = 0` |

The encoded block is **39 bytes** (7-byte header + 3 × 26-byte region entries +
0 argv + 0 env) — comfortably under the 256-byte `MAX_BLOCK`. The regions carry
**no new authority**: init still `map`s every page into storaged's aspace before
`start` exactly as before (`user/init/src/main.rs`, the `sys::map(sd.aspace_slot,
…)` calls are untouched); only the VAs travel, now under names in one
self-describing block instead of a fixed byte layout.

### The two informational lengths (a deliberate choice)

Of the three region lengths, only `DMA`'s is functionally consumed (it sizes the
`DmaPool`). The other two are carried for honesty/future use but storaged ignores
them:

- `MMIO_LEN = 32 * 0x200` — the span of the 32 virtio-mmio transports storaged
  probes. storaged keeps its own `for i in 0..32` probe loop driven from
  `mmio_va`; it does not derive the count from the length (behaviour preserved
  exactly).
- `TIME_LEN = 4096` — the time page is a single frame (rev1§2.6). storaged
  `urt::time::attach`es by VA and ignores the length.

Recorded so a later reader does not mistake these for load-bearing values.

## `parse_config` as the shared-codec seam

The cleanest shape kept `parse_config` as storaged's host-testable decode seam —
its body is now `startup::decode(buf)?` followed by three `region(&s, NAME_*)?`
lookups. A `region()` helper returns `(va, len, pa)` for a named `REGION` grant
or `None` if the name is absent **or** carries a non-region kind. So storaged
refuses cleanly (no panic, the existing `fail(b"bad config block")` path) on:
bad magic / truncated entry (decode returns `None`), a **missing** required
region, or a **wrong-kind** entry under a required name. Decode totality itself
is `loader::startup::decode`'s fuzzed guarantee (C1A); this layer adds only the
name resolution, and the new tests pin its two failure modes.

## Tests — ported to the shared codec (no more mirrored hand-parsers)

Because the codec is now shared on both ends, each binary's tests drive the
**real** `encode`/`decode` instead of a local mirror of the other end's byte
layout (the B15C round-trip pattern, now with one codec):

- **`user/storaged`** (6 tests, all green): `parse_config` round-trips the three
  regions (built via the real `encode`); refuses bad magic / one-byte truncation;
  **refuses a missing required region** (a valid EUS1 block omitting `VIRTIO_MMIO`
  → `None`); **refuses a wrong-kind region** (`TIME` carried as a `CapSlot` →
  `None`); the `parse_config_is_total` arbitrary-bytes proptest (never panics);
  and `parse_config_accepts_well_formed` over arbitrary VAs/PA/len with trailing
  padding. Dropped: the `sd02` byte-layout mirror.
- **`user/init`** (5 tests, all green): `build_storaged_block` carries the three
  regions with the right `(va, len, pa)` and no argv/env, decoded by the **real**
  `startup::decode`; a proptest round-trips arbitrary region fields through
  encode→decode. Kept: the `SH01` builder tests (C1C territory, still mirrored)
  and `rtc_sane`. Dropped: `build_sd02_*` and the `parse_sd02` mirror.

## Verification (all green)

- `cargo test --manifest-path user/storaged/Cargo.toml` — 6/6.
- `cargo test --manifest-path user/init/Cargo.toml` — 5/5.
- `cargo test -p loader` — unaffected (loader untouched).
- `cargo fmt` via **both** `user/init` and `user/storaged` manifests (the
  workspace-split trap — the root `cargo fmt` silently skips the `user/*`
  mini-workspaces).
- aarch64 cross-build: `cd kernel && cargo build` links every `user/*` binary
  with `loader::startup` compiled into storaged and init for the real target.
- **QEMU boot smoke** (`scripts/run-demo.sh` under the CLAUDE.md Perl
  process-group timeout harness) — green: `[storaged] virtio-blk up` →
  `store mounted` → `serving` (the three region grants arrived and storaged drove
  the device from the looked-up VAs), then `write`→`ok`, `cat docs/smoke`→
  `hello-c1b` (store round-trip), `ls`/`df` behave, `date` returns a valid
  timestamp, `run bin/selftest 42`→`exited(42)` (spawn + store-load work). No
  panic/`Corrupt`.
- Verified-surface gates (kcore/cas/ipc/freelist/dma-pool/urt Verus counts, the
  three TLA models, the fuzz corpora) held by not touching them.

## Notes for C1C / C1D

- The shared-codec test pattern proved out here applies directly: build the block
  with the real `encode`, assert the consumer's lookups recover it.
- `Config`/`parse_config` staying signature-stable kept `_start` a no-op change —
  the same trick (keep the consumer's extraction in one host-testable function,
  swap only its body) is available to the shell (C1C) and selftest (C1D).
- The region kind is now exercised on real hardware authority end-to-end; C1C
  adds the `CAP_SLOT` (`STORAGE`) and `STORAGE_HANDLE` (`ROOT`) kinds, C1D adds
  argv. No format change is needed for either — C1A's `b"EUS1"` already carries
  them.
