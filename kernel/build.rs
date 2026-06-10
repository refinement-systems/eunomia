use std::path::{Path, PathBuf};
use std::process::Command;

/// Build a userspace package (its own mini-workspace under user/) for the
/// bare-metal target and return the ELF path. Separate target dir so the
/// nested cargo doesn't fight the outer one's lock.
fn build_user(root: &Path, target_dir: &Path, pkg: &str, envs: &[(&str, String)]) -> PathBuf {
    let triple = "aarch64-unknown-none-softfloat";
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(root.join("user").join(pkg))
        .args(["build", "--release", "--target", triple])
        .arg("-Zbuild-std=core,compiler_builtins")
        .arg("-Zbuild-std-features=compiler-builtins-mem")
        .arg("--target-dir")
        .arg(target_dir)
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env_remove("CARGO_TARGET_DIR");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd.status().unwrap_or_else(|e| panic!("spawning cargo for {pkg}: {e}"));
    assert!(status.success(), "building user/{pkg} failed");
    target_dir.join(triple).join("release").join(pkg)
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-link-arg=-T{}/linker.ld", manifest.display());
    println!("cargo:rerun-if-changed=linker.ld");

    let root = manifest.parent().unwrap();
    for dep in [
        "user/hello/src",
        "user/hello/link.ld",
        "user/hello/Cargo.toml",
        "user/init/src",
        "user/init/link.ld",
        "user/init/Cargo.toml",
        "ipc/src",
        "loader/src",
    ] {
        println!("cargo:rerun-if-changed={}", root.join(dep).display());
    }

    let user_target = root.join("target").join("user");
    let hello = build_user(root, &user_target, "hello", &[]);
    let init = build_user(
        root,
        &user_target,
        "init",
        &[("HELLO_ELF_PATH", hello.display().to_string())],
    );
    println!("cargo:rustc-env=INIT_ELF_PATH={}", init.display());
}
