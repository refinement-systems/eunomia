# Findings 13 — Phase 4.1: the storaged filesystem client

Task 4.1 of `doc/plans/2_plan-std-revised.md`: a real `sys/fs/eunomia.rs` std
arm over the userspace storage server (`storaged`, rev2§4). A std binary can now
`File::create`/write/read/`read_dir`/`rename`/`remove_file`/`sync_all` against the
versioned store, proven live in QEMU (`STD4 PASS`).

## What shipped

- **`ipc::session::connect`** — the client half of the connect handshake (rev2§3.5),
  the counterpart of the existing server `admit_connect`. Reactor-free (a minimal
  request/response client has no notification cap for a `Reactor`): it drives
  `Transport::{send_nb,recv_nb}` with a caller-supplied `on_block` (yield on the
  target, no-op in a host test) and returns the negotiated wire version or a
  `ConnectErr`. Three new client-side `ConnectErr` variants — `PeerClosed`,
  `BadReply`, `Transport` — the "richer errors" the module doc said were "not yet
  constructed." Plain glue, outside `verus!{}`; host-tested against `admit_connect`
  with a loopback transport. `ipc` re-verifies **71 verified, 0 errors** (unchanged).
- **`eunomia-sys::fs`** — the client (target-gated, links `storage-server`+`ipc`):
  grant resolution + connect at bootstrap, `Request`/`Response` marshalling over
  `SyscallTransport`, the `File` cursor (storaged is offset-stateless), chunked
  read/write offset loops (256-byte `MAX_MSG`), and the `ErrorCode`/`NotFound` →
  raw-code map (extends `io_error`). `__eunomia_fs_*` bridge shims in `pal.rs`.
  `eunomia-sys` re-verifies **7 verified, 0 errors** (unchanged — the client is a
  target-only trusted shell, so the host verify graph is byte-identical).
- **`sys/fs/eunomia.rs`** (vendored std) — the full `imp` surface: `File`,
  `FileAttr`, `ReadDir`, `DirEntry`, `OpenOptions`, `FileType`, `FilePermissions`,
  `FileTimes`, `DirBuilder`, and the 16 free fns. The gate ops are real; the rest
  are thin `unsupported()` bodies (see below). Reuses `common::{copy,
  remove_dir_all, exists, Dir}`. Adds `ErrorKind::NotFound` to `decode_error_kind`.
- **storaged multiplex** — the serve loop now dispatches by reactor key: key 0 is
  the shell's session (unchanged), key 1 is the fs client's. The second session is
  admitted lazily on the child's `ConnectReq` and re-opened (TAG_REQ-detected) when
  a later child reuses the delegated channel. `storage_server` (the lib) is
  untouched — it already multiplexes sessions.
- **init + shell wiring** — init creates a second session channel (`SESSION2_A`
  → storaged slot 3, `SESSION2_B` → shell slot 7); the shell delegates a copy of
  `SESSION2_B` to each **fs-capable** child (`FS_CAPABLE` allowlist, the
  `THREAD_CAPABLE` mechanism) at child cspace slot 1, named `storage`/`root` in its
  startup block (`build_child_block`).
- **`user/stdfs`** — the fs GATE fixture; `scripts/fs-smoke-test.sh` + a CI step.

## Decisions (and alternatives rejected)

- **Multiplex storaged for a fresh session, not delegate the shell's live one.**
  The alternative (hand the fs child the shell's own connected session and reuse
  the negotiated version) is smaller but never exercises the client-side connect
  handshake in the 4.1 gate and shares a live channel between two processes. The
  multiplex path opens a *fresh* session per fs child with clean isolation and runs
  the real handshake live (`[storaged] fs session negotiated wire version 2`). The
  cost is bounded: the `storage_server::Server` already supports multiple sessions
  (`open_session` → distinct `SessionId`); only the storaged *binary*'s serve loop
  changed, additively.
- **Re-connect by TAG_REQ detection.** The shell delegates a copy of `SESSION2_B`
  per child; the underlying channel object persists across `stdfs` runs, so
  storaged's session-2 state would otherwise go stale. Peeking the first payload
  byte (`ipc::session::TAG_REQ` = `0xC0`, vs storage `PROTO_MAGIC` = `0x45`)
  cleanly distinguishes a fresh `ConnectReq` from a request, so each new child
  re-opens the session (closing the prior one, no session/window leak). No new API.
- **A dedicated `stdfs` binary, not an arm on `stdsmoke`.** `stdsmoke` is already
  thread-capable (6 startup grants); adding storage+root would put it at exactly
  `MAX_GRANTS=8` with zero headroom. `stdfs` needs only storage+root (+time+seed) =
  4, and keeps fs off the thread/lock fixture.
- **The storage codec lives in `eunomia-sys`, target-gated.** `storage-server`/`ipc`
  are added under the `cfg(any(target_os="eunomia",target_os="none"))` deps block
  (next to `urt`), so the host `cargo verus verify -p eunomia-sys` graph is
  byte-identical (no storage/cas obligations enter the session). std reaches the
  codec only through the `__eunomia_fs_*` symbols — sound because `eunomia-sys`
  links into the *user binary*, not as a `rustc-dep-of-std` (the finding 7-2
  constraint). `user/shell`/`user/storaged` already build both for the target.
- **`readdir` crosses the bridge as a flat `Vec<u8>`** (a private tag+entry
  encoding), not a shared complex type — both sides are the same rustc/std, so
  `Vec<u8>` ownership transfer over `extern "Rust"` is sound (the `__eunomia_argv`
  posture) and needs no type shared between the two crates.

## Problems hit

