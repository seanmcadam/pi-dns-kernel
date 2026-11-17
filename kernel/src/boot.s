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
//                                     physical 0x80000. Newer firmware
//                                     documents 0x200000 as the 64-bit
//                                     default. This image copies itself
//                                     to 0x80000 if it was loaded elsewhere.
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

// Linux arm64 Image header. code0 branches over the fields. BOOT_ARM64_MAGIC
// is the "ARM\x64" magic on the Pi 4 image and zero on virt and Pi 5. A
// loader that keys off that magic then adds KERNEL_PHYS to a 2 MiB-aligned
// RAM base. On virt that offset is 0x80000, so the image would land at
// 0x40080000. Putting the absolute address 0x40080000 in this field makes
// QEMU place the image past the end of RAM ("Not enough space for DTB").
// Virt and Pi 5 leave the magic zero, so they stay raw binaries and this
// field is ignored. image_size is 8 MiB, which covers this image. No byte
// here is an absolute high address: the branch is PC-relative and the
// quads are constants.
_start:
    b       boot_body
    .word   0
    .quad   KERNEL_PHYS
    .quad   0x800000
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .word   BOOT_ARM64_MAGIC
    .word   0

boot_body:
    // Mask IRQ, FIQ, SError, and debug before any vector base is installed.
    // A pending firmware interrupt must not enter the early stub. Synchronous
    // aborts are not masked by DAIF, so a fault still reaches that stub.
    msr     daifset, #0xf
    // -------------------------------------------------------------------------
    // Step 1: Find out which core we are, and park all but core 0.
    // -------------------------------------------------------------------------
    // MPIDR_EL1 is the "Multiprocessor Affinity Register". Its lowest byte
    // (Aff0) holds the core number inside the cluster. The Cortex-A72 in the
    // Pi 4 has 4 cores, numbered 0-3, so the bottom 2 bits are enough.
    mrs     x1, mpidr_el1       // x1 = MPIDR_EL1 (read a system register)
    and     x1, x1, #3          // keep only bits [1:0], so x1 = core number 0..3
    cbz     x1, boot_core0      // core 0 continues. Others park.

    // Any core that is not core 0 falls through to here and sleeps forever.
1:
    wfe                         // Wait For Event: low-power sleep until an
                                // event or interrupt wakes the core.
    b       1b                  // woke up anyway? go back to sleep.
                                // "1b" means "the previous label 1, going back".
    // QEMU keeps these cores off until PSCI CPU_ON. A Pi 4
    // firmware stub parks them on the spin table instead.
    // secondary_entry is that later entry, for both.

boot_core0:
    // BOOT_PI4_FLAG is an absolute 0 or 1 from the linker script, patched
    // into this movz. It is not a load, so a stale cache line cannot hide
    // it, and it does not care whether the PC is in the first megabyte.
    // Virt and Pi 5 are 0 and must not touch GPIO. x22 stays as set here.
    movz    x22, #:abs_g0_nc:BOOT_PI4_FLAG
    cbz     x22, 2f
    adrp    x1, _start
    add     x1, x1, :lo12:_start
    movz    x2, #0
    movk    x2, #0x8, lsl #16
    cmp     x1, x2
    b.eq    2f
    // Loaded somewhere other than 0x80000. A 64-bit firmware default is
    // 0x200000. The image is larger than the gap between those two, so the
    // copy overlaps the code that is running. Run the copy from 0x40000,
    // which is above the firmware stub and below both images.
    adrp    x3, __data_end
    add     x3, x3, :lo12:__data_end
    add     x3, x3, #7
    bic     x3, x3, #7
    sub     x4, x3, x1
    adr     x10, reloc_stub
    adr     x11, reloc_stub_end
    movz    x12, #0
    movk    x12, #0x4, lsl #16
    mov     x13, x12
stub_copy:
    cmp     x10, x11
    b.hs    stub_go
    ldr     x14, [x10], #8
    str     x14, [x13], #8
    b       stub_copy
stub_go:
    dsb     sy
    ic      iallu
    dsb     sy
    isb
    br      x12

    .balign 8
reloc_stub:
    mov     x5, x2
reloc_loop:
    cmp     x1, x3
    b.hs    reloc_clean
    ldr     x6, [x1], #8
    str     x6, [x5], #8
    b       reloc_loop
