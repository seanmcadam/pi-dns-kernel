ENTRY(_start)

SECTIONS
{
    /* The Pi 4 firmware loads kernel8.img at 0x80000. */
    . = 0x80000;
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

    .bss ALIGN(16) (NOLOAD) :
    {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    }

    /* Heap sits after the image. The stack grows down from 0x80000. */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x200000; /* 2MB heap */
    __heap_end = .;
}
