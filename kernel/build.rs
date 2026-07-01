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
    // The std-port Phase-2 GATE fixture (findings 7-1): the first std user
    // binary, copied onto the demo disk by scripts/std-smoke-test.sh.
    let stdsmoke = build_user(root, &user_target, "stdsmoke", "stdsmoke", &[]);
    // The std-port Phase-4.1 fs GATE fixture (findings #13): the std fs client,
    // copied onto the demo disk by scripts/fs-smoke-test.sh.
    let stdfs = build_user(root, &user_target, "stdfs", "stdfs", &[]);
    // The std-port Phase-5.1 console GATE fixture (findings #16): the std console
    // demonstrator, copied onto the demo disk by scripts/std-smoke-test.sh.
    let stdio = build_user(root, &user_target, "stdio", "stdio", &[]);
    let storaged = build_user(root, &user_target, "storaged", "storaged", &[]);
    // The shell is a std binary (std-port 5.3): size its `System` heap above the 1 MiB
    // default via `EUNOMIA_HEAP_BYTES`, threaded here into the sub-build that compiles
    // `eunomia-sys` for it (parsed by `eunomia-sys/src/heap.rs`'s `option_env!`). It loads
    // whole child ELFs into this heap on `run`, so it wants the headroom — 4 MiB is a
    // reservation (committed RAM at spawn, no demand paging) comfortably within `-m 256M`.
    // Only std binaries honor it; the no_std ones never compile `heap.rs`.
    let shell = build_user(
        root,
        &user_target,
        "shell",
        "ushell",
        &[("EUNOMIA_HEAP_BYTES", "4194304".to_string())],
    );
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
    let _ = (hello, selftest, stdsmoke, stdfs, stdio);
    println!("cargo:rustc-env=INIT_ELF_PATH={}", init.display());
}
