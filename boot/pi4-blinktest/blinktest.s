// Pi 4 diagnostic. No stack and no MMU. The delay reads CNTPCT_EL0.
// Every code address is a branch or an ADR, and device addresses are
// immediate constants. It runs at 0x80000 or 0x200000.
//
// GPIO 42 toggles before any mailbox call. A stuck property channel
// cannot hold the green LED on. The framebuffer request uses the
// firmware's current size, or 1280x720, and does not send a blank tag.
//
// Do not use curly braces in this file. The diagnostic is assembled with
// gas, which has no such rule, and the kernel's boot.s must stay free of
// them too. Keeping this file clean avoids a copy-paste into boot.s.

.section .text
.global _start

_start:
    b       blink_body
    .word   0
    .quad   0x80000
    .quad   blink_end - _start
    .quad   0
    .quad   0
    .quad   0
    .quad   0
    .word   0x644d5241
    .word   0

blink_body:
    mrs     x1, mpidr_el1
    and     x1, x1, #0xff
    cbnz    x1, park
    bl      gpio42_output
    mov     w20, #4
pre_blink:
    cbz     w20, pre_done
    mov     w0, #1
    bl      gpio42_level
    bl      delay
    mov     w0, #0
    bl      gpio42_level
    bl      delay
    sub     w20, w20, #1
    b       pre_blink
pre_done:
    bl      exp_config
    bl      screen_red
    mov     w0, #0
    bl      exp_level
blink_loop:
    mov     w0, #1
    bl      gpio42_level
    bl      delay
    mov     w0, #0
    bl      gpio42_level
    bl      delay
    b       blink_loop

park:
    wfe
    b       park

// Half a second. CNTFRQ_EL0 of 0 is the Pi 4 54 MHz counter. A counter
// that does not advance returns after 1<<28 polls, so the LED still
// toggles. The period does not depend on a 1.5 GHz or 1.8 GHz loop.
delay:
    mrs     x1, cntfrq_el0
    cbnz    x1, delay_have
    movz    x1, #0xf980
    movk    x1, #0x0337, lsl #16
delay_have:
    lsr     x1, x1, #1
    mrs     x2, cntpct_el0
    mov     x3, #1
    lsl     x3, x3, #28
delay_poll:
    subs    x3, x3, #1
    b.eq    delay_ret
    mrs     x4, cntpct_el0
    sub     x4, x4, x2
    cmp     x4, x1
    b.lo    delay_poll
delay_ret:
    ret

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

// w0 is 0 to clear GPIO42, anything else to set it.
gpio42_level:
    movz    x1, #0x002C
    cbz     w0, gpio42_store
    movz    x1, #0x0020
gpio42_store:
    movk    x1, #0xFE20, lsl #16
    mov     w2, #0x400
    str     w2, [x1]
    ret

// Direction output, polarity active high, pin 130. A refused call does
// not stop the GPIO42 blink. The state tag is still attempted.
exp_config:
    mov     x29, x30
    bl      exp_cfg_fill
    adr     x0, exp_cfg
    mov     x1, #48
    mov     x2, #1
    bl      mbox_call
    cbnz    w0, exp_config_ret
    bl      exp_cfg_fill
    adr     x0, exp_cfg
    mov     x1, #48
    mov     x2, #0
    bl      mbox_call
exp_config_ret:
    mov     x30, x29
    ret

