// SPDX-License-Identifier: 0BSD
include!("../build_common.rs");

fn main() {
    rerun_inputs();
    println!("cargo:rustc-check-cfg=cfg(libtests)");
    // On-target library-test triage: when kernel/build.rs builds the
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
    // The bare-metal linker script + page-size flag are for the Eunomia target only. A
    // host build — the test harness — links with the platform default; applying the
    // script there breaks the link (`clang: unknown argument: -zmax-page-size`). Unlike
    // the bin-scoped crates above, shell links plain args and guards the host path here.
    if is_bare_metal() {
        link_el0_image();
    }
}
