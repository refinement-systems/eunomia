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

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-link-arg=-T{}/linker.ld", manifest.display());
    println!("cargo:rerun-if-changed=linker.ld");

    let root = manifest.parent().unwrap();
    for dep in [
        "targets/aarch64-unknown-eunomia.json",
        // The vendored std the user builds compile via build-std; an edit here
        // (or a submodule bump) must re-spawn them.
        "vendor/rust/library/std/src",
        "user/hello",
        "user/selftest",
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
    let hello = build_user(root, &user_target, "hello", "hello", &[]);
    let selftest = build_user(root, &user_target, "selftest", "selftest", &[]);
    let storaged = build_user(root, &user_target, "storaged", "storaged", &[]);
    let shell = build_user(root, &user_target, "shell", "ushell", &[]);
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
    // hello + selftest are placed into the demo disk image by the scripts
    // (scripts/run-demo.sh, scripts/spawn-test.sh); they are loaded from the
    // store at runtime, not embedded in the kernel.
    let _ = (hello, selftest);
    println!("cargo:rustc-env=INIT_ELF_PATH={}", init.display());
}