reloc_clean:
    mov     x1, x2
    add     x5, x2, x4
reloc_line:
    cmp     x1, x5
    b.hs    reloc_jump
    dc      cvau, x1
    add     x1, x1, #64
    b       reloc_line
reloc_jump:
    dsb     sy
    ic      iallu
    dsb     sy
    isb
    br      x2
    .balign 8
reloc_stub_end:

2:
    // Stack before any call. QEMU raspi4b enters at EL2h with SP = 0.
    // QEMU virt enters at EL1h, also with SP = 0. The blink path does not
    // use the stack. The code after this point does. SPSel 1 makes mov
    // write the current level's pointer: SP_EL2 at EL2, SP_EL1 at EL1.
    // MSR SP_EL1 is undefined at EL1, so that write happens only when the
    // core is not already there. __stack_top is the physical load address
    // with the MMU off, and it is 16-byte aligned.
    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    msr     spsel, #1
    mov     sp, x1
    mrs     x0, currentel
    lsr     x0, x0, #2
    cmp     x0, #1
    b.eq    stack_ready
    msr     sp_el1, x1
stack_ready:
    mov     x18, xzr
    // x22 is 1 on the Pi 4 image and 0 on virt and Pi 5. x21 is the LED
    // stage. Both survive the EL drop. mark and the helpers below do not
    // write them. x18 is 1 after the pin is an output.
    // MMU off before VBAR points at a physical table. A vector fetch through
    // a firmware map that is not an identity map would fault again at once.
    bl      caches_off
    bl      install_early_vbar
    // Stage 1: MMU is off and the physical stub is installed. Next step
    // reads the L1 data-cache geometry. No set/way op has run yet.
    mov     x21, #1
    bl      mark
    // Pi 4: clean and invalidate L1 only. The previous image walked L2
    // with DC ISW here and the board stopped on that instruction: one
    // blink, then dark, no exception. Virt and Pi 5 keep the full walk.
    cbz     x22, not_pi4_cache
    bl      pi4_cache_maint
    b       cache_done
not_pi4_cache:
    bl      inval_caches
cache_done:
    // Stage 3: maintenance returned. Next step is the EL2 to EL1 drop.
    // Stage 2, if it played, was the Pi 4 L1 walk itself.
    mov     x21, #3
    bl      mark
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

    // Firmware may have left EL1 translations. Drop them before eret.
    // tlbi alle1 is an EL2 instruction. EL1 uses vmalle1 later.
    tlbi    alle1
    dsb     sy
    isb
    // Land on label 8, which is the first EL1 instruction. adr is a
    // PC-relative address, so it is correct with the MMU off.
    adr     x0, 8f
    msr     elr_el2, x0
    isb
    eret

// Wrong exception level. The physical stub repeats stage 3. Other boards
// park without touching GPIO.
bad_el:
    b       fault_blink

    // -------------------------------------------------------------------------
    // Step 3: Enable the FP/SIMD (NEON) unit. Now at EL1.
    // -------------------------------------------------------------------------
