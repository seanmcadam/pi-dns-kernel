ENTRY(_start)

SECTIONS
{
    . = 0x100000000;
    .text :
    {
        KEEP(*(.text.start))
        *(.text .text.*)
    }
    .rodata :
    {
        *(.rodata .rodata.*)
    }
    . = 0x100090000;
    .data :
    {
        *(.data .data.*)
    }
    .bss :
    {
        *(.bss .bss.*)
        *(COMMON)
    }
    /DISCARD/ :
    {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.ARM.attributes)
    }
}
