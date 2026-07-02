fn main() {
    println!("cargo:rerun-if-changed=link.ld");
    println!("cargo:rustc-check-cfg=cfg(libtests)");
    // On-target library-test triage (std-port 6.1): when kernel/build.rs builds the
    // coretests/alloctests suites (under EUNOMIA_BUILD_LIBTESTS), it passes their ELF
    // paths here so the shell embeds them and spawns from `.rodata` on `run
    // bin/{coretests,alloctests}`. This bypasses the store: the MVP fs read path
    // reconstructs the whole file per 256-byte request, so a multi-MiB test binary is
    // impractical to load from disk (OOM + O(n^2)). Absent the env, the `libtests` cfg
    // is off and the shell embeds nothing (no size or behavior change).
    println!("cargo:rerun-if-env-changed=CORETESTS_ELF_PATH");
    println!("cargo:rerun-if-env-changed=ALLOCTESTS_ELF_PATH");
    if let (Ok(c), Ok(a)) = (
        std::env::var("CORETESTS_ELF_PATH"),
        std::env::var("ALLOCTESTS_ELF_PATH"),
    ) {
        println!("cargo:rustc-cfg=libtests");
        println!("cargo:rustc-env=CORETESTS_ELF_PATH={c}");
        println!("cargo:rustc-env=ALLOCTESTS_ELF_PATH={a}");
    }
    // The bare-metal linker script + page-size flag are for the Eunomia target
    // only (`target_os = "none"` or `"eunomia"`). A host build — the test
    // harness — links with the platform default; applying the script there
    // breaks the link (`clang: unknown argument: -zmax-page-size`). The gate
    // leaves the shipped aarch64 binary's link args alone.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "none" && os != "eunomia" {
        return;
    }
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg=-T{dir}/link.ld");
    println!("cargo:rustc-link-arg=-zmax-page-size=4096");
}
