// Permission to use, copy, modify, and/or distribute this software for
// any purpose with or without fee is hereby granted.
//
// THE SOFTWARE IS PROVIDED “AS IS” AND THE AUTHOR DISCLAIMS ALL
// WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES
// OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE
// FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY
// DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN
// AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT
// OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

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
