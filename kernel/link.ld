ENTRY(_start)

SECTIONS
{
    /* Kernel starts at 0x80000 for Raspberry Pi 4 */
    . = 0x80000;

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

    .bss (NOLOAD) :
    {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        __bss_end = .;
    }

    /* Stack grows downward from 0x80000 */
    . = ALIGN(16);
    __stack_end = .;
    
    /* Heap starts after BSS */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x100000; /* 1MB heap */
    __heap_end = .;
}
