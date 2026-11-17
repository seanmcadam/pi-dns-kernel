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
// _start is the first instruction of the file the loader copies:
//   - QEMU virt (board/virt/link.ld): linked at 0xFFFF000040080000
//                                     loaded at physical 0x40080000
//   - Raspberry Pi 4 (board/pi4/link.ld): linked at 0xFFFF000000080000
//                                     firmware loads the raw image at
//                                     physical 0x80000. Untested on hardware.
//   - Raspberry Pi 5 (board/pi5/link.ld): linked at 0xFFFF000000200000
//                                     firmware loads kernel_2712.img at
//                                     physical 0x200000. Untested on hardware.
//
// The upper 16 bits are the only difference between the link address and the
// physical load address. While the MMU is off the PC is the physical address.
// ADRP and ADD are PC-relative, so they yield physical addresses until the
// jump below. An absolute literal would be the high link address and must
// not be used before that jump.
//
// State of the CPU on entry
// -------------------------
//   - AArch64. QEMU virt with -kernel starts at EL1. QEMU virt with
//     virtualization=on starts at EL2. A real Pi 4 with arm_64bit=1 normally
//     hands over at EL2. The Pi 5 stub normally does the same. The EL2 path
//     below is untested on Pi hardware. An EL3 entry parks.
//   - The MMU and caches should be off. A firmware can still leave dirty
//     data-cache lines, and turning the cache on over those lines is
//     UNPREDICTABLE. Core 0 discards them before it stores anything.
//   - There is no stack yet, so we cannot call any Rust code until we set
//     one up.
//   - Every core may jump here. The Pi 4 armstub enters at EL2 on core 0
//     and parks cores 1-3 on the spin table at 0xd8, 0xe0, 0xe8, and 0xf0.
//     Those cores do not execute this image. QEMU -kernel does the same
//     with its own stub. A core that does land here, other than core 0,
//     waits. Rust releases the parked cores later by writing that table.
//
// What this file does, in order
// -----------------------------
//   1. Work out which core is running and park every core except core 0.
//   2. If the core entered at EL2, drop to EL1h. If it is already at EL1,
//      skip that drop. x20 remembers the entry level for kernel_main.
//   3. Turn on the floating-point / SIMD (NEON) unit so Rust code doesn't trap.
//   4. Point the stack pointer at the physical alias of __stack_top.
//   5. Zero .bss, then the .dma section used by virtio.
//   6. Build a 1 GiB block map. TTBR0 is an identity map of that RAM and of
//      the device gigabyte, so the physical PC still translates when the MMU
//      turns on. TTBR1 maps the same blocks at the high link address.
//   7. Turn the MMU and caches on, add the high offset to the PC and SP, and
//      branch to the upper half. Then drop the identity map so TTBR0 is empty
//      and point VBAR_EL1 at the high vector table.
//   8. Call kernel_main. Rust installs the strict TTBR1 map and, later, the
//      user window in TTBR0. WXN is not set here: the 1 GiB block is both
//      writable and executable, and WXN would make it unexecutable.
//   9. If kernel_main ever returns anyway, park the core forever.
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
// QEMU keeps these cores off until PSCI CPU_ON. A Pi 4
    // firmware stub parks them on the spin table instead.
    // secondary_entry is that later entry, for both.

2:
    // Drop a stale EL2 or EL1 translation and throw away every data-cache
    // line before the first store. DC ISW discards the line. A clean would
    // write a stale line back over the image the GPU just loaded.
    bl      caches_off
    bl      inval_caches
    // One long ACT flash means this image is running on core 0. The count
    // codes below are Rust. Virt and Pi 5 leave the pin alone.
    bl      act_long
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
    b.ne    bad_el              // not EL1 or EL2: blink, then park

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

    // CPTR_EL2.TFP traps EL1 floating-point to EL2. 0x33ff clears the trap
    // bits and sets the RES1 bits in CPTR_EL2.
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