- **Submodule/eunomia-sys version skew (the "3.5 in CI" hazard).** The `vendor/rust`
  working tree was checked out at the 3.5 branch (`1fd6bc5`, key-based TLS), but this
  branch is off `main`, whose `eunomia-sys` predates 3.5 and does not define the
  `__eunomia_tls_*` symbols the 3.5 std references — so `stdsmoke` failed to link.
  `main` records vendor/rust at **`d8f7ffba733` (3.4)**; the fix was to reset the
  submodule to that commit (the fs edits to `sys/fs/mod.rs`, `sys/io/error/eunomia.rs`
  are byte-identical between 3.4 and 3.5, and `sys/fs/eunomia.rs` is new, so they
  re-base cleanly) and **`rm -rf target/user`** to drop the stale build-std std that
  still carried the 3.5 key-TLS references. Phase 4 is independent of Phase 3, so the
  fs work sits on the 3.4 submodule; the user manages 3.5/4.1 merge ordering.
- **`Server` is generic** (`Server<D: BlockDev>`, `BlockDev = cas::dev::BlockDev`);
  the factored `serve_request` is generic over `D` rather than naming the
  `VirtioBlockDev<MmioWindow, DmaRegion>` concrete type.
- **`File` must stay `Send + Sync`** like every platform's `File`, so the client
  cursor is an `AtomicU64`, not a `Cell`.

## Surface left unsupported or trusted (and why)

- **The `&[u8]` → `TreePath` split in `eunomia_sys::fs::split_path` is the 4.2 seam.**
  Minimal for 4.1 (split on `/`, drop empty components; no `.`/`..` resolution or
  root confinement). This is the one piece of not-yet-verified byte-parsing — tools
  *can* reach it, and 4.2 replaces it with the Verus-total, fuzzed, `.`/`..`-resolving,
  root-confining parser. Disclosed as an explicit follow-up, not a permanent gap.
- **Metadata is minimal** (size + file/dir). `Stat` reads content length, so it
  answers for files; a directory's type comes from `read_dir`'s `DirEnt`. `modified()`/
  mtime is `Unsupported` (a deferred storage-wire extension). The full 11-variant
  `ErrorCode`→`ErrorKind` decision table is 4.3; 4.1 ships a first cut
  (`NotFound`/`BadPath`→NotFound, `Denied`/`ReadOnly`→PermissionDenied, …).
- **`Unsupported` by construction** (rev2§4.9 has none of it): symlink/hard_link/
  readlink/canonicalize, permissions/`set_perm`/`set_times`, `truncate`/`set_len`,
  `DirBuilder::mkdir` (creation is a side effect of `Write`), `rmdir`, file locks,
  `duplicate`. `File::create` emulates truncate via a best-effort `unlink` (a fresh
  path is `NotFound`, ignored).
- **The fs marshalling is a trusted shell**, the `sys/stdio` posture — no new ledger
  seam (**tally stays 14**). It re-establishes every verified `requires` at the
  boundary: the ≤256 `ChanSend` bound is re-proven at `eunomia_sys::encode` (and the
  chunk loops keep messages bounded; `wire::encode_request` refuses `TooLarge`), the
  rights are the server's verified `attenuate`, and the wire header is the verified
  `check_header`. `sys/fs/eunomia.rs` adds zero logic over `unsupported` — every op
  is a one-line delegation to a `__eunomia_fs_*` symbol.

## Verification record

- `cargo verus verify -p ipc` (cold) → **71 verified, 0 errors**.
- `cargo verus verify -p eunomia-sys` (cold) → le-bytes 6, loader 30,
  **eunomia-sys 7, 0 errors** (unchanged).
- Host tests: `cargo test -p ipc` 37 (incl. 4 new `connect` tests),
  `cargo test -p eunomia-sys` 22 (incl. the fs error-band map),
  `cargo test --manifest-path user/shell/Cargo.toml` 28 (incl.
  `build_child_block_emits_storage_grants`), `cargo test -p storage-server` 20.
- Build: `cd kernel && cargo build` — kernel + every user binary (incl. `stdfs`).
- QEMU: `scripts/fs-smoke-test.sh` → `FS SMOKE TEST PASS` (`STD4 PASS` +
  `[storaged] fs session negotiated wire version 2`, no fault/panic).
- Regression: `scripts/std-smoke-test.sh` → `STD SMOKE TEST PASS`
  (STD2/STD32/STD33/STD34 — the shell/storaged/init changes don't disturb it).
- fmt: `cargo fmt --check` clean (root + touched user manifests); `scripts/verusfmt.sh`
  clean; vendored std fs files `rustfmt --edition 2024 --check` clean.

## Follow-ups / disclosed limits

- **4.2** — the verified path-component parser replacing `split_path`.
- **4.3** — the full `ErrorCode`→`ErrorKind` table + the `Unsupported` stub sweep +
  directory `metadata` (file/dir type).
- **Directory listings must fit one 256-byte message** — a large `read_dir` errors
  (`ERR_FS_INTERNAL`) until the deferred bulk data plane (rev2§3.1). Likewise the
  usable write-chunk shrinks with the encoded path length.
- **Every eunomia std binary now compiles `storage-server`/`cas`/`blake3`** (a
  build-time cost) because std unconditionally declares the `__eunomia_fs_*` symbols,
  so `eunomia-sys` must always define them. `--gc-sections` drops the unused code, so
  there is no binary-size cost. A later split (a dedicated fs-seam crate) could avoid
  the build cost; not worth it for the MVP.
- **A simultaneously thread- and fs-capable child** would clash on child cspace slot
  1 (`CHILD_STORAGE_SLOT` vs the thread self-cap slots); `stdfs` is fs-only, so the
  MVP sidesteps it. A distinct storage slot for thread children is the fix if needed.
