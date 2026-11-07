ENTRY(_start)

SECTIONS
{
    /* The Pi 4 firmware loads kernel8.img at 0x80000.
       This link is untested on hardware. There is no EL2-to-EL1 drop yet. */
    . = 0x80000;
    __stack_top = .;

    .text :
    {
        __text_start = .;
        KEEP(*(.text.boot))
        . = ALIGN(2048);
        KEEP(*(.text.vectors))
        *(.text*)
        . = ALIGN(4096);
        __text_end = .;
    }

    .rodata :
    {
        __rodata_start = .;
        *(.rodata*)
        . = ALIGN(4096);
        __rodata_end = .;
    }

    .data :
    {
        __data_start = .;
        *(.data*)
        . = ALIGN(4096);
        __data_end = .;
    }

    .bss ALIGN(4096) (NOLOAD) :
    {
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        . = ALIGN(4096);
        __bss_end = .;
    }

    /* Virtio rings are not linked into the Pi 4 image. The symbols stay so
       the MMU code can describe an empty DMA window. Untested on hardware. */
    .dma ALIGN(4096) (NOLOAD) :
    {
        __dma_start = .;
        *(.dma)
        *(.dma.*)
        . = ALIGN(4096);
        __dma_end = .;
    }

    /* Heap sits after the image. The stack grows down from 0x80000.
       The MMU leaves one guard page unmapped under the 256 KiB stack. */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x200000; /* 2MB heap */
    __heap_end = .;
}
