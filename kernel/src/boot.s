// =============================================================================
// boot.s - First code that runs when the CPU jumps into the kernel image
// =============================================================================
//
// This file is pulled into the kernel by main.rs with
//     core::arch::global_asm!(include_str!("boot.s"));
// so rustc's built-in LLVM assembler assembles it. No external assembler or
// C compiler is involved.
//
// NOTE: global_asm! treats curly braces as format placeholders, so do not
// use brace characters anywhere in this file, not even in comments.
//
// Where this code ends up in memory
// ---------------------------------
// The linker scripts place the .text.boot section first in the image, so
// _start is the very first instruction:
//   - QEMU virt (kernel/link.ld):    0x40080000  (RAM base 0x40000000 + 0x80000)
//   - Raspberry Pi 4 (link-pi4.ld):  0x00080000  (where the Pi firmware loads
//                                                 kernel8.img by default)
//
// State of the CPU on entry
// -------------------------
//   - AArch64. QEMU virt with -kernel starts at EL1. QEMU virt with
//     virtualization=on starts at EL2. A real Pi 4 with arm_64bit=1 normally
//     hands over at EL2. The EL2 path below is untested on Pi 4 hardware.
//   - The MMU and caches are off, so every address is a physical address.
//   - There is no stack yet, so we cannot call any Rust code until we set
//     one up.
//   - Every core may jump here. On QEMU virt only one core runs. On the Pi 4
//     the firmware usually parks cores 1-3 itself, but we park them here too
//     to be safe.
//
// What this file does, in order
// -----------------------------
//   1. Work out which core is running and park every core except core 0.
//   2. If the core entered at EL2, drop to EL1h. If it is already at EL1,
//      skip that drop. x20 remembers the entry level for kernel_main.
//   3. Turn on the floating-point / SIMD (NEON) unit so Rust code doesn't trap.
//   4. Point the stack pointer at __stack_top.
//   5. Install the EL1 exception vectors in VBAR_EL1.
//   6. Zero .bss, then the .dma section used by virtio.
//   7. Call kernel_main() in main.rs, which never returns.
//   8. If kernel_main ever returns anyway, park the core forever.
// =============================================================================

// Put the following code in its own section called .text.boot.
//   "a" = allocatable (takes up space in the loaded image)
//   "x" = executable
// The linker scripts use KEEP(*(.text.boot)) so the linker never throws this
// section away and always puts it at the very start of .text.
.section .text.boot, "ax"

// Export _start so the linker can see it. ENTRY(_start) in the linker script
// makes it the ELF entry point.
.global _start

// Mark _start as a function symbol so debuggers and objdump show it nicely.
.type _start, %function

_start:
    // -------------------------------------------------------------------------
    // Step 1: Find out which core we are, and park all but core 0.
    // -------------------------------------------------------------------------
    // MPIDR_EL1 is the "Multiprocessor Affinity Register". Its lowest byte
    // (Aff0) holds the core number inside the cluster. The Cortex-A72 in the
    // Pi 4 has 4 cores, numbered 0-3, so the bottom 2 bits are enough.
    mrs     x1, mpidr_el1       // x1 = MPIDR_EL1 (read a system register)
    and     x1, x1, #3          // keep only bits [1:0], so x1 = core number 0..3
    cbz     x1, 2f              // core 0? branch forward to label 2 to boot.
                                // "2f" means "the next label 2, going forward".

    // Any core that is not core 0 falls through to here and sleeps forever.
1:
    wfe                         // Wait For Event: low-power sleep until an
                                // event or interrupt wakes the core.
    b       1b                  // woke up anyway? go back to sleep.
                                // "1b" means "the previous label 1, going back".
                                // To support multiple cores later, this is
                                // where a secondary core would instead pick up
                                // its own stack and jump into Rust.