// Wrong exception level. A Pi 4 flashes the ACT LED fast so a serial-less
// board can show that the image started and then stopped here. Other
// boards fall through to the same wait the extra cores use.
bad_el:
    bl      act_fast_forever
    b       1b

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
    // Step 4: Set up the stack at its physical address.
    // -------------------------------------------------------------------------
    // AArch64 stacks grow downward. The linker sets __stack_top to the high
    // link address. ADRP while the PC is still physical yields the physical
    // alias, which is the load address:
    //   QEMU:  stack grows down from 0x40080000
    //   Pi 4:  stack grows down from 0x00080000 (untested on hardware)
    // The stack pointer must stay 16-byte aligned. __stack_top is page-aligned.
    // After the jump to the upper half, the same offset is added to SP.
    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    mov     sp, x1

    // -------------------------------------------------------------------------
    // Step 5: Clear the .bss section, then .dma.
    // -------------------------------------------------------------------------
    // .bss holds statics that start out as zero. The loader does not store
    // those zeros in kernel8.img. The early page tables live in .bss, so they
    // are zero before step 6 fills them. Addresses here are still physical.
    adrp    x1, __bss_start
    add     x1, x1, :lo12:__bss_start
    adrp    x2, __bss_end
    add     x2, x2, :lo12:__bss_end

3:
    cmp     x1, x2
    b.hs    7f
    str     xzr, [x1], #8
    b       3b

    // .dma holds virtio descriptor rings and packet buffers. It is not part
    // of .bss. The strict map later marks it non-cacheable. Zero it the same
    // way. If the section is empty, the compare falls through.
7:
    adrp    x1, __dma_start
    add     x1, x1, :lo12:__dma_start
    adrp    x2, __dma_end
    add     x2, x2, :lo12:__dma_end
6:
    cmp     x1, x2
    b.hs    4f
    str     xzr, [x1], #8
    b       6b

4:
    // -------------------------------------------------------------------------
    // Step 6: Coarse page tables for the jump into the upper half.
    // -------------------------------------------------------------------------
    // x3 = physical base of the 1 GiB that contains the kernel.
    // QEMU _start is 0x40080000, so this is 0x40000000 and the level-1 index
    // is 1. The Pi 4 _start is 0x80000, so this is 0 and the index is 0.
    // x20 still holds the entry exception level. Do not write x20.
    adrp    x3, _start
    lsr     x3, x3, #30
    lsl     x3, x3, #30
    lsr     x10, x3, #30

    // RAM block: valid, inner-shareable, access flag, normal memory, UXN.
    // No privileged-execute-never bit, so EL1 can run from this block. No
    // AP bit that would let EL0 in. The output address is the physical base.
    mov     x11, x3
    orr     x11, x11, #1
    orr     x11, x11, #0x300
    orr     x11, x11, #0x400
    movz    x12, #0x0040, lsl #48
    orr     x11, x11, x12

    // The other gigabyte is the devices kernel_main prints to before the
    // strict map exists. QEMU UART, GIC, and virtio-mmio sit in the first
    // gigabyte. Pi 4 peripherals sit at 0xFC000000 and up, inside the
    // gigabyte at 0xC0000000. Device-nGnRE, outer shareable, PXN and UXN.
    cbz     x10, 10f
    mov     x13, xzr
    mov     x14, xzr
    b       11f
10:
    movz    x13, #0xC000, lsl #16
    mov     x14, #3
