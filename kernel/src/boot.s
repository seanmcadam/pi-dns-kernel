.section .text.boot, "ax"
.global _start
.type _start, %function

_start:
    // Only the boot core continues. QEMU virt starts one CPU at EL1.
    mrs     x1, mpidr_el1
    and     x1, x1, #3
    cbz     x1, 2f

1:
    wfe
    b       1b

2:
    // Rust's aarch64 target uses NEON for copies. QEMU boots with FP/SIMD
    // trapped, so enable it before any Rust code runs.
    mrs     x0, cpacr_el1
    orr     x0, x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb

    // Stack grows down from the kernel load address into the RAM below it.
    // QEMU: 0x40080000. Pi 4: 0x80000. The linker script defines the symbol.
    ldr     x1, =__stack_top
    mov     sp, x1

    // Clear BSS (linked into RAM, so these stores are real memory).
    ldr     x1, =__bss_start
    ldr     x2, =__bss_end

3:
    cmp     x1, x2
    b.hs    4f
    str     xzr, [x1], #8
    b       3b

4:
    bl      kernel_main

5:
    wfe
    b       5b