8:
    // SCTLR_EL1 reset value, MMU and caches off, RES1 bits set. The EL2
    // path wrote this before eret. The EL1 path (QEMU virt) must write it
    // too: caches_off only clears M, C, and I, and a later enable must not
    // OR those bits onto a register that is missing the RES1 set.
    movz    x0, #0x0800
    movk    x0, #0x30d0, lsl #16
    msr     sctlr_el1, x0
    isb
    // Stage 4: this core is at EL1. Next step enables FP and the stack.
    mov     x21, #4
    bl      mark
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
    // Core 0 already set this at entry. Set it again so the value is the
    // physical top even if a later step moved it. After the jump to the
    // upper half, the same offset is added to SP.
    adrp    x1, __stack_top
    add     x1, x1, :lo12:__stack_top
    mov     sp, x1
    // Stage 5: FP is on and SP is set. Next step zeros BSS and DMA.
    mov     x21, #5
    bl      mark

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
    // Stage 6: BSS and DMA are zero. Next step builds the coarse tables.
    mov     x21, #6
    bl      mark
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
    // Stage 7: tables are in memory. Next step turns the MMU on.
    mov     x21, #7
    bl      mark

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
    // Stale EL1 translations fault the first fetch after M is set. QEMU
    // has none. Real firmware can. vmalle1 is the EL1 invalidate.
    tlbi    vmalle1
    dsb     sy
    isb

    // Full SCTLR_EL1: reset RES1 bits, plus M, C, and I. Not WXN. The
    // block map is writable and executable. Writing the constant avoids
    // OR-ing M onto whatever mrs returned.
    movz    x0, #0x1805
    movk    x0, #0x30d0, lsl #16
    msr     sctlr_el1, x0
    isb
    // Stage 8: the MMU is on. This instruction was fetched through the
    // identity map. Next step jumps to the high alias. The physical stub
    // is still the vector base, and TTBR0 still maps it.
    mov     x21, #8
    bl      mark

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
    // Stage 9: the high alias fetched. Next step installs the high vector
    // base and drops the identity map. Blink before that drop so a fault
    // here still runs the physical stub.
    mov     x21, #9
    bl      mark
    // High VBAR before TTBR0 is cleared. A fault in the gap used to fetch
    // the physical stub through a map that no longer existed.
    adrp    x0, exception_vectors
    add     x0, x0, :lo12:exception_vectors
    msr     vbar_el1, x0
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
    // Stage 10: identity map is gone. Next step is kernel_main. GPIO uses
    // the high alias from here on, because SCTLR.M is set.
    mov     x21, #10
    bl      mark

    // -------------------------------------------------------------------------
    // Step 8: Jump into Rust.
    // -------------------------------------------------------------------------
    // kernel_main is
    //     #[no_mangle] pub extern "C" fn kernel_main(entry_el: u64) -> !
    // x0 is the first argument. x20 still holds the level we entered at.
    mov     x0, x20
    bl      kernel_main

    // kernel_main does not return. If it does, repeat stage 10 with the
    // fault prefix. Virt has nothing to blink and fault_blink waits.
    mov     x21, #10
    b       fault_blink

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

// Pi 4 primary only. Armstub enters with the MMU and caches off, so the
// shared L2 is left alone: a DC ISW of that level did not return on the
// board. This is the ARM ARM set/way loop (the same one Linux uses in
// __flush_dcache_all) restricted to L1 data, and it cleans as well as
// invalidates. CCSIDR values outside the Cortex-A72 L1D shape (2 ways,
// 256 sets, 64-byte lines) are not walked. x19 holds the return address
// across mark. x25-x28 hold the geometry across that call. x20-x22 stay.
pi4_cache_maint:
    mov     x19, x30
    msr     csselr_el1, xzr
    isb
    mrs     x1, ccsidr_el1
    and     x25, x1, #7
    add     x25, x25, #4
    mov     x26, #0x3ff
    and     x26, x26, x1, lsr #3
    clz     w27, w26
    mov     x28, #0x7fff
    and     x28, x28, x1, lsr #13
    // Stage 2: geometry has been read. Next step is the L1 DC CISW loop,
    // then the instruction cache. The blink uses the same delay as stage 1.
    mov     x21, #2
    bl      mark
    dsb     sy
    cmp     x26, #3
    b.hi    pi4_maint_ic
    cmp     x28, #511
    b.hi    pi4_maint_ic
    cmp     x25, #6
    b.ne    pi4_maint_ic
    mov     x10, xzr
    mov     x12, #1
    lsl     x12, x12, #12
pi4_set:
    mov     x9, x26
pi4_way:
    cbz     x12, pi4_maint_ic
    sub     x12, x12, #1
    lsl     x6, x9, x27
    orr     x11, x10, x6
    lsl     x6, x28, x25
    orr     x11, x11, x6
    dc      cisw, x11
    subs    x9, x9, #1
    b.ge    pi4_way
    subs    x28, x28, #1
    b.ge    pi4_set
pi4_maint_ic:
    dsb     sy
    ic      iallu
    dsb     sy
    isb
    msr     csselr_el1, xzr
    isb
    mov     x30, x19
    ret