2:
    // -------------------------------------------------------------------------
    // Step 2: Drop from EL2 to EL1, unless we are already at EL1.
    // -------------------------------------------------------------------------
    // CurrentEL bits [3:2] are the current exception level. Shift them down
    // so x20 holds 1, 2, or 3. x20 is not used again until the call into
    // Rust, and eret does not change general registers, so the value survives
    // the drop. kernel_main prints it.
    //
    // EL1 (plain QEMU virt) branches straight to label 8.
    // EL2 (QEMU virtualization=on, and a Pi 4 firmware handover) falls
    // through and erets to that same label. Any other level parks. The Pi 4
    // handover is untested on hardware.
    mrs     x20, currentel      // x20 = CurrentEL
    lsr     x20, x20, #2        // x20 = exception level 1, 2, or 3
    cmp     x20, #1
    b.eq    8f                  // already EL1: stack, vectors, and Rust
    cmp     x20, #2
    b.ne    1b                  // not EL1 or EL2: park with the other cores

    // HCR_EL2.RW, bit 31. The level below EL2 must be AArch64. Leaving this
    // clear would make eret enter EL1 as AArch32.
    movz    x0, #0x8000, lsl #16
    msr     hcr_el2, x0

    // CNTHCTL_EL2 bits [1:0] are EL1PCEN and EL1PCTEN. Set means EL1 may
    // use the physical timer and the physical counter without trapping to
    // EL2. CNTVOFF_EL2 is the virtual counter offset. Zero makes the virtual
    // counter equal the physical counter, so EL1 can use either timer.
    mrs     x0, cnthctl_el2
    orr     x0, x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr

    // VPIDR_EL2 and VMPIDR_EL2 are what EL1 reads as MIDR_EL1 and MPIDR_EL1.
    // Copy the real ids so EL1 still sees this core.
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0

    // CPTR_EL2.TFP traps EL1 floating-point to EL2, and this file does not
    // install an EL2 vector table (a trap there would hang). 0x33ff clears
    // the trap bits and sets the RES1 bits in CPTR_EL2.
    movz    x0, #0x33ff
    msr     cptr_el2, x0

    // SCTLR_EL1 reset value: MMU off, caches off, and the RES1 bits set
    // (bits 11, 20, 22, 23, 28, and 29). That constant is 0x30d00800.
    // The MMU code turns caches on later, from EL1.
    movz    x0, #0x0800
    movk    x0, #0x30d0, lsl #16
    msr     sctlr_el1, x0

    // SPSR_EL2 is the PSTATE restored by eret.
    //   bits [3:0] = 0b0101  EL1h, so EL1 uses SP_EL1
    //   bits [9:6] = 0b1111  D, A, I, and F all masked
    // 0x3c5 is those fields together. IRQs stay masked until Rust turns
    // them on.
    movz    x0, #0x3c5
    msr     spsr_el2, x0

    // Land on label 8, which is the first EL1 instruction. adr is a
    // PC-relative address, so it is correct with the MMU off.
    adr     x0, 8f
    msr     elr_el2, x0
    isb
    eret

    // -------------------------------------------------------------------------
    // Step 3: Enable the FP/SIMD (NEON) unit. Now at EL1.
    // -------------------------------------------------------------------------
