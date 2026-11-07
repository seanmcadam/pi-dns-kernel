// =============================================================================
// exception.s - EL1 exception vectors
// =============================================================================
//
// Included from exception.rs with global_asm!(include_str!("exception.s")).
// global_asm! treats curly braces as format placeholders, so this file has
// no brace characters at all.
//
// VBAR_EL1 points at exception_vectors. The hardware requires that address
// to be aligned to 2 KiB, and it requires 16 entries, one every 128 bytes.
// Each entry here is a single branch. The register save lives after the
// table so it does not have to fit in those 128 bytes.
//
// The 16 slots, from the ARMv8-A vector table, are:
//   0x000  Synchronous, current EL, SP0
//   0x080  IRQ,          current EL, SP0
//   0x100  FIQ,          current EL, SP0
//   0x180  SError,       current EL, SP0
//   0x200  Synchronous, current EL, SPx   <- we run here, using SP_EL1
//   0x280  IRQ,          current EL, SPx
//   0x300  FIQ,          current EL, SPx
//   0x380  SError,       current EL, SPx
//   0x400  Synchronous, lower EL, AArch64
//   0x480  IRQ,          lower EL, AArch64
//   0x500  FIQ,          lower EL, AArch64
//   0x580  SError,       lower EL, AArch64
//   0x600  Synchronous, lower EL, AArch32
//   0x680  IRQ,          lower EL, AArch32
//   0x700  FIQ,          lower EL, AArch32
//   0x780  SError,       lower EL, AArch32
//
// The branch-per-slot shape is the same one used by
// rust-embedded/rust-raspberrypi-OS-tutorials, which is MIT OR Apache-2.0.
// This file is not a copy of that code.
//
// The current-EL SPx IRQ slot is the one this kernel runs on. It saves the
// general registers and the SIMD registers, calls irq_handler, restores
// them, and erets. Hardware saves ELR_EL1 and SPSR_EL1 and masks IRQ for the
// duration of the handler. eret puts both back, so a handler that returned
// with IRQs unmasked stays unmasked. irq_handler acknowledges the GIC.
// Lower-EL AArch64 synchronous exceptions and IRQs save into the current
// task and can return to EL0 or to the kernel. Every other slot still
// reports the fault and halts. exception_handler does not return, so the
// restore after those calls is not reached.
//
// boot.s drops EL2 to EL1 before it installs this table. A real Pi 4
// firmware handover is at EL2. That path is untested on hardware.
// =============================================================================

.section .text.vectors, "ax"
.global exception_vectors
.type exception_vectors, %function

.balign 2048
exception_vectors:
    b       exc_sp0_sync
    .balign 128
    b       exc_sp0_irq
    .balign 128
    b       exc_sp0_fiq
    .balign 128
    b       exc_sp0_serr
    .balign 128
    b       exc_el1_sync
    .balign 128
    b       exc_el1_irq
    .balign 128
    b       exc_el1_fiq
    .balign 128
    b       exc_el1_serr
    .balign 128
    b       exc_el0_sync
    .balign 128
    b       exc_el0_irq
    .balign 128
    b       exc_el0_fiq
    .balign 128
    b       exc_el0_serr
    .balign 128
    b       exc_a32_sync
    .balign 128
    b       exc_a32_irq
    .balign 128
    b       exc_a32_fiq
    .balign 128
    b       exc_a32_serr
    .balign 128