// Invalidate every data-cache level up to the point of coherency, by set
// and way, then the instruction cache. Virt and Pi 5 still use this.
// Pi 4 primary uses pi4_cache_maint instead. DC ISW comes first. An early
// ic iallu can refill from a stale unified L2 line. Lines are discarded,
// not written back. A stuck or huge geometry stops after 1<<20 operations
// so boot cannot spin here. Registers x0-x15 are scratch. x19-x28 are left
// alone, including the stage in x21 and the Pi 4 flag in x22.
// inval_l1 stops after the first level. Secondary cores use it so they
// do not throw away the primary's shared L2 once that core has caches on.
inval_caches:
    mrs     x0, clidr_el1
    and     x3, x0, #0x7000000
    lsr     x3, x3, #23
    b       inval_have_loc
inval_l1:
    mrs     x0, clidr_el1
    mov     x3, #2
inval_have_loc:
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
    ic      iallu
    dsb     sy
    isb
    ret

// GPIO 42 is the green ACT LED on current Pi 4 boards, active high.
// GPFSEL4 at 0xFE200010, bits 8:6. GPSET1 and GPCLR1 use bit 10.
// These are leaves. They do not call, they do not use the stack, and they
// write only x0-x3. x16 is the linker's veneer register, so a saved return
// address must not live there across a call. With the MMU off the address
// is physical. With the MMU on at the level that is running, the high
// alias is used. EL2 reads SCTLR_EL2 so a trap into the stub does not
// follow SCTLR_EL1.M while EL2's MMU is off.
gpio_store:
    mrs     x0, currentel
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    gpio_store_el1
    mrs     x0, sctlr_el2
    b       gpio_store_test
gpio_store_el1:
    mrs     x0, sctlr_el1
gpio_store_test:
    tbz     x0, #0, gpio_store_write
    movz    x0, #0xFFFF, lsl #48
    orr     x1, x1, x0
gpio_store_write:
    str     w2, [x1]
    ret

gpio42_on:
    movz    x1, #0x0020
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    b       gpio_store

gpio42_off:
    movz    x1, #0x002C
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    b       gpio_store

// Program the pin as an output. Leaf: the function-select update is inline
// so a call from mark cannot lose its return address. The set happens
// before the read. A hang on that load leaves the LED on.
gpio42_output:
    movz    x1, #0x0020
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    mrs     x0, currentel
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    gpio_out_el1a
    mrs     x0, sctlr_el2
    b       gpio_out_ta
gpio_out_el1a:
    mrs     x0, sctlr_el1
gpio_out_ta:
    tbz     x0, #0, gpio_out_wa
    movz    x0, #0xFFFF, lsl #48
    orr     x1, x1, x0
gpio_out_wa:
    str     w2, [x1]
    movz    x1, #0x0010
    movk    x1, #0xFE20, lsl #16
    mrs     x0, currentel
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    gpio_out_el1b
    mrs     x0, sctlr_el2
    b       gpio_out_tb
gpio_out_el1b:
    mrs     x0, sctlr_el1
gpio_out_tb:
    tbz     x0, #0, gpio_out_wb
    movz    x0, #0xFFFF, lsl #48
    orr     x1, x1, x0
gpio_out_wb:
    ldr     w2, [x1]
    mov     w3, #7
    lsl     w3, w3, #6
    bic     w2, w2, w3
    mov     w3, #1
    lsl     w3, w3, #6
    orr     w2, w2, w3
    str     w2, [x1]
    movz    x1, #0x0020
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    mrs     x0, currentel
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    gpio_out_el1c
    mrs     x0, sctlr_el2
    b       gpio_out_tc
gpio_out_el1c:
    mrs     x0, sctlr_el1
gpio_out_tc:
    tbz     x0, #0, gpio_out_wc
    movz    x0, #0xFFFF, lsl #48
    orr     x1, x1, x0
gpio_out_wc:
    str     w2, [x1]
    ret

// Blink x21 flashes, then a gap. x22 == 0 returns at once. x29 holds the
// return address. A shift of 2 is 250 ms and a shift of 1 is 500 ms, so
// three of those are the 1.5 s gap. The function-select register is written
// once (x18). Leaves called from here do not write x18-x29. The pin is left
// on, so a hang in the stage that follows stays lit instead of going dark.
mark:
    cbz     x22, mark_ret
    mov     x29, x30
    cbnz    x18, mark_ready
    bl      gpio42_output
    mov     x18, #1
