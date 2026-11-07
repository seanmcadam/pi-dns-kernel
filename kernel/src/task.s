// EL0 enter and return. global_asm! treats curly braces as format
// placeholders, so this file has none.
//
// TaskContext, written by task.rs, is:
//   0    x0-x30, 31 registers
//   248  sp_el0
//   256  elr_el1
//   264  spsr_el1
//   272  fpsr
//   280  fpcr
//   288  q0-q31
//
// KERNEL_RESUME is x19-x30, the kernel sp, then DAIF. Returning to the
// kernel restores DAIF. Exception entry masks IRQ, and a bare ret would
// leave the poll loop with the timer masked.
// A lower-EL exception arrives on the task kernel stack. The vector saves
// the user registers into CURRENT_CTX and calls el0_dispatch. Zero means
// eret back to EL0. Anything else restores the kernel and returns from
// enter_user.

.section .text
.global el0_sync_entry
.global el0_irq_entry
.global enter_user
.type enter_user, %function

.macro el0_prologue kind
    sub     sp, sp, #272
    stp     x0, x1, [sp, #16]
    stp     x2, x3, [sp, #32]
    stp     x4, x5, [sp, #48]
    stp     x6, x7, [sp, #64]
    stp     x8, x9, [sp, #80]
    stp     x10, x11, [sp, #96]
    stp     x12, x13, [sp, #112]
    stp     x14, x15, [sp, #128]
    stp     x16, x17, [sp, #144]
    stp     x18, x19, [sp, #160]
    stp     x20, x21, [sp, #176]
    stp     x22, x23, [sp, #192]
    stp     x24, x25, [sp, #208]
    stp     x26, x27, [sp, #224]
    stp     x28, x29, [sp, #240]
    str     x30, [sp, #256]
    mov     x0, #\kind
    str     x0, [sp]
    b       el0_saved
.endm

el0_sync_entry:
    el0_prologue 0
el0_irq_entry:
    el0_prologue 1

el0_saved:
    adrp    x9, CURRENT_CTX
    add     x9, x9, :lo12:CURRENT_CTX
    ldr     x9, [x9]
    cbz     x9, el0_no_ctx
    mov     x0, x9
    mov     x1, sp
    add     x1, x1, #16
    mov     x2, #31
1:
    ldr     x3, [x1], #8
    str     x3, [x0], #8
    subs    x2, x2, #1
    b.ne    1b
    mrs     x2, sp_el0
    str     x2, [x9, #248]
    mrs     x2, elr_el1
    mrs     x3, spsr_el1
    stp     x2, x3, [x9, #256]
    mrs     x2, fpsr
    mrs     x3, fpcr
    stp     x2, x3, [x9, #272]
    str     q0, [x9, #288]
    str     q1, [x9, #304]
    str     q2, [x9, #320]
    str     q3, [x9, #336]
    str     q4, [x9, #352]
    str     q5, [x9, #368]
    str     q6, [x9, #384]
    str     q7, [x9, #400]
    str     q8, [x9, #416]
    str     q9, [x9, #432]
    str     q10, [x9, #448]
    str     q11, [x9, #464]
    str     q12, [x9, #480]
    str     q13, [x9, #496]
    str     q14, [x9, #512]
    str     q15, [x9, #528]
    str     q16, [x9, #544]
    str     q17, [x9, #560]
    str     q18, [x9, #576]
    str     q19, [x9, #592]
    str     q20, [x9, #608]
    str     q21, [x9, #624]
    str     q22, [x9, #640]
    str     q23, [x9, #656]
    str     q24, [x9, #672]
    str     q25, [x9, #688]
    str     q26, [x9, #704]
    str     q27, [x9, #720]
    str     q28, [x9, #736]
    str     q29, [x9, #752]
    str     q30, [x9, #768]
    str     q31, [x9, #784]
    ldr     x19, [sp]
    adrp    x1, TASK_KSP
    add     x1, x1, :lo12:TASK_KSP
    ldr     x1, [x1]
    mov     sp, x1
    mov     x0, x19
    bl      el0_dispatch
    cbz     x0, el0_resume
    b       return_to_kernel

el0_resume:
    adrp    x0, CURRENT_CTX
    add     x0, x0, :lo12:CURRENT_CTX
    ldr     x0, [x0]
    b       restore_user

el0_no_ctx:
    bl      el0_no_task

enter_user:
    mov     x10, x0
    mov     x11, x1
    adrp    x9, KERNEL_RESUME
    add     x9, x9, :lo12:KERNEL_RESUME
    stp     x19, x20, [x9, #0]
    stp     x21, x22, [x9, #16]
    stp     x23, x24, [x9, #32]
    stp     x25, x26, [x9, #48]
    stp     x27, x28, [x9, #64]
    stp     x29, x30, [x9, #80]
    mov     x12, sp
    str     x12, [x9, #96]
    mrs     x12, daif
    str     x12, [x9, #104]
    adrp    x9, CURRENT_CTX
    add     x9, x9, :lo12:CURRENT_CTX
    str     x10, [x9]
    adrp    x9, TASK_KSP
    add     x9, x9, :lo12:TASK_KSP
    str     x11, [x9]
    mov     sp, x11
    mov     x0, x10

restore_user:
    ldr     x2, [x0, #248]
    msr     sp_el0, x2
    ldp     x2, x3, [x0, #256]
    msr     elr_el1, x2
    msr     spsr_el1, x3
    ldp     x2, x3, [x0, #272]
    msr     fpsr, x2
    msr     fpcr, x3
    ldr     q0, [x0, #288]
    ldr     q1, [x0, #304]
    ldr     q2, [x0, #320]
    ldr     q3, [x0, #336]
    ldr     q4, [x0, #352]
    ldr     q5, [x0, #368]
    ldr     q6, [x0, #384]
    ldr     q7, [x0, #400]
    ldr     q8, [x0, #416]
    ldr     q9, [x0, #432]
    ldr     q10, [x0, #448]
    ldr     q11, [x0, #464]
    ldr     q12, [x0, #480]
    ldr     q13, [x0, #496]
    ldr     q14, [x0, #512]
    ldr     q15, [x0, #528]
    ldr     q16, [x0, #544]
    ldr     q17, [x0, #560]
    ldr     q18, [x0, #576]
    ldr     q19, [x0, #592]
    ldr     q20, [x0, #608]
    ldr     q21, [x0, #624]
    ldr     q22, [x0, #640]
    ldr     q23, [x0, #656]
    ldr     q24, [x0, #672]
    ldr     q25, [x0, #688]
    ldr     q26, [x0, #704]
    ldr     q27, [x0, #720]
    ldr     q28, [x0, #736]
    ldr     q29, [x0, #752]
    ldr     q30, [x0, #768]
    ldr     q31, [x0, #784]
    ldr     x1, [x0, #8]
    ldr     x2, [x0, #16]
    ldr     x3, [x0, #24]
    ldr     x4, [x0, #32]
    ldr     x5, [x0, #40]
    ldr     x6, [x0, #48]
    ldr     x7, [x0, #56]
    ldr     x8, [x0, #64]
    ldr     x9, [x0, #72]
    ldr     x10, [x0, #80]
    ldr     x11, [x0, #88]
    ldr     x12, [x0, #96]
    ldr     x13, [x0, #104]
    ldr     x14, [x0, #112]
    ldr     x15, [x0, #120]
    ldr     x16, [x0, #128]
    ldr     x17, [x0, #136]
    ldr     x18, [x0, #144]
    ldr     x19, [x0, #152]
    ldr     x20, [x0, #160]
    ldr     x21, [x0, #168]
    ldr     x22, [x0, #176]
    ldr     x23, [x0, #184]
    ldr     x24, [x0, #192]
    ldr     x25, [x0, #200]
    ldr     x26, [x0, #208]
    ldr     x27, [x0, #216]
    ldr     x28, [x0, #224]
    ldr     x29, [x0, #232]
    ldr     x30, [x0, #240]
    ldr     x0, [x0, #0]
    eret

return_to_kernel:
    adrp    x1, EL0_ACTION
    add     x1, x1, :lo12:EL0_ACTION
    str     x0, [x1]
    adrp    x1, KERNEL_RESUME
    add     x1, x1, :lo12:KERNEL_RESUME
    ldp     x19, x20, [x1, #0]
    ldp     x21, x22, [x1, #16]
    ldp     x23, x24, [x1, #32]
    ldp     x25, x26, [x1, #48]
    ldp     x27, x28, [x1, #64]
    ldp     x29, x30, [x1, #80]
    ldr     x2, [x1, #96]
    mov     sp, x2
    ldr     x2, [x1, #104]
    msr     daif, x2
    adrp    x1, EL0_ACTION
    add     x1, x1, :lo12:EL0_ACTION
    ldr     x0, [x1]
    ret
