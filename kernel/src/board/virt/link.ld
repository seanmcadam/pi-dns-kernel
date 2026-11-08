ENTRY(_start)

SECTIONS
{
    /* QEMU loads the raw image at physical 0x40080000 (RAM 0x40000000 + 0x80000).
       The link address is that physical address with the upper bits set, so
       the kernel runs from TTBR1. Bits 47:0 still equal the physical address.
       objcopy -O binary starts at the lowest LMA, so the file stays the size
       of the image. The Pi 4 image uses board/pi4/link.ld. */
    . = 0xFFFF000040080000;
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

    /* Virtio rings and buffers. Mapped Normal non-cacheable once the MMU is on. */
    .dma ALIGN(4096) (NOLOAD) :
    {
        __dma_start = .;
        *(.dma)
        *(.dma.*)
        . = ALIGN(4096);
        __dma_end = .;
    }

    /* Heap sits in RAM, after the image. Stack grows down from 0x40080000.
       The MMU leaves one guard page unmapped under the 256 KiB stack. */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x200000; /* 2MB heap */
    __heap_end = .;
}
