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

use core::arch::global_asm;

global_asm!(
    r#"
    .section ".text.boot", "ax"
    .global _start
_start:
    /* Halt every core except MPIDR[7:0] == 0. */
    mrs     x0, mpidr_el1
    and     x0, x0, #0xFF
    cbz     x0, .Lprimary
.Lhalt:
    wfe
    b       .Lhalt

.Lprimary:
    /* Switch SP selector to SP_EL1. */
    mov     x0, #1
    msr     spsel, x0
    isb

    /* Load the top-of-stack address from the linker symbol. */
    ldr     x0, =__stack_top
    mov     sp, x0

    /* Zero .bss — linker guarantees 16-byte alignment on both ends. */
    ldr     x0, =__bss_start
    ldr     x1, =__bss_end
    cmp     x0, x1
    beq     .Lbss_done
.Lbss_loop:
    stp     xzr, xzr, [x0], #16
    cmp     x0, x1
    blt     .Lbss_loop
.Lbss_done:
    bl      kernel_main

    /* kernel_main must not return; spin just in case. */
.Lspin:
    wfe
    b       .Lspin
    "#
);
