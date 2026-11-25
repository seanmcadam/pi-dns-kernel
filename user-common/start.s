// EL0 entry. global_asm! treats curly braces as format placeholders,
// so this file has none. The kernel sets SP_EL0 before eret.

.section .text.start, "ax"
.global _start
.type _start, %function

_start:
    bl      rust_main
    mov     x0, #1
    mov     x8, #3
    svc     #0
    b       .
