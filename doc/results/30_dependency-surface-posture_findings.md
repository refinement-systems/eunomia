# Findings 30 — dependency-surface posture (C7, review finding 11)

Task **C7** of `doc/plans/3_plan-std-correction.md`, acting on finding **11**
(observation) of the independent review (`doc/results/22_std-port-review.md`). The
review noted that a std user binary pulls the `storage-server`/`cas`/`blake3` stack
into its lock graph through `eunomia-sys` and merely *relies on* release LTO/DCE to
shed it — an inference it never measured. C7 measures it.

**The review's premise, corrected (per the plan).** The review read `ipc` and
`storage-server` as plain `[dependencies]`. They are not: `eunomia-sys/Cargo.toml`
sits them (with `urt`) under
`[target.'cfg(any(target_os = "eunomia", target_os = "none"))'.dependencies]`, so
the host verify/test graph never sees them. The substance survives the correction:
every `user/*` binary *is* built for that target, so at the lock/rlib level the
stack **is** compiled (this run's build log shows `cas`/`storage-server`/`blake3`
compiling for `user/hello`). The open question C7 answers is whether that compiled
stack survives into the *final linked binary* or is shed by LTO + `--gc-sections`
dead-code elimination. It is genuinely open because `eunomia-sys/src/pal.rs`
exports the fs shims `__eunomia_fs_*` as `#[no_mangle] extern "Rust"`, and
`#[no_mangle]` symbols can survive DCE depending on how LTO and `--gc-sections`
treat unreferenced exported symbols in a static executable.

