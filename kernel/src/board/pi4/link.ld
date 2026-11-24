ENTRY(_start)

BOOT_PI4_FLAG = 1;
KERNEL_PHYS = 0x80000;
/* Magic "ARM\x64". Only this image advertises the Linux arm64 header.
   Virt and Pi 5 keep the field zero so their loaders still treat the
   file as a raw binary. */
BOOT_ARM64_MAGIC = 0x644d5241;

SECTIONS
{
    /* The Pi 4 firmware loads the raw kernel8.img at physical 0x80000.
       The link address is that physical address in the upper half (TTBR1).
       boot.s does drop EL2 to EL1 before the MMU is on. This image has not
       been tested on hardware. */
    . = 0xFFFF000000080000;
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
    }

    /* Claim these so they stay ahead of the boot id. Otherwise the linker
       drops them after .bootid and the id is no longer the file tail. */
    .eh_frame_hdr : { *(.eh_frame_hdr) *(.eh_frame_hdr.*) }
    .eh_frame : { *(.eh_frame) *(.eh_frame.*) }

    /* Last bytes of the file. A Pi 4 boot reads this tail from KERNEL8.IMG
       and KERNEL8.NEW to see which one is running. */
    .bootid :
    {
        KEEP(*(.bootid))
        KEEP(*(.bootid.*))
        . = ALIGN(8);
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
    /* .dma.mbox is the HDMI property buffer. It is placed first, on its
       own cache lines, so a later .dma object cannot share that line.
       Cleaning a shared line after the GPU replies writes the neighbor
       back over the answer. */
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

    /* Heap sits after the image. The stack grows down from 0x80000.
       The MMU leaves one guard page unmapped under the 256 KiB stack. */
    . = ALIGN(4096);
    __heap_start = .;
    . += 0x200000; /* 2MB heap */
    __heap_end = .;

    /* Survives a watchdog reset. Not in the image, not in .bss, not zeroed.
       32 MiB is outside the kernel's fine map and below the GPU split. */
    .crash 0xFFFF000002000000 (NOLOAD) :
    {
        __crash_start = .;
        . += 8192;
        __crash_end = .;
    }

    /* Domain table. Above the crash slot and the kernel image, so a
       larger image cannot collide with it. Not loaded, not zeroed. */
    .block 0xFFFF000004000000 (NOLOAD) :
    {
        __block_start = .;
        . += 0x2000000; /* 32 MiB */
        __block_end = .;
    }
    ASSERT(__block_start > __heap_end, "block region overlaps the heap")
    ASSERT(__block_start > __crash_end, "block region overlaps crash")
}
