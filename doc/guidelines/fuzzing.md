# Fuzzing

cargo-fuzz (libFuzzer) harnesses for the host-side parsers — the on-disk
formats, the IPC wire protocol, and the ELF loader. 

One fuzz crate lives next to each library crate it exercises:

| Crate | Fuzz crate | Targets |
|-------|------------|---------|
| `cas` | `cas/fuzz` | `tlv_entry`, `tree_node`, `index_frame`, `superblock`, `superblock_fixup`, `wal_replay_scan`, `wal_replay_scan_fixup`, `chunker`, `mount_recovery` |
| `storage-server` | `storage-server/fuzz` | `request_dispatch`, `structured_request` |
| `loader` | `loader/fuzz` | `elf_parse` |

## Running

Needs a nightly toolchain and `cargo install cargo-fuzz`.

```sh
# Replay the committed corpus through every target (fast; what CI runs per PR).
scripts/fuzz.sh smoke

# Time-boxed hunt across every target (default 300s each).
scripts/fuzz.sh hunt 300
scripts/fuzz.sh hunt 600 cas        # one crate only

# A single target, by hand (run from the owning crate's directory):
cd cas && cargo +nightly fuzz run mount_recovery -- -max_total_time=120
```

Each fuzz crate's profile forces `debug-assertions` and `overflow-checks`
on, so arithmetic on an untrusted length/offset **traps** rather than
wrapping — that is how the two findings in `fuzzing-findings.md` surface.
Run with a low `-malloc_limit_mb` (the `hunt` mode sets 128) so a
length-field-driven allocation becomes a reportable crash rather than a
silent OOM.

## The oracles

The strongest property is nearly free because the on-disk formats are
**canonical** — exactly one byte encoding per logical value. So for the
strict codecs the workhorse assertion is one line: any bytes the decoder
accepts must equal their own re-encoding (`tlv_entry`, the leaf half of
`tree_node`, `superblock`, `wal_replay_scan`). Accepting non-canonical
bytes is a bug even when nothing panics — it makes two byte strings denote
one logical value, breaking "same contents ⇒ same hash."

Two formats decode into `BTreeMap`s, which normalize key order and collapse
duplicates: the durable **index frame** and the **ref table**. They are
canonical only up to key ordering, not byte-canonical, so those targets
assert round-trip *stability* (`decode∘encode∘decode == decode`) instead —
the same weakening the postcard wire bodies carry (`structured_request`),
and for the same reason: nothing downstream hashes a *logical* value, only
the encoder's bytes. The asymmetry is documented in each harness.

## Getting behind the integrity gates

A mutation fuzzer can't forge a BLAKE3 checksum, so a target fed straight
at a checksummed structure explores the rejection branch forever. Two
moves keep coverage from plateauing at "checksum mismatch":

- **Fuzz below the gate.** The node/index/WAL-record decoders are fuzzed on
  raw bytes directly — their hash/checksum check lives one layer up (at
  fetch / at mount), so the decoder itself sees unauthenticated input.
- **Re-seal after mutation.** `cas::fuzz_support` (compiled only under the
  `fuzzing` feature) recomputes the checksum field after mutation, so the
  fuzzer's edits reach the field-validation logic behind the gate. The
  `_fixup` targets use this; the no-fixup variants stay too, because the
  rejection path is itself code under test.

`mount_recovery` is the meeting point: arbitrary bytes presented as a whole
image to the fake block device. Mount must return `Ok`/`Err`, never panic,
never read out of bounds; on `Ok` a cheap consistency pass walks the refs.
It is seeded with mkfs-style minimal images and crash-injection artifacts.

## Corpus

The committed corpus is the **curated seeds** emitted by the generators
(`cargo run -p cas --example gen_cas_corpus`, and `gen_storage_corpus` /
`gen_loader_corpus` for the other two) — small, deterministic, and
regenerable, so the repo doesn't carry thousands of opaque fuzz blobs.
They are enough to start every run warm on the happy path the mutation
fuzzer struggles to reach unaided.

A local fuzzing run grows `fuzz/corpus/<target>/` with coverage-expanding
inputs; minimize before sharing with `cargo +nightly fuzz cmin <target>`.
`fuzz/artifacts/` (crashes) and `fuzz/target/` are git-ignored.

Every committed input is also replayed by `cargo test` (the `fuzz_corpus`
integration test in each crate). Pointed at Miri it UB-checks each one — the
two tools compose. The replay reads the corpus from disk, and Miri's
filesystem isolation otherwise makes that a no-op, so disable it:

```sh
MIRIFLAGS=-Zmiri-disable-isolation cargo +nightly miri test -p cas --test fuzz_corpus
```

(The `mount_recovery` replay is skipped under Miri — whole-image BLAKE3
hashing is prohibitively slow; the decoders mount calls into are UB-checked
by the other targets.)