mark_ready:
    mov     x23, x21
mark_pulse:
    cbz     x23, mark_gap
    bl      gpio42_on
    mov     x0, #2
    bl      delay_shift
    bl      gpio42_off
    mov     x0, #2
    bl      delay_shift
    sub     x23, x23, #1
    b       mark_pulse
mark_gap:
    mov     x0, #1
    bl      delay_shift
    mov     x0, #1
    bl      delay_shift
    mov     x0, #1
    bl      delay_shift
    bl      gpio42_on
    mov     x30, x29
mark_ret:
    ret

// Physical vector target. No stack. Three rapid flashes, then x21 at the
// slow rate, forever. x21 == 0 repeats only the rapid three. The pin is
// never left off at the end of the loop. Any other board waits.
fault_blink:
    cbz     x22, fault_park
    cbnz    x18, fault_rapid
    bl      gpio42_output
    mov     x18, #1
fault_rapid:
    mov     x23, #3
fault_rapid_one:
    bl      gpio42_on
    mov     x0, #4
    bl      delay_shift
    bl      gpio42_off
    mov     x0, #4
    bl      delay_shift
    subs    x23, x23, #1
    b.ne    fault_rapid_one
    cbz     x21, fault_rapid
    mov     x23, x21
fault_count:
    cbz     x23, fault_count_gap
    bl      gpio42_on
    mov     x0, #2
    bl      delay_shift
    bl      gpio42_off
    mov     x0, #2
    bl      delay_shift
    sub     x23, x23, #1
    b       fault_count
fault_count_gap:
    mov     x0, #1
    bl      delay_shift
    mov     x0, #1
    bl      delay_shift
    mov     x0, #1
    bl      delay_shift
    b       fault_rapid
fault_park:
    wfe
    b       fault_park

// VBAR_EL1 and, at EL2, VBAR_EL2. The table is physical while the MMU is
// off, and the identity map covers it until TTBR0 is dropped.
install_early_vbar:
    adrp    x0, early_vectors
    add     x0, x0, :lo12:early_vectors
    msr     vbar_el1, x0
    mrs     x1, currentel
    lsr     x1, x1, #2
    cmp     x1, #2
    b.ne    install_early_done
    msr     vbar_el2, x0
install_early_done:
    isb
    ret

    .balign 2048
early_vectors:
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink
    .balign 128
    b       fault_blink

// Wait CNTFRQ >> x0 ticks. Leaf. Clobbers x12-x15 only. Elapsed time is a
// subtract, not a deadline compare, so a wrapped absolute count cannot
// stick. CNTFRQ of 0 uses 54 MHz, which is what the Pi armstub programs.
// A counter that does not move stops after 1<<26 polls. The old cap was
// 1<<30, long enough to look like a dark hang after the first flash.
delay_shift:
    mrs     x12, cntfrq_el0
    cbnz    x12, delay_have_freq
    movz    x12, #0xf980
    movk    x12, #0x0337, lsl #16
delay_have_freq:
    lsr     x12, x12, x0
    cbnz    x12, delay_have_ticks
    mov     x12, #1
delay_have_ticks:
    mrs     x13, cntpct_el0
    mov     x15, #1
    lsl     x15, x15, #26
delay_poll:
    mrs     x14, cntpct_el0
    sub     x14, x14, x13
    cmp     x14, x12
    b.hs    delay_shift_ret
    subs    x15, x15, #1
    b.ne    delay_poll
delay_shift_ret:
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
    // L1 only. A full DC ISW here would discard the primary's shared L2.
    bl      inval_l1
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
    tlbi    vmalle1
    dsb     sy
    isb
    movz    x0, #0x1805
    movk    x0, #0x30d0, lsl #16
    msr     sctlr_el1, x0
    isb
    adr     x0, 21f
    movz    x1, #0xFFFF, lsl #48
    add     x0, x0, x1
    add     sp, sp, x1
    br      x0
21:
    // High vectors before the identity map goes away.
    adrp    x0, exception_vectors
    add     x0, x0, :lo12:exception_vectors
    msr     vbar_el1, x0
    isb
    msr     ttbr0_el1, xzr
    dsb     nsh
    tlbi    vmalle1
    dsb     nsh
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
