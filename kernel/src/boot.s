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
//   - We run in AArch64 at EL1 (QEMU virt with -kernel starts at EL1).
//     On a real Pi 4 the firmware normally hands over at EL2. This file does
//     not drop from EL2 to EL1; that is still untested on hardware.
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
//   2. Turn on the floating-point / SIMD (NEON) unit so Rust code doesn't trap.
//   3. Point the stack pointer at __stack_top.
//   4. Install the EL1 exception vectors in VBAR_EL1.
//   5. Zero .bss, then the .dma section used by virtio.
//   6. Call kernel_main() in main.rs, which never returns.
//   7. If kernel_main ever returns anyway, park the core forever.
//
// This file still does not drop from EL2 to EL1. That is a later change.
// The Pi 4 firmware normally enters at EL2, and that path is untested.
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
    // Step 2: Enable the FP/SIMD (NEON) unit.
    // -------------------------------------------------------------------------
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
    // Step 3: Set up the stack.
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
    // Step 4: Install the exception vector table.
    // -------------------------------------------------------------------------
    // VBAR_EL1 is the Vector Base Address Register for EL1. It must point at
    // a 16-entry table aligned to 2 KiB. exception_vectors is defined in
    // exception.s. From here on, a fault prints a report instead of hanging
    // with no vectors installed. IRQs stay masked. There is still no EL2 to
    // EL1 drop in this file.
    ldr     x0, =exception_vectors
    msr     vbar_el1, x0
    isb

    // -------------------------------------------------------------------------
    // Step 5: Clear the .bss section.
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
    // Step 6: Jump into Rust.
    // -------------------------------------------------------------------------
    // kernel_main is defined in main.rs as
    //     #[no_mangle] pub extern "C" fn kernel_main() -> !
    // "bl" (Branch with Link) jumps to it and saves the return address in x30
    // (the link register). kernel_main runs the main poll loop
    // (network, then DNS, then HTTP) and should never return.
    bl      kernel_main

    // -------------------------------------------------------------------------
    // Step 7: Safety net.
    // -------------------------------------------------------------------------
    // If kernel_main ever does return, don't run off the end into whatever
    // bytes come next. Sleep forever instead.
5:
    wfe                         // sleep until an event
    b       5b                  // and go straight back to sleep
