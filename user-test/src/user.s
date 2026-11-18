// EL0 test program. Linked at 0x100000000. global_asm! treats curly braces
// as format placeholders, so this file has none.
//
// x0 on entry is the scenario. x1 is an address chosen by the kernel:
//   0  log, yield, spin until the tick count has moved by 3, log, exit 0
//      The kernel preempts every 2 EL0 ticks, so that delta crosses a slice.
//   1  unknown syscall number in x8, then a log so we know it returned
//   2  log(x1, 4) with a pointer the kernel must reject
//   3  load from x1, which is kernel text. This must not return.
//   4  mrs currentel. A privileged read must not return.
//   5  branch to the user stack. That page is not executable.
//   6  four threads add under a futex mutex. The total must be exact.
//   7  recurse until the store falls in the guard page under the stack.
//
// Syscalls: x8 is the number, x0-x5 the arguments, x0 the return.
//   0 log(ptr, len)   1 yield   2 uptime_ticks   3 exit(code)

.section .text.start, "ax"
.global _start
.type _start, %function

_start:
    mov     x19, x0
    mov     x20, x1
    cmp     x19, #0
    b.eq    1f
    cmp     x19, #1
    b.eq    2f
    cmp     x19, #2
    b.eq    3f
    cmp     x19, #3
    b.eq    4f
    cmp     x19, #4
    b.eq    5f
    cmp     x19, #5
    b.eq    6f
    cmp     x19, #6
    b.eq    7f
    cmp     x19, #7
    b.eq    8f
    mov     x0, #9
    mov     x8, #3
    svc     #0
    b       .

1:
    adr     x0, msg_hello
    mov     x1, #(msg_hello_end - msg_hello)
    mov     x8, #0
    svc     #0
    mov     x8, #1
    svc     #0
    mov     x8, #2
    svc     #0
    mov     x22, x0
    mov     x21, #64
10:
    movz    x23, #0x40, lsl #16
11:
    subs    x23, x23, #1
    b.ne    11b
    mov     x8, #2
    svc     #0
    sub     x0, x0, x22
    cmp     x0, #3
    b.ge    12f
    subs    x21, x21, #1
    b.ne    10b
    mov     x0, #2
    mov     x8, #3
    svc     #0
    b       .
12:
    adr     x0, msg_ticks
    mov     x1, #(msg_ticks_end - msg_ticks)
    mov     x8, #0
    svc     #0
    mov     x0, #0
    mov     x8, #3
    svc     #0
    b       .

2:
    mov     x8, #99
    svc     #0
    adr     x0, msg_badsvc
    mov     x1, #(msg_badsvc_end - msg_badsvc)
    mov     x8, #0
    svc     #0
    mov     x0, #0
    mov     x8, #3
    svc     #0
    b       .

3:
    mov     x0, x20
    mov     x1, #4
    mov     x8, #0
    svc     #0
    adr     x0, msg_badptr
    mov     x1, #(msg_badptr_end - msg_badptr)
    mov     x8, #0
    svc     #0
    mov     x0, #0
    mov     x8, #3
    svc     #0
    b       .

4:
    ldr     x2, [x20]
    mov     x0, #9
    mov     x8, #3
    svc     #0
    b       .

5:
    mrs     x2, currentel
    mov     x0, #9
    mov     x8, #3
    svc     #0
    b       .

6:
    sub     x2, sp, #16
    br      x2

7:
    bl      futex_test
    b       .

8:
    bl      overflow
    b       .

overflow:
    sub     sp, sp, #256
    stp     x29, x30, [sp]
    bl      overflow

msg_hello:
    .ascii "hello from el0"
msg_hello_end:
msg_ticks:
    .ascii "ticks advanced"
msg_ticks_end:
msg_badsvc:
    .ascii "bad syscall survived"
msg_badsvc_end:
msg_badptr:
    .ascii "bad pointer survived"
msg_badptr_end:
