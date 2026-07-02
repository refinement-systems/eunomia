include!("../build_common.rs");

fn main() {
    rerun_inputs();
    // The custom linker script + EL0 page size are meaningful only for the bare-metal
    // aarch64 image (the kernel-built binary). For host test builds (`cargo test` under
    // cfg(test)) they must NOT be applied — they would break the libtest harness link
    // with `cc`. Gate on the bare-metal target, and scope to bin targets so a host test
    // harness never receives them.
    if is_bare_metal() {
        link_el0_image_bins();
    }
}