// Save x0-x30 in a 256-byte frame, then call exception_handler.
// x0 = pointer to that frame. x1 = vector number 0..15.
// The stack was already 16-byte aligned. 256 is a multiple of 16.
.macro enter kind
    sub     sp, sp, #256
    stp     x0,  x1,  [sp, #16 * 0]
    stp     x2,  x3,  [sp, #16 * 1]
    stp     x4,  x5,  [sp, #16 * 2]
    stp     x6,  x7,  [sp, #16 * 3]
    stp     x8,  x9,  [sp, #16 * 4]
    stp     x10, x11, [sp, #16 * 5]
    stp     x12, x13, [sp, #16 * 6]
    stp     x14, x15, [sp, #16 * 7]
    stp     x16, x17, [sp, #16 * 8]
    stp     x18, x19, [sp, #16 * 9]
    stp     x20, x21, [sp, #16 * 10]
    stp     x22, x23, [sp, #16 * 11]
    stp     x24, x25, [sp, #16 * 12]
    stp     x26, x27, [sp, #16 * 13]
    stp     x28, x29, [sp, #16 * 14]
    str     x30,      [sp, #16 * 15]
    msr     daifset, #0xf
    mov     x0, sp
    mov     x1, #\kind
    bl      exception_handler
    ldr     x30,      [sp, #16 * 15]
    ldp     x28, x29, [sp, #16 * 14]
    ldp     x26, x27, [sp, #16 * 13]
    ldp     x24, x25, [sp, #16 * 12]
    ldp     x22, x23, [sp, #16 * 11]
    ldp     x20, x21, [sp, #16 * 10]
    ldp     x18, x19, [sp, #16 * 9]
    ldp     x16, x17, [sp, #16 * 8]
    ldp     x14, x15, [sp, #16 * 7]
    ldp     x12, x13, [sp, #16 * 6]
    ldp     x10, x11, [sp, #16 * 5]
    ldp     x8,  x9,  [sp, #16 * 4]
    ldp     x6,  x7,  [sp, #16 * 3]
    ldp     x4,  x5,  [sp, #16 * 2]
    ldp     x2,  x3,  [sp, #16 * 1]
    ldp     x0,  x1,  [sp, #16 * 0]
    add     sp, sp, #256
    eret
.endm

// 256 bytes of x0-x30, 512 bytes of q0-q31, 16 bytes of FPSR and FPCR.
// 784 is a multiple of 16, and so is the incoming stack pointer.
.macro enter_irq
    sub     sp, sp, #784
    stp     x0,  x1,  [sp, #16 * 0]
    stp     x2,  x3,  [sp, #16 * 1]
    stp     x4,  x5,  [sp, #16 * 2]
    stp     x6,  x7,  [sp, #16 * 3]
    stp     x8,  x9,  [sp, #16 * 4]
    stp     x10, x11, [sp, #16 * 5]
    stp     x12, x13, [sp, #16 * 6]
    stp     x14, x15, [sp, #16 * 7]
    stp     x16, x17, [sp, #16 * 8]
    stp     x18, x19, [sp, #16 * 9]
    stp     x20, x21, [sp, #16 * 10]
    stp     x22, x23, [sp, #16 * 11]
    stp     x24, x25, [sp, #16 * 12]
    stp     x26, x27, [sp, #16 * 13]
    stp     x28, x29, [sp, #16 * 14]
    str     x30,      [sp, #16 * 15]
    stp     q0,  q1,  [sp, #256 + 32 * 0]
    stp     q2,  q3,  [sp, #256 + 32 * 1]
    stp     q4,  q5,  [sp, #256 + 32 * 2]
    stp     q6,  q7,  [sp, #256 + 32 * 3]
    stp     q8,  q9,  [sp, #256 + 32 * 4]
    stp     q10, q11, [sp, #256 + 32 * 5]
    stp     q12, q13, [sp, #256 + 32 * 6]
    stp     q14, q15, [sp, #256 + 32 * 7]
    stp     q16, q17, [sp, #256 + 32 * 8]
    stp     q18, q19, [sp, #256 + 32 * 9]
    stp     q20, q21, [sp, #256 + 32 * 10]
    stp     q22, q23, [sp, #256 + 32 * 11]
    stp     q24, q25, [sp, #256 + 32 * 12]
    stp     q26, q27, [sp, #256 + 32 * 13]
    stp     q28, q29, [sp, #256 + 32 * 14]
    stp     q30, q31, [sp, #256 + 32 * 15]
    mrs     x0, fpsr
    mrs     x1, fpcr
    add     x2, sp, #768
    stp     x0, x1, [x2]
    bl      irq_handler
    add     x2, sp, #768
    ldp     x0, x1, [x2]
    msr     fpsr, x0
    msr     fpcr, x1
    ldp     q30, q31, [sp, #256 + 32 * 15]
    ldp     q28, q29, [sp, #256 + 32 * 14]
    ldp     q26, q27, [sp, #256 + 32 * 13]
    ldp     q24, q25, [sp, #256 + 32 * 12]
    ldp     q22, q23, [sp, #256 + 32 * 11]
    ldp     q20, q21, [sp, #256 + 32 * 10]
    ldp     q18, q19, [sp, #256 + 32 * 9]
    ldp     q16, q17, [sp, #256 + 32 * 8]
    ldp     q14, q15, [sp, #256 + 32 * 7]
    ldp     q12, q13, [sp, #256 + 32 * 6]
    ldp     q10, q11, [sp, #256 + 32 * 5]
    ldp     q8,  q9,  [sp, #256 + 32 * 4]
    ldp     q6,  q7,  [sp, #256 + 32 * 3]
    ldp     q4,  q5,  [sp, #256 + 32 * 2]
    ldp     q2,  q3,  [sp, #256 + 32 * 1]
    ldp     q0,  q1,  [sp, #256 + 32 * 0]
    ldr     x30,      [sp, #16 * 15]
    ldp     x28, x29, [sp, #16 * 14]
    ldp     x26, x27, [sp, #16 * 13]
    ldp     x24, x25, [sp, #16 * 12]
    ldp     x22, x23, [sp, #16 * 11]
    ldp     x20, x21, [sp, #16 * 10]
    ldp     x18, x19, [sp, #16 * 9]
    ldp     x16, x17, [sp, #16 * 8]
    ldp     x14, x15, [sp, #16 * 7]
    ldp     x12, x13, [sp, #16 * 6]
    ldp     x10, x11, [sp, #16 * 5]
    ldp     x8,  x9,  [sp, #16 * 4]
    ldp     x6,  x7,  [sp, #16 * 3]
    ldp     x4,  x5,  [sp, #16 * 2]
    ldp     x2,  x3,  [sp, #16 * 1]
    ldp     x0,  x1,  [sp, #16 * 0]
    add     sp, sp, #784
    eret
.endm

exc_sp0_sync:
    enter 0
exc_sp0_irq:
    enter 1
exc_sp0_fiq:
    enter 2
exc_sp0_serr:
    enter 3
exc_el1_sync:
    enter 4
exc_el1_irq:
    enter_irq
exc_el1_fiq:
    enter 6
exc_el1_serr:
    enter 7
exc_el0_sync:
    b       el0_sync_entry
exc_el0_irq:
    b       el0_irq_entry
exc_el0_fiq:
    enter 10
exc_el0_serr:
    enter 11
exc_a32_sync:
    enter 12
exc_a32_irq:
    enter 13
exc_a32_fiq:
    enter 14
exc_a32_serr:
    enter 15
