// SPDX-License-Identifier: 0BSD
include!("../build_common.rs");

fn main() {
    rerun_inputs();
    link_el0_image();
}