exp_cfg_fill:
    adr     x10, exp_cfg
    mov     w1, #48
    str     w1, [x10]
    str     wzr, [x10, #4]
    movz    w1, #0x8043
    movk    w1, #0x0003, lsl #16
    str     w1, [x10, #8]
    mov     w1, #24
    str     w1, [x10, #12]
    str     wzr, [x10, #16]
    mov     w1, #130
    str     w1, [x10, #20]
    mov     w1, #1
    str     w1, [x10, #24]
    str     wzr, [x10, #28]
    str     wzr, [x10, #32]
    str     wzr, [x10, #36]
    str     wzr, [x10, #40]
    str     wzr, [x10, #44]
    ret

// w0 is the SET_GPIO_STATE value for pin 130. Called once, after the
// framebuffer, never between a GPIO42 set and the matching clear.
exp_level:
    mov     x29, x30
    mov     w19, w0
    bl      exp_state_fill
    adr     x0, exp_state
    mov     x1, #32
    mov     x2, #1
    bl      mbox_call
    cbnz    w0, exp_level_ret
    bl      exp_state_fill
    adr     x0, exp_state
    mov     x1, #32
    mov     x2, #0
    bl      mbox_call
exp_level_ret:
    mov     x30, x29
    ret

exp_state_fill:
    adr     x10, exp_state
    mov     w1, #32
    str     w1, [x10]
    str     wzr, [x10, #4]
    movz    w1, #0x8041
    movk    w1, #0x0003, lsl #16
    str     w1, [x10, #8]
    mov     w1, #8
    str     w1, [x10, #12]
    str     wzr, [x10, #16]
    mov     w1, #130
    str     w1, [x10, #20]
    str     w19, [x10, #24]
    str     wzr, [x10, #28]
    ret

// Read the current physical size, then paint that panel solid red.
// The first request does not set a size: it allocates the buffer the
// firmware is already scanning. Setting that same size is the fallback.
// 1280x720 is used only when the firmware reports no size. Failure
// returns, and the LED loop keeps running. w22 and w23 are the width
// and height.
screen_red:
    mov     x29, x30
    bl      get_mode
    cbz     w24, screen_set
    bl      screen_keep
    cbnz    w0, screen_ret
screen_set:
    adr     x0, fb_work
    adr     x1, fb_template
    mov     x2, #140
    bl      copy_bytes
    adr     x21, fb_work
    str     w22, [x21, #20]
    str     w23, [x21, #24]
    str     w22, [x21, #40]
    str     w23, [x21, #44]
    adr     x0, fb_work
    mov     x1, #140
    mov     x2, #1
    bl      mbox_call
    cbnz    w0, screen_have
    adr     x0, fb_work
    adr     x1, fb_template
    mov     x2, #140
    bl      copy_bytes
    adr     x21, fb_work
    str     w22, [x21, #20]
    str     w23, [x21, #24]
    str     w22, [x21, #40]
    str     w23, [x21, #44]
    adr     x0, fb_work
    mov     x1, #140
    mov     x2, #0
    bl      mbox_call
    cbz     w0, screen_ret
screen_have:
    adr     x21, fb_work
    ldr     w22, [x21, #20]
    ldr     w23, [x21, #24]
    cmp     w22, #320
    b.lo    screen_ret
    cmp     w22, #3840
    b.hi    screen_ret
    cmp     w23, #240
    b.lo    screen_ret
    cmp     w23, #2160
    b.hi    screen_ret
    ldr     w0, [x21, #112]
    movz    w1, #0xffff
    movk    w1, #0x3fff, lsl #16
    and     w0, w0, w1
    cbz     w0, screen_ret
    cmp     w0, #0x10000
    b.lo    screen_ret
    movz    w1, #0xFC00, lsl #16
    cmp     w0, w1
    b.hs    screen_ret
    ldr     w3, [x21, #132]
    lsl     w4, w22, #2
    cmp     w3, w4
    b.hs    screen_pitch
    mov     w3, w4
screen_pitch:
    umull   x1, w3, w23
    movz    x5, #0x80, lsl #16
    cmp     x1, x5
    b.ls    screen_bytes
    mov     x1, x5
screen_bytes:
    movz    x6, #0xFC00, lsl #16
    adds    x7, x0, x1
    b.cs    screen_ret
    cmp     x7, x6
    b.hi    screen_ret
    ldr     w2, [x21, #76]
    movz    w3, #0x00FF
    cbnz    w2, screen_fill
    movz    w3, #0x0000
    movk    w3, #0x00FF, lsl #16
screen_fill:
    mov     x4, x0
    mov     x5, x1
screen_pix:
    cbz     x5, screen_clean
    str     w3, [x4], #4
    sub     x5, x5, #4
    b       screen_pix
screen_clean:
    mov     x4, x0
    add     x5, x0, x1
screen_line:
    cmp     x4, x5
    b.hs    screen_ret
    dc      civac, x4
    add     x4, x4, #64
    b       screen_line
screen_ret:
    dsb     sy
    mov     x30, x29
    ret

// Allocate the firmware's current buffer. No set-size and no blank tag.
// w0 is 1 when the panel was filled. w22 and w23 stay the width and height.
screen_keep:
    mov     x28, x30
    adr     x0, fb_work
    adr     x1, fb_keep
    mov     x2, #100
    bl      copy_bytes
    adr     x0, fb_work
    mov     x1, #100
    mov     x2, #1
    bl      mbox_call
    cbnz    w0, keep_have
    adr     x0, fb_work
    adr     x1, fb_keep
    mov     x2, #100
    bl      copy_bytes
    adr     x0, fb_work
    mov     x1, #100
    mov     x2, #0
    bl      mbox_call
    cbz     w0, keep_fail
keep_have:
    adr     x21, fb_work
    ldr     w4, [x21, #76]
    cmp     w4, #32
    b.ne    keep_fail
    ldr     w0, [x21, #40]
    movz    w1, #0xffff
    movk    w1, #0x3fff, lsl #16
    and     w0, w0, w1
    cbz     w0, keep_fail
    cmp     w0, #0x10000
    b.lo    keep_fail
    movz    w1, #0xFC00, lsl #16
    cmp     w0, w1
    b.hs    keep_fail
    ldr     w3, [x21, #60]
    lsl     w4, w22, #2
    cmp     w3, w4
    b.lo    keep_fail
    umull   x1, w3, w23
    movz    x5, #0x80, lsl #16
    cmp     x1, x5
    b.hi    keep_fail
    movz    x6, #0xFC00, lsl #16
    adds    x7, x0, x1
    b.cs    keep_fail
    cmp     x7, x6
    b.hi    keep_fail
    ldr     w2, [x21, #92]
    movz    w3, #0x00FF
    cbnz    w2, keep_paint
    movz    w3, #0x0000
    movk    w3, #0x00FF, lsl #16
keep_paint:
    b       screen_fill
keep_fail:
    mov     w0, #0
    mov     x30, x28
    ret

// w22 width, w23 height. GET_PHYSICAL_DISPLAY uses tag 0x00040003.
// Width is accepted from 320 to 3840, height from 240 to 2160.
// A rejected read becomes 1280x720 for the set-size fallback only.
get_mode:
    mov     x28, x30
    bl      fill_get
    adr     x0, fb_work
    mov     x1, #32
    mov     x2, #1
    bl      mbox_call
    cbnz    w0, get_read
    bl      fill_get
    adr     x0, fb_work
    mov     x1, #32
    mov     x2, #0
    bl      mbox_call
    cbz     w0, get_default
get_read:
    adr     x10, fb_work
    ldr     w22, [x10, #20]
    ldr     w23, [x10, #24]
    cmp     w22, #320
    b.lo    get_default
    cmp     w22, #3840
    b.hi    get_default
    cmp     w23, #240
    b.lo    get_default
    cmp     w23, #2160
    b.hi    get_default
    mov     w24, #1
    b       get_ret
get_default:
    mov     w22, #1280
    mov     w23, #720
    mov     w24, #0
get_ret:
    mov     x30, x28
    ret

fill_get:
    adr     x10, fb_work
    mov     w1, #32
    str     w1, [x10]
    str     wzr, [x10, #4]
    movz    w1, #0x0003
    movk    w1, #0x0004, lsl #16
    str     w1, [x10, #8]
    mov     w1, #8
    str     w1, [x10, #12]
    str     wzr, [x10, #16]
    str     wzr, [x10, #20]
    str     wzr, [x10, #24]
    str     wzr, [x10, #28]
    ret

// x0 = dest, x1 = src, x2 = length in bytes.
copy_bytes:
    mov     x4, #0
copy_loop:
    cmp     x4, x2
    b.hs    copy_ret
    ldr     w5, [x1, x4]
    str     w5, [x0, x4]
    add     x4, x4, #4
    b       copy_loop
copy_ret:
    ret

// x0 buffer, x1 length, x2 nonzero to set the 0xC0000000 bus alias.
// w0 is 1 when the property channel returns code 0x80000000.
// Each wait gives up after 1<<24 polls.
mbox_call:
    mov     x9, x0
    mov     x10, x1
    add     x11, x0, x1
mbox_clean:
    cmp     x0, x11
    b.hs    mbox_send
    dc      civac, x0
    add     x0, x0, #64
    b       mbox_clean
mbox_send:
    dsb     sy
    mov     x0, x9
    cbz     x2, mbox_phys
    movz    x3, #0xC000, lsl #16
    orr     x0, x0, x3
mbox_phys:
    bic     x0, x0, #0xf
    orr     x0, x0, #8
    movz    x3, #0xB880
    movk    x3, #0xFE00, lsl #16
    mov     x4, #1
    lsl     x4, x4, #24
mbox_wait_room:
    subs    x4, x4, #1
    b.eq    mbox_fail
    ldr     w5, [x3, #0x18]
    tbnz    w5, #31, mbox_wait_room
    str     w0, [x3, #0x20]
    mov     x4, #1
    lsl     x4, x4, #24
mbox_wait_reply:
    subs    x4, x4, #1
    b.eq    mbox_fail
    ldr     w5, [x3, #0x18]
    tbnz    w5, #30, mbox_wait_reply
    ldr     w5, [x3]
    and     w6, w5, #0xf
    cmp     w6, #8
    b.ne    mbox_wait_reply
    mov     x0, x9
    add     x11, x9, x10
mbox_inval:
    cmp     x0, x11
    b.hs    mbox_code
    dc      civac, x0
    add     x0, x0, #64
    b       mbox_inval
mbox_code:
    dsb     sy
    ldr     w0, [x9, #4]
    movz    w1, #0x0000
    movk    w1, #0x8000, lsl #16
    cmp     w0, w1
    cset    w0, eq
    ret
mbox_fail:
    mov     w0, #0
    ret

    .balign 16
fb_keep:
    .word   100
    .word   0
    .word   0x00048009
    .word   8
    .word   0
    .word   0
    .word   0
    .word   0x00040001
    .word   8
    .word   0
    .word   4096
    .word   0
    .word   0x00040008
    .word   4
    .word   0
    .word   0
    .word   0x00040005
    .word   4
    .word   0
    .word   0
    .word   0x00040006
    .word   4
    .word   0
    .word   0
    .word   0

    .balign 16
fb_template:
    .word   140
    .word   0
    .word   0x00048003
    .word   8
    .word   0
    .word   1280
    .word   720
    .word   0x00048004
    .word   8
    .word   0
    .word   1280
    .word   720
    .word   0x00048005
    .word   4
    .word   0
    .word   32
    .word   0x00048006
    .word   4
    .word   0
    .word   1
    .word   0x00048009
    .word   8
    .word   0
    .word   0
    .word   0
    .word   0x00040001
    .word   8
    .word   0
    .word   4096
    .word   0
    .word   0x00040008
    .word   4
    .word   0
    .word   0
    .word   0

    .balign 16
fb_work:
    .space  256

    .balign 16
exp_cfg:
    .word   48
    .word   0
    .word   0x00038043
    .word   24
    .word   0
    .word   130
    .word   1
    .word   0
    .word   0
    .word   0
    .word   0
    .word   0

    .balign 16
exp_state:
    .word   32
    .word   0
    .word   0x00038041
    .word   8
    .word   0
    .word   130
    .word   0
    .word   0

blink_end:
