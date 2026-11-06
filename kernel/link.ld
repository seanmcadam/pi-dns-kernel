ENTRY(_start)

SECTIONS
{
    /* QEMU's virt machine loads a raw kernel at RAM base + 0x80000.
       RAM starts at 0x40000000, so the image must be linked at 0x40080000.
       The Pi 4 image uses link-pi4.ld and is linked at 0x80000. */
    . = 0x40080000;
    __stack_top = .;

    .text :
    {
        KEEP(*(.text.boot))
        *(.text*)
    }

    .rodata :
    {
        *(.rodata*)
    }

    .data :
    {
        *(.data*)
    }

    .bss ALIGN(4096) (NOLOAD) :
    {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    }

    /* Heap sits in RAM, after the image. Stack grows down from 0x40080000. */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x200000; /* 2MB heap */
    __heap_end = .;
}