**Verdict: posture confirmed, and stronger than the review inferred.** A trivial
std binary sheds `blake3`, `cas`, **and** `storage-server` entirely — zero symbols.
`blake3`/`cas` are shed from *every* client-side binary (even one that exercises
the fs), because the fs client only marshals the storaged wire protocol; it never
hashes or chunks (that is the server's job). No feature-gating is needed; the
deferred item stays deferred.

## Measurement method

Each binary was built for `aarch64-unknown-eunomia` in release, reproducing
`kernel/build.rs`'s build-std flags (`forward-port.md` §3.6), with the single
profile override `CARGO_PROFILE_RELEASE_STRIP=false`. `strip` removes only the
symbol table, not code, and runs *after* codegen — so the unstripped binary has
byte-identical `.text` to the shipped (`strip = true`) one, but keeps the symbol
table `llvm-nm` needs. Toolchain: the kernel/user pin `nightly-2026-06-26`;
`llvm-nm`/`llvm-size`/`llvm-strip` from that toolchain's sysroot.

```sh
env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS -u CARGO_TARGET_DIR \
  CARGO_PROFILE_RELEASE_STRIP=false \
  __CARGO_TESTS_ONLY_SRC_ROOT="$PWD/vendor/rust/library" \
  cargo +nightly-2026-06-26 build --release \
    --manifest-path user/<bin>/Cargo.toml \
    --target targets/aarch64-unknown-eunomia.json \
    -Zjson-target-spec \
    -Zbuild-std=core,compiler_builtins,alloc,std,panic_abort \
    -Zbuild-std-features=compiler-builtins-mem \
    --target-dir target/user
```

Symbol check (demangled crate paths, not bare substrings — bare `cas` collides
with atomic compare-and-swap, "case", "broadcast", so the pattern anchors `cas::`
at a crate-root boundary):

```sh
llvm-nm -C <bin> | grep -E 'blake3::|(^|[^A-Za-z0-9_])cas::|storage_server::'
```

## What was measured

Three binaries, chosen so every pattern is proven non-zero where the crate is
genuinely reachable and zero where it is not (anti-vacuity):

- **`user/hello`** — the minimal real std binary (deps: `eunomia-sys` only;
  `println!`/argv/alloc/`env::var`/`Instant`/exit; **no** file I/O).
- **`user/stdfs`** — the std fs gate fixture (`File`/`read`/`write`/`read_dir`/
  `rename`/`remove`/`sync`), so it references `__eunomia_fs_*`.
- **`user/storaged`** — the storage *server*, which hashes and chunks, so it is the
  one binary that genuinely uses `cas` + `blake3`.

`llvm-size` sections are strip-invariant (the runtime footprint); the shipped
on-disk size is the `strip = true` file. Symbol counts are demangled matches.

| binary | shipped bytes | text | data | bss | `blake3::` | `cas::` | `storage_server::` | `__eunomia_fs_*` |
|---|---|---|---|---|---|---|---|---|
| `hello`    |  54 432 |  47 368 | 764 | 1 067 488 | **0** | **0** | **0** | **0** |
| `stdfs`    |  79 048 |  72 888 | 804 | 1 067 488 | 0 | 0 | 8 | 10 |
| `storaged` | 311 648 | 301 512 |   0 | 3 162 248 | 4 | 257 | 64 | 0 |

Reading the table:

- **`hello` sheds the whole stack.** No `blake3`/`cas`/`storage_server` symbol and
  no `__eunomia_fs_*` shim survives. The only `__eunomia_*` seam symbols left as
  distinct `T` entries are the non-inlined lifecycle/env/exit shims —
  `__eunomia_{bootstrap_init,argv,env,io_message,thread_exit,tls_run_dtors}`; the
  thin alloc/stdio shims are inlined into their std callers by LTO (their bodies —
  `StdoutRaw`/`LineWriter`, `urt::slots::SlotAlloc` — are present, but no standalone
  shim symbol). `urt` (the allocator) is present, as expected for a binary that
  allocates.
- **`stdfs` keeps the fs client, still sheds `blake3`/`cas`.** Exercising the fs
  retains all 10 `__eunomia_fs_*` shims and 8 `storage_server::` symbols (the
  postcard wire codec — `Request`/`Response`/`DirEnt`/`SnapInfo` de/serialization),
  which gives the `storage_server::` and `__eunomia_fs_*` checks teeth. But
  `blake3`/`cas` stay at **0**: the client marshals messages to storaged; it never
  hashes content or walks the CAS tree.
- **`storaged` keeps all three.** The server retains `blake3` (4), `cas` (257 — the
  prolly-tree/btree instantiations over `cas::disk::*`), and `storage_server` (64),
  proving the `blake3::`/`cas::` patterns detect those crates when present. It keeps
  **no** `__eunomia_fs_*` shims — it is the server, not a consumer of the client
  PAL. (`storaged` is `no_std`; the symbol-detection teeth check is language-model
  agnostic.)

Every pattern is thus non-zero in at least one binary and zero where the code is
unreachable — the `hello` result is a real shed, not a vacuous grep.

## Decisions (with rejected alternatives)

- **Measured `user/hello`, the binary the plan names.** It is the smallest *real*
  (non-fixture) std program and its only declared dep is `eunomia-sys`, so it is the
  cleanest witness that a trivial binary sheds the stack. The fixtures
  (`stdsmoke`/`stdfs`/`stdio`) are equally minimal but purpose-built; `hello` is the
  honest "a user ran a hello-world" case finding 11 is about.
- **Unstripped build via `CARGO_PROFILE_RELEASE_STRIP=false`, not a manifest edit.**
  An env override changes nothing in the tree and touches only the one profile key;
  because `strip` is post-codegen, the LTO/DCE outcome (which symbols the linker
  keeps) is identical to the shipped binary. Rejected: temporarily editing
  `user/hello/Cargo.toml` (mutates a committed file for a measurement) and inspecting
  a linker map (needs `RUSTFLAGS`, which `kernel/build.rs` strips — more moving parts
  than `nm` for no extra certainty).
- **Demangled, crate-boundary-anchored grep.** `llvm-nm -C` yields `blake3::…`,
  `cas::…`, `storage_server::…`; anchoring `cas::` at a non-identifier boundary
  avoids the false positives a bare `cas` substring hits (atomic CAS, `broadcast`,
  `downcast`), while `<cas::…>` in trait-impl symbols still matches. Confirmed
  against `storaged`, where real `cas::`/`blake3::` symbols are present.
- **Three binaries for anti-vacuity, not one.** `hello` alone cannot show the
  `blake3`/`cas` check has teeth — those crates are absent from *every* client, so a
  zero could be vacuous. `storaged` supplies the positive control for `blake3`/`cas`;
  `stdfs` for `storage_server`/`__eunomia_fs_*`. The layering this surfaced
  (`blake3`/`cas` are server-only; the fs client carries only the wire codec) is
  itself the finding's strongest form.

## Standing check recorded

Because a `std`-nightly / LLVM bump could regress LTO or `--gc-sections` behavior
silently, a one-line re-check was added to the forward-port runbook's regression set
(`doc/guidelines/forward-port.md` §6): on a bump, rebuild `user/hello` unstripped and
confirm `llvm-nm` shows no `blake3`/`cas`/`storage_server` symbols. It is a manual
runbook line, deliberately **not** a CI gate — an observation-level posture does not
warrant a CI job that builds `hello` unstripped (diverging from the `strip = true`
shipping posture) on every push. The dependency-edge *rationale* already lives in
`eunomia-sys/Cargo.toml`'s target-gate comments and needs no change.

## Surface left trusted

- The measurement is one nightly (`nightly-2026-06-26`) at one opt level (`hello`'s
  `opt-level = "s"`, `lto = true`). The shed is a property of that toolchain's LTO +
  `--gc-sections`, not a proof — which is exactly why the re-check rides the
  forward-port bump procedure rather than being asserted once and forgotten.
- No code, `Cargo.toml`, `verus!{}`, wire/ABI, or CI surface moved. This task builds,
  inspects, and documents; the verified tally is untouched.

## Follow-ups

- **Feature-gating fs out of `eunomia-sys` stays deferred.** The plan escalates it
  into a real task only if DCE were measured to *fail*; it does not. std's PAL
  declares `__eunomia_fs_*` unconditionally, so gating the shims out is link-fragile,
  and this measurement shows it buys nothing: the unused stack is already shed. No
  task filed.
