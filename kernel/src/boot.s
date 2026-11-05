.section ".text.boot"

.global _start

_start:
    // Read CPU ID, stop cores 1-3
    mrs     x1, mpidr_el1
    and     x1, x1, #3
    cbz     x1, 2f
    
    // CPU ID > 0, hang
1:
    wfe
    b       1b

2:
    // Enable FP/SIMD (CPACR_EL1)
    mrs     x1, cpacr_el1
    orr     x1, x1, #(3 << 20)   // FPEN bits to 0b11
    msr     cpacr_el1, x1
    isb
    
    // Set stack pointer
    ldr     x1, =0x80000
    mov     sp, x1
    
    // Clear BSS
    ldr     x1, =__bss_start
    ldr     x2, =__bss_end

3:
    cmp     x1, x2
    b.hs    4f
    str     xzr, [x1], #8
    b       3b

4:
    // Jump to Rust
    bl      kernel_main
    
    // Halt if kernel_main returns
5:
    wfe
    b       5b
