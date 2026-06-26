# Plan — Porting the Rust Standard Library to Eunomia

> Renumber to the current era convention (e.g. `N_plan_rust_std.md`). Targets `spec rev2`.
> Two-track note: almost all of this is **userspace-track** work and is host-testable on macOS
> (Miri/Loom/Shuttle) decoupled from the kernel, per §1. Only three items touch the
> **kernel track** — `TPIDR_EL0` save/restore, a `yield` syscall, and an entropy source — and
> they are isolated in M3 so the kernel and storage tracks are not blocked before then.

## Strategy

Shortest path to a running `println!`, then expand surface incrementally. std layers cleanly:
`core` and `alloc` carry no OS coupling and already work (your userspace is `no_std` Rust today);
all OS coupling lives behind the **PAL** at `library/std/src/sys/pal/eunomia`. The two hard boot
blockers are a working `GlobalAlloc` and TLS — we clear the allocator first (M2, single-threaded
with the global-statics TLS fallback), then real TLS the moment a second thread shares an aspace
(M3). The clocks, the startup block, stdio, exit status, and stacks+guard pages are already
specified, so the genuinely new work is small and concentrated.

**Toolchain posture:** vendor `rust-lang/rust` at a pinned nightly, build std via `-Zbuild-std`,
maintain the PAL out-of-tree, forward-port per release (the Xous cadence — ~6 weeks). Defer any
tier-3 upstreaming to M6 as optional.

**Global decisions (made once, no kernel work):**
- `panic = "abort"` initially — skips porting the unwinder; backtraces start `unsupported`.
- `OsStr` is bytes (matches §4.9 byte-equality names), not WTF-8.
- Locks use std's **thread-parker backend over notifications (§3.6)**, not a futex backend.
- `net` is permanently `unsupported` for now (non-goal, §8.1); `process::Command` stays thin.

---

## M0 — Toolchain & target *(likely already done)*

Your kernel/userspace already cross-compile to bare-metal aarch64, so this is mostly confirmation.

- [ ] Custom target JSON `aarch64-unknown-eunomia` based on `aarch64-unknown-none`, with
      `os = "eunomia"`, `panic-strategy = "abort"`, the right linker/relocation-model.
- [ ] `core` + `alloc` build under `-Zbuild-std=core,alloc`; `compiler_builtins` mem intrinsics present.
- [ ] A `#![no_std]` program boots under QEMU `virt` and prints via the kernel `debug-log` write syscall.

**Done when:** a `no_std` binary links against `core`/`alloc` for the target and prints in QEMU.

---

## M1 — std skeleton (all-unsupported)

Teach std the target exists; stand up the syscall seam.

- [ ] Vendor `rust-lang/rust` at a pinned nightly; record the commit.
- [ ] Add `target_os = "eunomia"` to `library/std/build.rs` supported-OS list.
- [ ] Copy `library/std/src/sys/pal/unsupported` → `.../pal/eunomia`; everything stubs to unsupported.
- [ ] Create `eunomia-sys` (the PAL↔OS seam): raw syscall wrappers, startup-block parser,
      named-grant lookup. This crate is the only place that knows the ABI; it belongs in the
      Verus "userspace runtime" chokepoint surface (§6) and its startup-block parser is a
      fuzz target under the untrusted-decode discipline (§3.7).

**Done when:** `std` compiles for the target with the all-unsupported PAL and `fn main(){}` links.

---

## M2 — Hello world (single-threaded) ⭐ headline milestone

First running std program. Clears the allocator blocker; defers TLS via the global-statics fallback.

**Spec additions required (small):**
- [ ] Add a `heap` standard named grant: spendable untyped the allocator may consume. (Mechanism
      exists in §2.5; this only pins a convention in the §5.1 named-grant table.)
- [ ] Decide `stderr`: add a `stderr` standard name, fold it into `stdout`, or route std's stderr
      to the `debug-log` write path. (Recommended start: route stderr to `debug-log`.)

**Implementation:**
- [ ] Runtime init / `_start` → `lang_start` → `main`: read the slot-0 bootstrap channel's first
      message, parse argv/env/named-grant table (§5.1).
- [ ] `alloc.rs`: real `GlobalAlloc`. Back `dlmalloc-rs` (already used by std on several OS-less
      targets) with a `sbrk`-style grow that retypes `heap` untyped → frame → maps via the
      pool-at-creation aspace (§2.5). Wire `System` / `#[global_allocator]`.
- [ ] `stdio.rs`: `Stdout`/`Stderr` write to the `debug-log` write syscall (simplest sink);
      `panic_output()` → same path.
- [ ] TLS: enable the single-threaded global-statics fallback (real TLS deferred to M3).
- [ ] **Time (free, do it now):** `Instant` ← `CNTVCT_EL0`; `SystemTime` ← time-page formula
      (§2.6). Zero new mechanism.

**Done when:** `println!`, `format!`, `Vec`/`Box`/`String`, and `Instant::now`/`SystemTime::now`
work in QEMU.

---

## M3 — Real threading, TLS, and synchronization

Enable multithreaded std. This is the only milestone that touches the kernel track.

**Spec/kernel additions required:**
- [ ] **TLS:** per-thread `TPIDR_EL0` save/restore in the context switch (the one trusted-core
      change, §6.1). Single-core keeps it trivial; still required once two threads share an aspace.