11:
    orr     x13, x13, #1
    orr     x13, x13, #4
    orr     x13, x13, #0x200
    orr     x13, x13, #0x400
    movz    x12, #0x0060, lsl #48
    orr     x13, x13, x12

    // Same descriptors in TTBR0 (identity, so the physical PC translates)
    // and TTBR1 (high alias). x2-x5 are the arguments to build_early.
    adrp    x0, early_l0_id
    add     x0, x0, :lo12:early_l0_id
    adrp    x1, early_l1_id
    add     x1, x1, :lo12:early_l1_id
    mov     x2, x11
    mov     x3, x10
    mov     x4, x13
    mov     x5, x14
    bl      build_early

    adrp    x0, early_l0_hi
    add     x0, x0, :lo12:early_l0_hi
    adrp    x1, early_l1_hi
    add     x1, x1, :lo12:early_l1_hi
    mov     x2, x11
    mov     x3, x10
    mov     x4, x13
    mov     x5, x14
    bl      build_early

    // BCM2712 peripherals sit above 4 GiB. Two extra 1 GiB device blocks
    // cover the SDHCI at 0x1000fff000 and the debug UART plus GIC-400 at
    // 0x107d001000 and 0x107fff9000. Virt and Pi 4 have no RAM there, and
    // nothing in those images loads from these indices. Same attribute bits
    // as the other device block: valid, AttrIdx 1, AF, outer shareable,
    // PXN and UXN.
    movz    x13, #0x10, lsl #32
    orr     x13, x13, #1
    orr     x13, x13, #4
    orr     x13, x13, #0x200
    orr     x13, x13, #0x400
    movz    x12, #0x0060, lsl #48
    orr     x13, x13, x12
    mov     x14, #0x40
    adrp    x1, early_l1_id
    add     x1, x1, :lo12:early_l1_id
    str     x13, [x1, x14, lsl #3]
    adrp    x1, early_l1_hi
    add     x1, x1, :lo12:early_l1_hi
    str     x13, [x1, x14, lsl #3]
    movz    x13, #0x10, lsl #32
    movk    x13, #0x4000, lsl #16
    orr     x13, x13, #1
    orr     x13, x13, #4
    orr     x13, x13, #0x200
    orr     x13, x13, #0x400
    movz    x12, #0x0060, lsl #48
    orr     x13, x13, x12
    mov     x14, #0x41
    adrp    x1, early_l1_id
    add     x1, x1, :lo12:early_l1_id
    str     x13, [x1, x14, lsl #3]
    adrp    x1, early_l1_hi
    add     x1, x1, :lo12:early_l1_hi
    str     x13, [x1, x14, lsl #3]

    // Push the tables to the point of coherency before the walker reads them.
    // Four pages, 64-byte lines. clean_page clobbers x0 and x2.
    adrp    x0, early_l0_id
    add     x0, x0, :lo12:early_l0_id
    bl      clean_page
    adrp    x0, early_l1_id
    add     x0, x0, :lo12:early_l1_id
    bl      clean_page
    adrp    x0, early_l0_hi
    add     x0, x0, :lo12:early_l0_hi
    bl      clean_page
    adrp    x0, early_l1_hi
    add     x0, x0, :lo12:early_l1_hi
    bl      clean_page
    dsb     sy

    // -------------------------------------------------------------------------
    // Step 7: Turn the MMU on and jump to the high alias.
    // -------------------------------------------------------------------------
    // MAIR: Attr0 Normal write-back, Attr1 Device-nGnRE, Attr2 Normal
    // non-cacheable. Same value Rust uses for the strict map.
    movz    x0, #0x04ff
    movk    x0, #0x0044, lsl #16
    msr     mair_el1, x0

    // TCR: 48-bit VA on both halves, 4 KiB granules, inner write-back,
    // inner shareable. The low 32 bits are 0xB5103510. TG1's 4 KiB
    // encoding is 0b10. IPS is bits 34:32. 0b010 is a 40-bit physical
    // address, which is bit 33. Virt and Pi 4 addresses still fit.
    // BCM2712 peripherals sit above 4 GiB and need the wider size.
    movz    x0, #0x3510
    movk    x0, #0xB510, lsl #16
    movk    x0, #2, lsl #32
    msr     tcr_el1, x0

    adrp    x0, early_l0_id
    add     x0, x0, :lo12:early_l0_id
    msr     ttbr0_el1, x0
    adrp    x0, early_l0_hi
    add     x0, x0, :lo12:early_l0_hi
    msr     ttbr1_el1, x0
    dsb     sy
    isb
    ic      iallu
    dsb     sy
    isb

    // SCTLR M, C, and I. Not WXN. The block map is writable and executable.
    mrs     x0, sctlr_el1
    orr     x0, x0, #1
    orr     x0, x0, #4
    orr     x0, x0, #0x1000
    msr     sctlr_el1, x0
    isb

    // Label 9 is the first instruction fetched through TTBR1.
    adr     x0, 9f
    movz    x1, #0xFFFF, lsl #48
    add     x0, x0, x1
    add     sp, sp, x1
    br      x0

9:
    ic      iallu
    dsb     sy
    isb
    // Drop the identity map. TTBR0's only level-0 entry covered RAM and
    // devices. After this, a low address faults until Rust installs the
    // user window. The store is through the high alias of the same page.
    adrp    x0, early_l0_id
    add     x0, x0, :lo12:early_l0_id
    str     xzr, [x0]
    dc      civac, x0
    dsb     sy
    tlbi    vmalle1
    dsb     sy
    isb

    // VBAR must be the high address. A physical vector base would fault
    // now that the identity map is gone. IRQs stay masked until Rust.
    adrp    x0, exception_vectors
    add     x0, x0, :lo12:exception_vectors
    msr     vbar_el1, x0
    isb

    // -------------------------------------------------------------------------
    // Step 8: Jump into Rust.
    // -------------------------------------------------------------------------
    // kernel_main is
    //     #[no_mangle] pub extern "C" fn kernel_main(entry_el: u64) -> !
    // x0 is the first argument. x20 still holds the level we entered at.
    mov     x0, x20
    bl      kernel_main

    // -------------------------------------------------------------------------
    // Step 9: Safety net.
    // -------------------------------------------------------------------------
5:
    wfe
    b       5b

// x0 = level-0 table, x1 = level-1 table.
// x2 = RAM block descriptor, x3 = its level-1 index.
// x4 = device block descriptor, x5 = its level-1 index.
// The level-0 entry points at the level-1 table. Both block entries are
// stored. The two indices are never the same.
build_early:
    orr     x6, x1, #3
    str     x6, [x0]
    str     x2, [x1, x3, lsl #3]
    str     x4, [x1, x5, lsl #3]
    ret

// Turn the MMU and both caches off at the level we are in, and in EL1.
// Writes of SCTLR_EL2 happen only at EL2. Clearing a bit that is already
// clear is the usual case: firmware and QEMU enter with the MMU off.
caches_off:
    mrs     x9, currentel
    lsr     x9, x9, #2
    cmp     x9, #2
    b.ne    caches_off_el1
    mrs     x0, sctlr_el2
    bic     x0, x0, #1
    bic     x0, x0, #4
    bic     x0, x0, #0x1000
    msr     sctlr_el2, x0
    isb
caches_off_el1:
    mrs     x0, sctlr_el1
    bic     x0, x0, #1
    bic     x0, x0, #4
    bic     x0, x0, #0x1000
    msr     sctlr_el1, x0
    isb
    ret

// Invalidate the instruction cache, then every data-cache level up to the
// point of coherency, by set and way. Lines are discarded, not written
// back. A stuck or huge geometry stops after 1<<20 operations so boot
// cannot spin here. Registers x0-x15 are scratch. x19-x28 are left alone.
inval_caches:
    ic      iallu
    dsb     sy
    isb
    mrs     x0, clidr_el1
    and     x3, x0, #0x7000000
    lsr     x3, x3, #23
    cbz     x3, inval_done
    mov     x10, #0
    mov     x12, #1
    lsl     x12, x12, #20
inval_level:
    add     x2, x10, x10, lsr #1
    lsr     x1, x0, x2
    and     x1, x1, #7
    cmp     x1, #2
    b.lt    inval_next
    msr     csselr_el1, x10
    isb
    mrs     x1, ccsidr_el1
    and     x2, x1, #7
    add     x2, x2, #4
    mov     x4, #0x3ff
    and     x4, x4, x1, lsr #3
    clz     w5, w4
    mov     x7, #0x7fff
    and     x7, x7, x1, lsr #13
inval_set:
    mov     x9, x4
inval_way:
    cbz     x12, inval_done
    sub     x12, x12, #1
    lsl     x6, x9, x5
    orr     x11, x10, x6
    lsl     x6, x7, x2
    orr     x11, x11, x6
    dc      isw, x11
    subs    x9, x9, #1
    b.ge    inval_way
    subs    x7, x7, #1
    b.ge    inval_set
inval_next:
    add     x10, x10, #2
    cmp     x3, x10
    b.gt    inval_level
inval_done:
    dsb     sy
    isb
    ret

// GPIO 42 is the green ACT LED on current Pi 4 boards, active high.
// GPFSEL4 at 0xFE200010, bits 8:6. GPSET1 and GPCLR1 use bit 10.
// Physical addresses: this runs with the MMU off. BOOT_PI4 is 0 on the
// other images, and those images do not touch the pin.
gpio42_output:
    movz    x1, #0x0010
    movk    x1, #0xFE20, lsl #16
    ldr     w2, [x1]
    mov     w3, #7
    lsl     w3, w3, #6
    bic     w2, w2, w3
    mov     w3, #1
    lsl     w3, w3, #6
    orr     w2, w2, w3
    str     w2, [x1]
    ret

gpio42_on:
    movz    x1, #0x0020
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    str     w2, [x1]
    ret

gpio42_off:
    movz    x1, #0x002C
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    str     w2, [x1]
    ret

// Wait CNTFRQ >> x0 ticks. A counter that does not move stops after a
// fixed spin, so a stuck CNTPCT cannot park the core. x0 is the shift.
delay_shift:
    mrs     x12, cntfrq_el0
    lsr     x12, x12, x0
    cbz     x12, delay_spin
    mrs     x13, cntpct_el0
    add     x13, x13, x12
    mov     x15, #1
    lsl     x15, x15, #30
delay_poll:
    mrs     x14, cntpct_el0
    cmp     x14, x13
    b.hs    delay_shift_ret
    subs    x15, x15, #1
    b.ne    delay_poll
delay_shift_ret:
    ret
delay_spin:
    mov     x15, #1
    lsl     x15, x15, #24
delay_spin_loop:
    subs    x15, x15, #1
    b.ne    delay_spin_loop
    ret

// One long on, about half a second, then off. Distinct from the short
// counted blinks Rust uses for the later stages.
act_long:
    mov     x19, x30
    adrp    x0, BOOT_PI4
    add     x0, x0, :lo12:BOOT_PI4
    ldr     x0, [x0]
    cbz     x0, act_long_ret
    bl      gpio42_output
    bl      gpio42_on
    mov     x0, #1
    bl      delay_shift
    bl      gpio42_off
    mov     x0, #2
    bl      delay_shift
act_long_ret:
    mov     x30, x19
    ret

// Fast continuous blink. Returns only when this is not a Pi 4 image.
act_fast_forever:
    adrp    x0, BOOT_PI4
    add     x0, x0, :lo12:BOOT_PI4
    ldr     x0, [x0]
    cbz     x0, act_fast_ret
    bl      gpio42_output
act_fast_loop:
    bl      gpio42_on
    mov     x0, #5
    bl      delay_shift
    bl      gpio42_off
    mov     x0, #5
    bl      delay_shift
    b       act_fast_loop
act_fast_ret:
    ret

// Clean and invalidate the 4 KiB page at x0, one cache line at a time.
clean_page:
    mov     x2, #64
12:
    dc      civac, x0
    add     x0, x0, #64
    subs    x2, x2, #1
    b.ne    12b
    ret

// Secondary cores arrive here with the MMU off. x0 is the PSCI context
// id. QEMU virt passes the cpu id there, so that path is unchanged.
// The Pi 4 spin-table stub, including QEMU raspi4b, zeroes x0-x3 and then
// branches. A zero x0 means take the MPIDR affinity instead. Aff0 is the
// cpu id on the Pi 4. On the BCM2712 every core has Aff0 0 and the cpu
// number lives in Aff1, so a zero Aff0 uses Aff1. Core 0 never enters
// here. The Pi 4 spin table and the Pi 5 PSCI path are untested on hardware.
.section .text
.global secondary_entry
.type secondary_entry, %function
secondary_entry:
    mov     x21, x0
    bl      caches_off
    bl      inval_caches
    cbnz    x21, 18f
    mrs     x21, mpidr_el1
    and     x22, x21, #0xff
    cbnz    x22, 19f
    lsr     x21, x21, #8
    and     x21, x21, #0xff
    b       18f
19:
    mov     x21, x22
18:
    cmp     x21, #3
    b.hi    29f
    mrs     x20, currentel
    lsr     x20, x20, #2
    cmp     x20, #1
    b.eq    20f
    cmp     x20, #2
    b.ne    29f
    movz    x0, #0x8000, lsl #16
    msr     hcr_el2, x0
    mrs     x0, cnthctl_el2
    orr     x0, x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mrs     x0, midr_el1
    msr     vpidr_el2, x0
    mrs     x0, mpidr_el1
    msr     vmpidr_el2, x0
    movz    x0, #0x33ff
    msr     cptr_el2, x0
    movz    x0, #0x0800
    movk    x0, #0x30d0, lsl #16
    msr     sctlr_el1, x0
    movz    x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, 20f
    msr     elr_el2, x0
    isb
    eret
20:
    mrs     x0, cpacr_el1
    orr     x0, x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    adrp    x1, SEC_SP
    add     x1, x1, :lo12:SEC_SP
    ldr     x1, [x1, x21, lsl #3]
    mov     sp, x1
    movz    x0, #0x0800
    movk    x0, #0x30d0, lsl #16
    msr     sctlr_el1, x0
    adrp    x1, SEC_MAIR
    add     x1, x1, :lo12:SEC_MAIR
    ldr     x1, [x1]
    msr     mair_el1, x1
    adrp    x1, SEC_TCR
    add     x1, x1, :lo12:SEC_TCR
    ldr     x1, [x1]
    msr     tcr_el1, x1
    adrp    x1, early_l0_id
    add     x1, x1, :lo12:early_l0_id
    msr     ttbr0_el1, x1
    adrp    x1, SEC_TTBR1
    add     x1, x1, :lo12:SEC_TTBR1
    ldr     x1, [x1]
    msr     ttbr1_el1, x1
    dsb     sy
    isb
    ic      iallu
    dsb     sy
    isb
    mrs     x0, sctlr_el1
    orr     x0, x0, #1
    orr     x0, x0, #4
    orr     x0, x0, #0x1000
    msr     sctlr_el1, x0
    isb
    adr     x0, 21f
    movz    x1, #0xFFFF, lsl #48
    add     x0, x0, x1
    add     sp, sp, x1
    br      x0
21:
    msr     ttbr0_el1, xzr
    dsb     nsh
    tlbi    vmalle1
    dsb     nsh
    isb
    adrp    x0, exception_vectors
    add     x0, x0, :lo12:exception_vectors
    msr     vbar_el1, x0
    isb
    mov     x0, x21
    bl      secondary_main
29:
    wfe
    b       29b

// Early translation tables. Zeroed with the rest of .bss, then filled above.
// They stay in the image's RAM, so the high map covers them after the jump.
.section .bss
.balign 4096
early_l0_id:
.space 4096
early_l1_id:
.space 4096
early_l0_hi:
.space 4096
early_l1_hi:
.space 4096
