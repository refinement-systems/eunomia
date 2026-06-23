fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    // The custom linker script + page size are meaningful only for the
    // bare-metal aarch64 EL0 image (the kernel-built binary). For host test
    // builds (the PL011-layer proptests under cfg(test)) they must NOT be
    // applied — they would break the libtest harness link with `cc`. Gate on
    // the bare-metal `*-none` target, and scope to bin targets so a host test
    // harness never receives them. (Without this the console links at the
    // default 0x200000 instead of the rev2§5 process base 0x80000000, so
    // `spawn::prepare` maps it at the wrong VA.)
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("-none") {
        println!("cargo:rustc-link-arg-bins=-T{dir}/link.ld");
        println!("cargo:rustc-link-arg-bins=-zmax-page-size=4096");
    }
    println!("cargo:rerun-if-changed=link.ld");
}