- [ ] **`yield` syscall** in the thread-lifecycle opcode set (§2.7) if absent — for
      `thread::yield_now` and the spin-before-park path.
- [ ] **Entropy source** for `hashmap_random_keys`: a startup-block seed grant (cheapest) or
      virtio-rng added to the device set (cleaner).

**Implementation:**
- [ ] Runtime TLS setup: local-exec/static model — point `TPIDR_EL0` at the `.tdata`/`.tbss`
      image; allocate + init a per-thread TLS block on spawn.
- [ ] `thread.rs`: spawn (retype untyped → thread, fund stack + guard page per §5.3, set
      entry/sp/aspace/cspace, start), join (bind on-exit notification, wait, read-report →
      `exited(status)`), `yield_now`, `sleep` (timer → notification), detach.
- [ ] `Parker` over a per-thread notification using the bind-poll-wait discipline (§3.6).
- [ ] Select the **parker-based** Mutex/Condvar/RwLock backend (not futex). Validate on host with
      Loom/Shuttle.
- [ ] Wire `hashmap_random_keys` → entropy source; `HashMap` now functional.

**Done when:** `thread::spawn`/`join`, `Mutex`/`Condvar`/`RwLock`, `mpsc`, `HashMap`, and
`thread::sleep` all work; the host Loom/Shuttle suite is green on the lock backend.

---

## M4 — Filesystem

Largest surface. Iterative — ship minimal open/read/write first, expand.

**Decisions (no kernel work):** root everything at the `root` grant; parse `Path` into the
component lists the wire protocol takes (§4.9); `File` = handle + client-side offset; leave
symlink, hardlink, cwd-relative, and inode-dependent metadata `unsupported` (no global root,
no ambient cwd, ephemeral file IDs).

- [ ] `fs.rs` minimal: `File::open`/`read`/`write`/`close` and `read_dir` against `root`, paths
      → component lists.
- [ ] Expand: `create`/`remove`/`rename`/`metadata` (synthesize what the storage protocol gives;
      stub inode-shaped fields).
- [ ] `path.rs`: separator + component handling consistent with byte names and the `/`-as-
      presentation rule.

**Done when:** open/read/write/create/remove/read_dir/rename work against the `root` handle from
ordinary `std::fs` code.

---

## M5 — Process, env, and stdio polish

Make std good enough to rewrite the shell and userspace tools against it.

- [ ] `process.rs`: `process::exit(code)` → main-thread exit with `exited(status)`; `abort` →
      `panic=abort` path. Leave `Command` thin/`unsupported`; expose a native capability-rich
      spawn API instead of pretending to be fork/exec.
- [ ] `env`: argv/env already parsed (M2); `temp_dir` → `tmp` grant; `current_dir`/`set_current_dir`
      `unsupported` (no ambient cwd) or shell-emulated.
- [ ] stdio switch: move `stdout`/`stdin` to the **console channel** (the user-facing path, §7);
      implement `stdin` read; keep `debug-log` for panics only.
- [ ] `net.rs`: confirm clean `unsupported` returns; document.

**Done when:** the shell and at least one userspace tool build and run against std (not raw
syscalls), with stdio over the console channel.

---

## M6 — Hardening, testing, forward-port discipline

- [ ] Run the libcore/liballoc/libstd test suites on-target via QEMU; triage and record skips.
- [ ] Promote the startup-block parser and PAL marshalling into the Verus userspace-runtime
      surface (§6); add fuzz corpora for the parser.
- [ ] Write the forward-port runbook: pinned-nightly bump cadence, the diff surface, regression set.
- [ ] *(Optional)* Upstream a tier-3 target: target spec in `rustc_target`, `sys/pal/eunomia`
      in-tree.

**Done when:** the chosen test subset is green, the forward-port runbook exists, and (optionally)
the tier-3 target is submitted.

---

## Spec-change schedule (consolidated)

| Change | Kind | Lands at |
|---|---|---|
| `heap` named grant (spendable untyped for the allocator) | convention (§5.1) | **M2** |
| `stderr` story (name / fold / debug-log) | convention | **M2** |
| `TPIDR_EL0` per-thread save/restore | kernel, trusted core (§6.1) | **M3** |
| `yield` syscall (if absent) | kernel, thin shell (§2.7) | **M3** |
| Entropy source (seed grant or virtio-rng) | kernel/device or §5.1 grant | **M3** |
| Console-channel stdio (move off debug-log) | convention (§7) | **M5** |

Everything else std needs is already specified: clocks (§2.6), the startup block with
`root`/`stdin`/`stdout`/`tmp`/`storage`/`time` and argv/env (§5.1), `exited(status)` (§5.1),
stacks with guard pages (§5.3), and the target/`-Zbuild-std` toolchain (§7).

## Dependency order

```
M0 ─ M1 ─ M2 ─ M3 ─ M4 ─ M5 ─ M6
              │    │
   time glue ─┘    └─ parker/locks depend on TLS + yield
   (do in M2)
```

M4 (fs) depends only on M2 (needs the allocator and storage connector), not on M3 — it can run in
parallel with M3 if you want, since storage sessions are syscall/IPC, not threading. The clock glue
is trivial and is pulled forward into M2.
