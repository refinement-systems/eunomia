fn main() {
    println!("cargo:rerun-if-changed=link.ld");
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
