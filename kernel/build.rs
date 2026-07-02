use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a userspace package (its own mini-workspace under user/) for the
/// bare-metal target and return the ELF path. Separate target dir so the
/// nested cargo doesn't fight the outer one's lock.
fn build_user(
    root: &Path,
    target_dir: &Path,
    pkg: &str,
    bin: &str,
    envs: &[(&str, String)],
) -> PathBuf {
    // Custom JSON target: cargo wants an absolute path for `--target` (the
    // build runs in user/<pkg>), but names the artifact dir by the file stem.
    let triple = "aarch64-unknown-eunomia";
    let spec = root.join("targets").join(format!("{triple}.json"));
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root.join("user").join(pkg))
        .args(["build", "--release", "--target"])
        .arg(&spec)
        // `--target <path>.json` is gated behind this unstable cargo flag.
        .arg("-Zjson-target-spec")
        .arg("-Zbuild-std=core,compiler_builtins,alloc,std,panic_abort")
        .arg("-Zbuild-std-features=compiler-builtins-mem")
        .arg("--target-dir")
        .arg(target_dir)
        // Build std from the vendored fork (which carries the eunomia PAL +
        // restricted_std allowlist entry), not rustup's pristine rust-src. This
        // cargo override names the std workspace directory directly (the dir
        // whose Cargo.toml is the library workspace), i.e. vendor/rust/library —
        // not the monorepo root, whose Cargo.toml is the whole-compiler workspace.
        .env(
            "__CARGO_TESTS_ONLY_SRC_ROOT",
            root.join("vendor").join("rust").join("library"),
        )
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR");
    // The pinned nightly (kernel/rust-toolchain.toml) flows to this sub-build for
    // free: we invoke the active toolchain's `cargo` (CARGO) and inherit its
    // `RUSTC`/`RUSTUP_TOOLCHAIN`, none of which are scrubbed above — so the
    // sub-build uses the same toolchain regardless of its user/<pkg> cwd.
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("spawning cargo for {pkg}: {e}"));
    assert!(status.success(), "building user/{pkg} failed");
    target_dir.join(triple).join("release").join(bin)
}

/// Build an on-target **libtest** binary: a `user/<pkg>` mini-crate
/// whose `[[test]]` target compiles the vendored upstream `coretests`/`alloctests`
/// suite. Differs from [`build_user`] in that a test target lands in `deps/` under a
/// content-hashed name, so we drive `cargo test --no-run` with JSON output, extract
/// the artifact's `executable` path, and copy it to a stable `release/<test>` name the
/// runner script can find. `test` joins the build-std set (its `libc`/`process`/
/// `os::unix` uses are all `cfg(unix)`-gated, so they compile out on this non-unix
/// target). Guarded by the caller behind `EUNOMIA_BUILD_LIBTESTS` because these
/// suites are large and only the libtest runner needs them.
fn build_user_test(
    root: &Path,
    target_dir: &Path,
    pkg: &str,
    test: &str,
    envs: &[(&str, String)],
) -> PathBuf {
    let triple = "aarch64-unknown-eunomia";
    let spec = root.join("targets").join(format!("{triple}.json"));
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root.join("user").join(pkg))
        .args(["test", "--no-run", "--release", "--target"])
        .arg(&spec)
        .arg("-Zjson-target-spec")
        // `test` (libtest) joins build-std so the suite links a test harness.
        .arg("-Zbuild-std=core,compiler_builtins,alloc,std,panic_abort,test")
        .arg("-Zbuild-std-features=compiler-builtins-mem")
        // Build the test harness with panic=abort. Without this, cargo builds the
        // test/bench profile as panic=unwind, so build-std produces a *second* core
        // (the unwind variant) and the non-sysroot deps (serde_core/verus_builtin, via
        // eunomia-sys) link the wrong one — a duplicate-lang-item (E0152) failure. The
        // runtime consequence (libtest defaults to subprocess-per-test) is overridden
        // at run time by `--force-run-in-process`.
        .arg("-Zpanic-abort-tests")
        .args(["--test", test])
        // Machine-readable so we can read the hashed test-binary path back.
        .arg("--message-format=json")
        .arg("--target-dir")
        .arg(target_dir)
        .env(
            "__CARGO_TESTS_ONLY_SRC_ROOT",
            root.join("vendor").join("rust").join("library"),
        )
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawning cargo test for {pkg}: {e}"));
    assert!(
        out.status.success(),
        "building user/{pkg} test failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Find the compiler-artifact line for our test target and pull its `executable`.
    // The value is an absolute unix path (no JSON-escaped chars), so a substring cut
    // is enough — no JSON parser in the build dep graph. Scan from the end: the test
    // target is the last artifact emitted (its deps come first).
    let needle = format!("/deps/{test}-");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let exe = stdout
        .lines()
        .rev()
        .find_map(|line| {
            let marker = "\"executable\":\"";
            let start = line.find(marker)? + marker.len();
            let rest = &line[start..];
            let end = rest.find('"')?;
            let path = &rest[..end];
            path.contains(&needle).then(|| PathBuf::from(path))
        })
        .unwrap_or_else(|| panic!("no test executable for user/{pkg} in cargo json output"));
    // Copy to a stable, unhashed name the runner stages onto the disk image.
    let dst = target_dir.join(triple).join("release").join(test);
    std::fs::copy(&exe, &dst)
        .unwrap_or_else(|e| panic!("copying {} -> {}: {e}", exe.display(), dst.display()));
    dst
}

