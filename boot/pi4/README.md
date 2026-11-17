# Pi 4 boot progress

The firmware rainbow means `start4.elf` ran and the kernel has not yet
replaced the framebuffer. This image turns that screen on as soon as the
mailbox answers, and it blinks the green activity LED so a board with no
serial adapter still shows how far boot got.

## ACT LED

Current Pi 4 device trees attach the green activity LED to **GPIO 42**,
active high (`STATUS_LED_G_CLK` in `bcm2711-rpi-4-b.dts`). The firmware
GPIO expander pin 2 is mailbox GPIO **130** (`128 + 2`) and is the **red
power LED** on that same tree (`PWR_LED_OFF`, active low). Older notes
call pin 130 the activity LED. This kernel pulses both, with the same
count. GPIO 42 is what lights the green LED on current boards. The
expander uses mailbox tags `0x00038043` (SET_GPIO_CONFIG) and
`0x00038041` (SET_GPIO_STATE). A timed-out or refused call is not
retried. The expander stays quiet until the framebuffer mailbox
transaction has finished.

The earliest mark is pure assembly, before Rust, with the MMU off. It is
one long flash (about half a second), not a counted group.

## Blink codes

Groups are short flashes of about 100 ms, separated by a gap. A healthy
boot is not in numeric order: the panel is programmed before the strict
page tables, on purpose.

| What you see | Meaning |
| --- | --- |
| One long flash | `boot.s` is running on core 0, caches discarded, MMU still off |
| 1 short flash | Rust `kernel_main` has been entered |
| 3 short flashes | HDMI framebuffer is programmed (rainbow should already be gone) |
| 2 short flashes | Strict kernel page tables are installed |
| 4 short flashes | Scheduler and user tasks have been started |
| Fast continuous blink | Panic, unexpected exception level, or a kernel fault halt |

If the long flash never happens, the firmware did not enter this image
(wrong `kernel=` / `kernel_address`, or the file on the card is not this
build). A long flash and then nothing else means the core died in the
EL2 drop, the early page tables, or the jump to Rust. One flash and no
panel means the framebuffer mailbox did not complete. One flash, a
panel, and no second group means the strict map did not finish.

`uart_2ndstage=1` prints firmware diagnostics on the UART only. It does
not change the LED or the panel.
