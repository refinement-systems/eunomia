/* QEMU virt machine, AArch64: DRAM starts at 0x40000000.
 * QEMU places the DTB at 0x40000000-0x40100000 (up to 1 MiB).
 * Load the kernel at 0x40200000 to leave room for DTB. */
ENTRY(_start)

MEMORY {
    DRAM (rwx) : ORIGIN = 0x40200000, LENGTH = 254M
}

SECTIONS {
    . = ORIGIN(DRAM);

    /* Boot entry must be first so ELF entry == _start. */
    .text : {
        KEEP(*(.text.boot))
        . = ALIGN(2048);
        KEEP(*(.text.vectors))
        *(.text .text.*)
    } > DRAM

    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    } > DRAM

    .data : ALIGN(4096) {
        *(.data .data.*)
    } > DRAM

    .bss : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    } > DRAM

    /* 64 KiB kernel stack, 16-byte aligned. */
    .stack (NOLOAD) : ALIGN(16) {
        __stack_bottom = .;
        . += 0x10000;
        __stack_top = .;
    } > DRAM

    /* Discard unwanted sections that trip up the bare-metal linker. */
    /DISCARD/ : {
        *(.comment)
        *(.note.*)
        *(.eh_frame*)
        *(.ARM.exidx*)
    }
}
