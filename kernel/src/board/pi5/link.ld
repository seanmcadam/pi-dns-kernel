ENTRY(_start)

BOOT_PI4_FLAG = 0;
KERNEL_PHYS = 0x200000;
/* Zero so the Pi 5 firmware keeps treating kernel_2712.img as a raw
   binary and honors kernel_address=0x200000. */
BOOT_ARM64_MAGIC = 0;

SECTIONS
{
    /* The Pi 5 firmware loads kernel_2712.img at physical 0x200000.
       kernel_address=0x200000 in config.txt asks for that address explicitly.
       The link address is that physical address in the upper half (TTBR1).
       boot.s drops EL2 to EL1 before the MMU is on. This image has not
       been tested on hardware. */
    . = 0xFFFF000000200000;
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

    /* Virtio rings are not linked into the Pi 5 image. The symbols stay so
       the MMU code can describe an empty DMA window. */
    .dma ALIGN(4096) (NOLOAD) :
    {
        __dma_start = .;
        . = ALIGN(64);
        *(.dma.mbox)
        . = ALIGN(64);
        *(.dma)
        *(.dma.*)
        . = ALIGN(4096);
        __dma_end = .;
    }

    /* Heap sits after the image. The stack grows down from 0x200000.
       The MMU leaves one guard page unmapped under the 256 KiB stack.
       0x200000 - 256 KiB is still above the ATF reservation at 0..0x80000. */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x200000; /* 2MB heap */
    __heap_end = .;
}
