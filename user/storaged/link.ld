/* Eunomia userspace: statically linked at the process VA base (rev2§5).
 * Each permission class (RX / R / RW) starts on a 4 KiB boundary so the
 * PT_LOAD segments never share a page — the loader maps per segment. */
ENTRY(_start)

SECTIONS {
    . = 0x80000000;
    .text : { *(.text._start) *(.text .text.*) }
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    .data : { *(.data .data.*) }
    .bss : { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : { *(.comment) *(.note.*) *(.eh_frame*) }
}
