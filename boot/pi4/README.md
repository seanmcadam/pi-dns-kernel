# Pi 4 boot progress

The firmware rainbow means `start4.elf` ran and the kernel has not yet
replaced the framebuffer. This image turns that screen on as soon as the
mailbox answers, and it blinks the green activity LED so a board with no
serial adapter still shows how far boot got.

On power-up the green LED flickers, stays solid for several seconds, and
may blink once as the firmware reads the card. That pattern is
`start4.elf`, not a kernel stage. The kernel counts below start after
that, once `kernel8.img` is running.

## ACT LED

Current Pi 4 device trees attach the green activity LED to **GPIO 42**,
active high (`STATUS_LED_G_CLK` in `bcm2711-rpi-4-b.dts`). The firmware
GPIO expander pin 2 is mailbox GPIO **130** (`128 + 2`) and is the **red
power LED** on that same tree (`PWR_LED_OFF`, active low). Older notes
call pin 130 the activity LED. Rust pulses both, with the same count.
GPIO 42 is what lights the green LED on current boards. The expander
uses mailbox tags `0x00038043` (SET_GPIO_CONFIG) and `0x00038041`
(SET_GPIO_STATE). A timed-out or refused call is not retried. The
expander stays quiet until the framebuffer mailbox transaction has
finished.

Assembly owns the early counts. It talks only to GPIO 42. Each stage is
a group of short flashes (about 60 ms on, 60 ms off) and then the same
60 ms again as the gap. On, off, and the gap share one delay, so a pause
between stages is not a different timer path. The group is the stage
that is about to run. Virt and Pi 5 do not touch the pin.

`VBAR_EL1` and `VBAR_EL2` point at a physical vector table before the
cache invalidate returns and before the MMU is enabled. A fault in that
window used to hang with the LED already off, because the reset vector
is not a blink. The stub flashes quickly eight times, then repeats the
stage number for as long as the core sits there. A stage of 0 stays on
the fast flash. That repeat is the end state of a fault: it does not
advance, so the number can be counted after the fact.

## Blink codes

Count the flashes in a group. A healthy boot walks 1 through 14 in
order, then the LED goes dark while the server runs. The framebuffer is
programmed before the strict page tables, so 12 comes before 13.

The previous image announced stage 1 and then went dark with no fast
flashes. That was the full-cache `dc isw` walk, including the shared L2,
and it never took an exception. This image does not issue a set/way op
to L2. Stage 1 only reads the L1 geometry. Stage 2 is the L1
clean-and-invalidate. The numbers from the old stage 2 onward each moved
up by one.

| Count | What is about to run | If this count repeats after eight fast flashes |
| --- | --- | --- |
| 1 | Read the L1 data-cache geometry. Physical vectors are already installed. No set/way op yet | Fault in that register read |
| 2 | Clean and invalidate L1 data only, then the instruction cache. L2 is not walked | Fault in the L1 loop |
| 3 | Drop EL2 to EL1 | Fault in the drop, or the core was not at EL1 or EL2 |
| 4 | Enable FP/SIMD and set the stack | Fault in `CPACR_EL1` or the stack pointer |
| 5 | Zero `.bss` and `.dma` | Fault in that zeroing |
| 6 | Build the coarse page tables | Fault while writing or cleaning the tables |
| 7 | Turn the MMU on | Fault programming `MAIR`, `TCR`, `TTBR`, or `SCTLR` |
| 8 | Jump to the high address | MMU is on. Fault is in the jump |
| 9 | Install the high vector base and drop the identity map | High fetch worked. Fault is in that blink |
| 10 | Call `kernel_main` | Identity map is gone. The next fault uses the high vectors |
| 11 | Rust entered `kernel_main` | High vectors: fast blink, no counted repeat |
| 12 | HDMI framebuffer programmed | Rainbow should already be gone |
| 13 | Strict kernel page tables installed | |
| 14 | Scheduler and user tasks started | |
| Fast, continuous | Panic or a fault after the high vectors are in | The physical stub is no longer installed |

A group that plays once, then the next group, is progress. A group that
plays once and is followed by eight fast flashes and then the same group
forever is a fault in that stage. A group that plays once and then the
LED stays dark, with no fast flashes, is a hang that did not take an
exception (the core is stuck in that stage, not in the vector stub).
After 14, dark is the idle loop. One flash and then dark means the core
stopped before the L1 walk (the geometry read, or the delay that stage 1
and stage 2 share). Two flashes and then dark means the L1 walk itself
did not return.

If stage 1 never plays, the firmware did not enter this image (wrong
`kernel=` / `kernel_address`, or the file on the card is not this
build). The old single half-second flash is gone. Do not wait for it.

`uart_2ndstage=1` prints firmware diagnostics on the UART only. It does
not change the LED or the panel.
