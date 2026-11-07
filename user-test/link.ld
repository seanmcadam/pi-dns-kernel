ENTRY(_start)

SECTIONS
{
    /* Outside the kernel identity map on QEMU and on the Pi 4 table budget. */
    . = 0x100000000;
    .text : {
        *(.text.start)
        *(.text .text.*)
    }
    .rodata : {
        *(.rodata .rodata.*)
    }
    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }
}