/// The newest file modification time anywhere under `dir` (recursively), or
/// `UNIX_EPOCH` for an empty/absent tree. Used to tell when the vendored std source
/// has been edited relative to the last build-std artifact.
fn newest_mtime(dir: &Path) -> std::time::SystemTime {
    let mut newest = std::time::UNIX_EPOCH;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(md) = e.metadata() else { continue };
            if md.is_dir() {
                stack.push(e.path());
            } else if let Ok(m) = md.modified() {
                if m > newest {
                    newest = m;
                }
            }
        }
    }
    newest
}

/// Whether the cached build-std `libstd` in `deps` is older than the vendored std
/// source — i.e. an edit to a `sys/pal/eunomia` arm would otherwise link a **stale**
/// std. `-Zbuild-std` fingerprints the toolchain, not the redirected
/// `__CARGO_TESTS_ONLY_SRC_ROOT` source, so it never rebuilds std on a source edit;
/// this is the signal to invalidate its cache (see `main`). Returns `false` when no
/// `libstd` is built yet (a fresh tree — `build_user` will compile it) so we never wipe
/// needlessly. Compares against the **oldest** `libstd-*.rlib` so any stale variant
/// triggers the rebuild.
fn build_std_is_stale(std_src: &Path, deps: &Path) -> bool {
    let mut oldest_std: Option<std::time::SystemTime> = None;
    let Ok(entries) = std::fs::read_dir(deps) else {
        return false; // no deps dir yet ⇒ nothing cached to invalidate
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("libstd-") && name.ends_with(".rlib") {
            if let Ok(m) = e.metadata().and_then(|md| md.modified()) {
                oldest_std = Some(oldest_std.map_or(m, |o| o.min(m)));
            }
        }
    }
    match oldest_std {
        Some(built) => newest_mtime(std_src) > built,
        None => false, // std not built yet ⇒ build_user will build it fresh
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-link-arg=-T{}/linker.ld", manifest.display());
    println!("cargo:rerun-if-changed=linker.ld");

    let root = manifest.parent().unwrap();
    for dep in [
        "targets/aarch64-unknown-eunomia.json",
        // The vendored std the user builds compile via build-std; an edit here (or a
        // submodule bump) must re-spawn them. NOTE: this only makes *build.rs* rerun —
        // `-Zbuild-std` still caches std and won't rebuild it on a source edit, so the
        // `build_std_is_stale` wipe below is what actually forces the std rebuild.
        "vendor/rust/library/std/src",
        "user/hello",
        "user/selftest",
        "user/stdsmoke",
        "user/stdfs",
        "user/stdio",
        "user/init",
        "user/storaged",
        "user/shell",
        "user/console",
        // On-target libtest suites, built only under
        // EUNOMIA_BUILD_LIBTESTS. Track the mini-crates and the vendored test
        // sources they compile so a change re-invokes build.rs.
        "user/coretests",
        "user/alloctests",
        "vendor/rust/library/coretests/tests",
        "vendor/rust/library/alloctests/tests",
        // The PAL↔seam crate every std user binary links; an edit here must rebuild
        // them (build.rs only reruns build_user on a tracked change).
        "eunomia-sys/src",
        "ipc/src",
        "loader/src",
        "urt/src",
        "cas/src",
        "dma-pool/src",
        "virtio-blk/src",
        "storage-server/src",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(dep).display());
    }
    // Toggling the libtest opt-in must re-run build.rs so the suites get built (or
    // skipped) accordingly.
    println!("cargo:rerun-if-env-changed=EUNOMIA_BUILD_LIBTESTS");

    let user_target = root.join("target").join("user");

    // Force a std rebuild when the vendored std source was edited. `-Zbuild-std`
    // fingerprints the toolchain, not the `__CARGO_TESTS_ONLY_SRC_ROOT`-redirected
    // source, so it silently links a **stale** std after an edit to a `sys/pal/eunomia`
    // arm — a debugging trap (stdout still "works" via the old path, stdin reads EOF).
    // The `rerun-if-changed` above reruns build.rs on such an edit; here we invalidate
    // build-std's cache by removing the per-binary target dir so the `build_user` calls
    // below recompile std from the current source. Only fires when the vendored std is
    // newer than the built `libstd` (an actual edit / submodule bump), so steady-state
    // builds pay only a cheap tree walk. Edits to the vendored `core`/`alloc` (outside
    // `std/src`, and not part of this port's surface) still need a manual
    // `rm -rf target/user`.
    let std_src = root.join("vendor/rust/library/std/src");
    let deps = user_target
        .join("aarch64-unknown-eunomia")
        .join("release")
        .join("deps");
    if build_std_is_stale(&std_src, &deps) {
        let _ = std::fs::remove_dir_all(&user_target);
    }

    let hello = build_user(root, &user_target, "hello", "hello", &[]);
    let selftest = build_user(root, &user_target, "selftest", "selftest", &[]);
    // The std runtime GATE fixture: the first std user
    // binary, copied onto the demo disk by scripts/std-smoke-test.sh.
    let stdsmoke = build_user(root, &user_target, "stdsmoke", "stdsmoke", &[]);
    // The fs GATE fixture: the std fs client,
    // copied onto the demo disk by scripts/fs-smoke-test.sh.
    let stdfs = build_user(root, &user_target, "stdfs", "stdfs", &[]);
    // The console GATE fixture: the std console
    // demonstrator, copied onto the demo disk by scripts/std-smoke-test.sh.
    let stdio = build_user(root, &user_target, "stdio", "stdio", &[]);
    let storaged = build_user(root, &user_target, "storaged", "storaged", &[]);
    // On-target libtest suites, built only when the runner opts in via
    // EUNOMIA_BUILD_LIBTESTS (they are large — several minutes and multi-MiB ELFs — so
    // unrelated builds and the other smoke scripts do not pay for them). Built BEFORE the
    // shell so their ELF paths can be embedded into it: `run bin/{coretests,alloctests}`
    // then spawns from the shell's `.rodata`, bypassing the store — the MVP fs read path
    // reconstructs the whole file per 256-byte request, so a multi-MiB test binary is
    // impractical to load from disk (storaged OOM + O(n²)). 16 MiB child heap covers the
    // suites' peak allocation (raise if an allocator abort — not a test failure — appears).
    let test_bins = std::env::var_os("EUNOMIA_BUILD_LIBTESTS")
        .is_some()
        .then(|| {
            // 16 MiB covers both a single module's peak and a whole-suite run: libtest frees
            // each test's resources before the next, so the live set is bounded (~one test),
            // not cumulative across the thousands of tests. The child's segments + this `.bss`
            // must also fit the 48 MiB shell donation (`DONATION_BYTES`), which 16 MiB does
            // comfortably (32 MiB did not — the segments overran the donation → spawn failed).
            let heap = [("EUNOMIA_HEAP_BYTES", "16777216".to_string())];
            (
                build_user_test(root, &user_target, "coretests", "coretests", &heap),
                build_user_test(root, &user_target, "alloctests", "alloctests", &heap),
            )
        });

    // The shell is a std binary: size its `System` heap above the 1 MiB
    // default via `EUNOMIA_HEAP_BYTES`, threaded here into the sub-build that compiles
    // `eunomia-sys` for it (parsed by `eunomia-sys/src/heap.rs`'s `option_env!`). It loads
    // whole child ELFs into this heap on `run` (holding the Vec for the child's lifetime,
    // with a transient ~2x peak while `std::fs::read` grows it), so it wants headroom above
    // the largest store-loaded child. It stays small on purpose: init carves the shell's
    // whole aspace (this `.bss` heap + a ~100 MiB POOL untyped) from its own 127 MiB boot
    // untyped, so an oversized shell heap overflows init's budget and fails the spawn. The
    // libtest suites are embedded (see above) rather than loaded, so they do not bound it.
    // Still a reservation (committed at spawn, no demand paging). Only std binaries honor it.
    let mut shell_env = vec![("EUNOMIA_HEAP_BYTES", "8388608".to_string())];
    if let Some((coretests, alloctests)) = &test_bins {
        shell_env.push(("CORETESTS_ELF_PATH", coretests.display().to_string()));
        shell_env.push(("ALLOCTESTS_ELF_PATH", alloctests.display().to_string()));
    }
    let shell = build_user(root, &user_target, "shell", "ushell", &shell_env);
    let console = build_user(root, &user_target, "console", "console", &[]);
    let init = build_user(
        root,
        &user_target,
        "init",
        "init",
        &[
            ("STORAGED_ELF_PATH", storaged.display().to_string()),
            ("SHELL_ELF_PATH", shell.display().to_string()),
            // The console ELF is built and its path passed to init; init
            // consumes it (include_bytes!) when it spawns the console.
            ("CONSOLE_ELF_PATH", console.display().to_string()),
        ],
    );
    // hello + selftest + stdsmoke + stdfs + stdio are placed into the demo disk image by
    // the scripts (scripts/run-demo.sh, scripts/spawn-test.sh,
    // scripts/std-smoke-test.sh, scripts/fs-smoke-test.sh); they are loaded from the
    // store at runtime, not embedded in the kernel.
    let _ = (hello, selftest, stdsmoke, stdfs, stdio, test_bins);
    println!("cargo:rustc-env=INIT_ELF_PATH={}", init.display());
}