8:
    // The Rust compiler freely uses NEON registers (q0-q31) for things like
    // memcpy and struct copies, even in no_std code. QEMU starts with FP/SIMD
    // access trapped, and the first such instruction would raise an exception
    // (and with no exception vectors set up, the kernel would just hang).
    //
    // CPACR_EL1 is the "Architectural Feature Access Control Register".
    // Bits [21:20] are FPEN:
    //   0b00 or 0b10 = trap FP/SIMD at EL0 and EL1
    //   0b01         = trap at EL0 only
    //   0b11         = never trap  <- what we want
    mrs     x0, cpacr_el1       // x0 = current CPACR_EL1
    orr     x0, x0, #(3 << 20)  // set FPEN bits [21:20] to 0b11
    msr     cpacr_el1, x0       // write it back
    isb                         // Instruction Synchronization Barrier: make
                                // sure the change takes effect before the next
                                // instruction runs (system register writes are
                                // not guaranteed to be seen right away).

    // -------------------------------------------------------------------------
    // Step 4: Set up the stack.
    // -------------------------------------------------------------------------
    // AArch64 stacks grow downward (toward lower addresses). The linker script
    // sets __stack_top to the kernel load address, so the stack grows down
    // into the free RAM *below* the kernel image and never overwrites it:
    //   QEMU:  stack grows down from 0x40080000 (0x80000 = 512 KiB available)
    //   Pi 4:  stack grows down from 0x00080000
    //
    // "ldr x1, =symbol" is an assembler pseudo-instruction. It stores the
    // 64-bit address of the symbol in a nearby literal pool and loads it with
    // a PC-relative load. We can't write sp directly from a literal, so we go
    // through x1. The stack pointer must stay 16-byte aligned, and __stack_top
    // is (it is page-aligned).
    ldr     x1, =__stack_top    // x1 = address of the top of the stack
    mov     sp, x1              // sp = x1

    // -------------------------------------------------------------------------
    // Step 5: Install the exception vector table.
    // -------------------------------------------------------------------------
    // VBAR_EL1 is the Vector Base Address Register for EL1. It must point at
    // a 16-entry table aligned to 2 KiB. exception_vectors is defined in
    // exception.s. From here on, a fault prints a report instead of hanging
    // with no vectors installed. IRQs stay masked until Rust enables them.
    // This register is EL1, so it is written after the drop above.
    ldr     x0, =exception_vectors
    msr     vbar_el1, x0
    isb

    // -------------------------------------------------------------------------
    // Step 6: Clear the .bss section.
    // -------------------------------------------------------------------------
    // .bss holds statics that start out as zero (for example
    // "static mut COUNTER: u64 = 0;"). To keep the image small, the
    // loader does NOT store those zeros in kernel8.img; the section is marked
    // NOLOAD in the linker script. Whatever garbage is in that RAM at power-on
    // is still there, so we must zero it ourselves before Rust runs.
    //
    // The linker script aligns .bss to 4096 bytes, and its size is a multiple
    // of 8, so we can safely clear it 8 bytes (one 64-bit register) at a time.
    ldr     x1, =__bss_start    // x1 = current address to clear
    ldr     x2, =__bss_end      // x2 = one past the last byte of .bss

3:
    cmp     x1, x2              // have we reached the end?
    b.hs    7f                  // if x1 >= x2 (unsigned "higher or same"),
                                // .bss is done, so clear .dma next
    str     xzr, [x1], #8       // store the zero register (xzr = always 0) at
                                // [x1], then add 8 to x1 ("post-index" mode)
    b       3b                  // loop back to label 3

    // .dma holds virtio descriptor rings and packet buffers. It is not part
    // of .bss, so the loop above does not touch it. The MMU later maps it
    // non-cacheable. Zero it the same way, 8 bytes at a time. If the section
    // is empty, the compare falls through immediately.
7:
    ldr     x1, =__dma_start
    ldr     x2, =__dma_end
6:
    cmp     x1, x2
    b.hs    4f
    str     xzr, [x1], #8
    b       6b

4:
    // -------------------------------------------------------------------------
    // Step 7: Jump into Rust.
    // -------------------------------------------------------------------------
    // kernel_main is defined in main.rs as
    //     #[no_mangle] pub extern "C" fn kernel_main(entry_el: u64) -> !
    // x0 is the first argument. x20 still holds the level we entered at
    // (1 for QEMU virt, 2 after a drop from EL2). "bl" (Branch with Link)
    // jumps to kernel_main and saves the return address in x30. kernel_main
    // runs the main poll loop and should never return.
    mov     x0, x20
    bl      kernel_main

    // -------------------------------------------------------------------------
    // Step 8: Safety net.
    // -------------------------------------------------------------------------
    // If kernel_main ever does return, don't run off the end into whatever
    // bytes come next. Sleep forever instead.
5:
    wfe                         // sleep until an event
    b       5b                  // and go straight back to sleep
