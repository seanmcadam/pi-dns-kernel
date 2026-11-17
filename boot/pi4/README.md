# Pi 4 boot progress

The firmware rainbow means `start4.elf` ran and the kernel has not yet
replaced the framebuffer. This image turns that screen on as soon as the
mailbox answers, and it blinks the green activity LED so a board with no
serial adapter still shows how far boot got.

On power-up the green LED flickers, stays solid for several seconds, and
may blink once as the firmware reads the card. That pattern is
`start4.elf`, not a kernel stage. The kernel counts below start after
that, once `kernel8.img` is running.

A single blink of about half a second is not a kernel stage and not a
documented ACT error code. Error codes repeat (four short flashes means
`start*.elf` was not found, seven short flashes means the kernel file
was not found). On the large kernel image that flash matches the
firmware finishing the read. Raspberry Pi OS on another card reaches a
login prompt on this board, so the board, the monitor, the cable, and
the EEPROM are fine.

The card that left the rainbow up used firmware master `start4.elf`
and `fixup4.dat` and did not have `bcm2711-rpi-4-b.dtb`. That pair is
matched to itself. It is not the March 2025 pair. With the March 2025
trio, the same diagnostic did start: after the firmware's solid LED
there was a quick blink, GPIO 42 stayed solid on, and HDMI went from
the rainbow to no signal. That image asked for a 640x480 framebuffer
and sent a blank tag before it cleared the pin. The diagnostic now
toggles GPIO 42 from `CNTPCT_EL0` before any mailbox call, then
allocates the firmware's current framebuffer without setting a new
mode. Setting that same size is the fallback. There is no blank tag.
1280x720 is used only when the firmware reports no size. The panel is
filled red. A rainbow that stays, with no toggle, still means
the firmware did not enter the file. The diagnostic build is in
`boot/pi4-blinktest/README.md`.

## ACT LED

Current Pi 4 device trees attach the green activity LED to **GPIO 42**,
active high (`STATUS_LED_G_CLK` in `bcm2711-rpi-4-b.dts`). The firmware
GPIO expander pin 2 is mailbox GPIO **130** (`128 + 2`) and is the **red
power LED** on that same tree (`PWR_LED_OFF`, active low). Older notes
call pin 130 the activity LED. Rust pulses both, with the same count.
GPIO 42 is what lights the green LED on current boards. The expander
uses mailbox tags `0x00038043` (SET_GPIO_CONFIG) and `0x00038041`
(SET_GPIO_STATE). The call tries the `0xC0000000` bus alias, then the
raw physical address. A timed-out or refused pair is not sent again.
The expander stays quiet until the framebuffer mailbox transaction has
finished. GPIO 42 itself does not use the mailbox, and its delay is
`CNTPCT_EL0`, so a stuck property channel cannot hold the green LED on.

Assembly owns the early counts. It talks only to GPIO 42. Each stage is
N flashes of 250 ms on and 250 ms off, then a 1.5 s gap. The on and off
times are `CNTFRQ >> 2`. The gap is three `CNTFRQ >> 1` waits. The group
is the stage that is about to run. The pin is left on while that stage
runs, so a hang there stays lit. Virt and Pi 5 do not touch the pin.

Core 0 points the current stack and `SP_EL1` at `__stack_top` before the
first call. The delay reads `CNTPCT_EL0` and subtracts. A `CNTFRQ_EL0` of
0 is treated as 54 MHz. A counter that does not move gives up after a
fixed number of polls. The pin is switched to an output once; later
flashes only set or clear it. The counts below are unchanged.

The raw file starts with a Linux arm64 Image header. The first instruction
is a PC-relative branch over that header. Nothing before the MMU is on
loads an absolute high address. `kernel_address=0x80000` asks for the
link address. If the firmware still enters at `0x200000`, this image
copies itself to `0x80000` and continues there. The LED path follows the
Pi 4 link, not "is the PC in the first megabyte", so that load does not
take the virt cache walk and does not skip the LED.

`VBAR_EL1` and `VBAR_EL2` point at a physical vector table before the
cache invalidate returns and before the MMU is enabled. A fault in that
window used to hang with the LED already off, because the reset vector
is not a blink. The stub flashes three times quickly, then repeats the
stage number at the slow rate, forever. A stage of 0 repeats only the
three rapid flashes. The loop never finishes on an off period. A panic
or a hang after the virtual timer is armed uses the same shape, with
the last stage Rust recorded. That repeat does not advance, so the
number can be counted after the fact.

## Blink codes

Count the flashes in a group. A flash is about a quarter of a second,
and the gap before the next group is about a second and a half. A
healthy boot walks 1 through 14 in order, then one quarter-second flash
repeats with that same gap. That heartbeat means the idle loop is
running. The framebuffer is programmed before the strict page tables,
so 12 comes before 13.

This image does not issue a set/way op to L2. Stage 1 only reads the L1
geometry, and that read happens after the one-flash announcement. The
blink and the delay are leaves, the pin is programmed once, and a stuck
counter cannot sit in the gap forever. The counts are unchanged.

| Count | What is about to run | If three rapid flashes, then this count, repeat |
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
| 11 | Rust entered `kernel_main` | High vectors. The rapid three, then the last count Rust stored |
| 12 | HDMI framebuffer programmed | Rainbow should already be gone, or the link stayed up |
| 13 | Strict kernel page tables installed | |
| 14 | Scheduler and user tasks started | After this group the heartbeat replaces the count |
| Heartbeat | One 250 ms flash, 1.5 s off, repeating | The idle loop is running |

Stage 14 is not skipped when the count seems to stop at 13. `pattern(13)`
leaves GPIO 42 on, and the pin stays solid through the allocator, the
filesystem, and GENET bring-up. That solid stretch is stage 13's work,
not a hang, as long as it ends. Stage 14 then flashes fourteen times.
After that the idle loop shows one 250 ms flash and a 1.5 s gap. A photo
taken once the console is reprinting status is looking at that heartbeat.
The HDMI log prints `[OK] LED stage 14` before the fourteen flashes and
`[OK] LED heartbeat` when the idle loop starts. The pinned top row ends
in `led:13` during the solid stretch and `led:heartbeat` afterwards.
Three rapid flashes and then thirteen slow flashes, forever, is the hang
pattern with the last stored stage still 13. The status line in the
console means that pattern did not stick.
| Rapid three, forever | Fault before a count was stored | |

On `413ef6d` the board showed stage flashing, then a faster stretch,
then the LED went out. That image used 50–60 ms flashes, so stages
11–14 look like one fast run, and `pattern` ended by clearing GPIO 42.
The idle loop never set the pin again. UART setup writes `GPFSEL1`
(GPIO 14 and 15), not `GPFSEL4`. The expander mailbox drives pin 130
only, and only after the framebuffer call. Nothing in the dashboard or
the idle loop turns GPIO 42 off. Dark after the fast stretch was the
designed end of stage 14, or a hang that started while the pin was
already clear. A fault in the physical stub would have kept repeating.
Panic was a 40 ms blink that also did not stop. Off now means the
indicator itself stopped: the core is not in the idle loop, not in a
fault loop, and not in a stage that left the pin on. After the virtual
timer is armed, 60 s without a blink or a stage wait starts the fault
pattern instead of staying dark. A hang with interrupts masked, before
that timer, stays in whatever state the last stage left (lit, while
that stage is running).

A group that plays once, then the next group, is progress. Three rapid
flashes and then the same group forever is a fault in that stage. A
single half-second flash, with the rainbow still up, is the firmware,
not a stage.

## HDMI network row

The top row of the framebuffer does not scroll. It shows the GENET
revision (raw word and decoded version; silicon major 6 is version 5),
link up or down, speed, MAC, DHCP state (`wait`, `leased`, `timeout`,
`static`, or `off`), the IPv4 address, and the LED label. Probe steps
are also logged as `[OK]`, `[WARN]`, and `[FAIL]` lines in the scrolling
region. `nic=` in the `[status]` line is a GIC interrupt count. Zero
there does not mean the controller is missing. QEMU raspi4b has no GENET
block, so that image prints `[FAIL] GENET revision register is not present`
and `[WARN] No NIC`.

`./scripts/build.sh pi4` enables DHCP. Without that feature the image
uses static `10.0.2.15` and does not send a discover.

## Files on the card

Raspberry Pi OS from another card reaches a login prompt on this Pi, so
the board and the EEPROM can boot. That card's boot FAT (Bookworm label
`bootfs`, files in the root) contains `start4.elf`, `fixup4.dat`,
`bcm2711-rpi-4-b.dtb`, `overlays/`, `config.txt`, `cmdline.txt`, and
`kernel8.img`. Ours had `start4.elf` and `fixup4.dat` and did not have
the dtb. A 2019 bare-metal port did not boot a Pi 4 without
`bcm2711-rpi-4-b.dtb`. Later firmware can boot some images with no dtb
if `enable_gic=1` is set and `device_tree=` is empty, but Raspberry Pi
OS always ships the dtb, and a missing tree is the difference between
the card that reaches a login prompt and the card that leaves the
rainbow up. `bootcode.bin` is not read on a Pi 4. `overlays/` is only
for `dtoverlay=` lines, which this config does not use. `cmdline.txt`
is for Linux. `armstub8-gic.bin` is not required; the firmware has its
own stub.

The two `start4.elf` files are not the same build, and the failing pair
was not mixed.

| Card | `start4.elf` | `fixup4.dat` | `bcm2711-rpi-4-b.dtb` |
| --- | --- | --- | --- |
| Raspberry Pi OS, boots to a login prompt | 2263840 bytes, built Mar 19 2025, `VC_BUILD_ID` `ca6e8171a80ea46924ffaa629250bfb482f3a02c` | 5459 bytes | 56211 bytes |
| Public firmware commit `80f23d4be5` (Mar 19 2025) | 2263840 | 5459 | 56211 |
| Bare-metal card that left the rainbow up | 2289248 bytes, `VC_BUILD_ID` `fed420a1c80559ebb4830efe4d8073976d3ae518` (Nov 16 2026) | 5413 bytes | absent |
| Public firmware master `d131b57900` (Nov 16 2026) | 2289248 | 5413 | 56431, not copied onto the card |

`ca6e8171` and `fed420a1` are the `VC_BUILD_ID_VERSION` strings inside
`start4.elf`. They are not commits in `raspberrypi/firmware`. The public
commit that contains the working bytes is `80f23d4be5`. The public
commit that contains the failing bytes is master `d131b57900`, and its
`fixup4.dat` is the 5413-byte file. Those two files were published
together. The missing piece on that card is the dtb, which master does
ship.

Both binaries log `Failed to load Device Tree file` and, separately,
`Starting ARM`. The March 2025 firmware that boots this Pi has the same
two strings, so a missing dtb is not a new master-only refusal that can
be read out of the file. `os_check` is the Pi 5 "no compatible kernel"
check. The Pi OS `config.txt` does not set it, and it does set
`arm_boost=1`, `disable_overscan=1`, and `dtoverlay=vc4-kms-v3d`. It
does not set `kernel_address`, `core_freq`, or the UART lines. Do not
copy that `config.txt`: `dtoverlay=vc4-kms-v3d` needs `overlays/` and
the Linux display stack. Do not set `device_tree=` to an empty value.

Use the March 2025 trio from the working card. Do not pair master's
`start4.elf` with the March dtb or fixup. A Pi 400 uses
`bcm2711-rpi-400.dtb`. A CM4 uses `bcm2711-rpi-cm4.dtb`. Every Pi 4B
revision, including rev 1.5 and the 8 GB boards, uses
`bcm2711-rpi-4-b.dtb`.

`./scripts/fetch-pi4-boot.sh` downloads the March 2025 trio from
`80f23d4be5` when that card is not mounted. The test already running
uses the card's copies, which are the same bytes.

`config.txt` for that test is:

```
arm_64bit=1
kernel=kernel8.img
kernel_address=0x80000
enable_uart=1
```

`kernel_address=0x80000` is the link address. `enable_uart=1` turns the
PL011 on. These lines stay out until the image is known to start:
`os_check=0`, `uart_2ndstage=1`, `core_freq=250`,
`init_uart_clock=48000000`, `hdmi_force_hotplug=1`,
`disable_overscan=1`, `arm_boost=1`, and `dtoverlay=vc4-kms-v3d`.
After the LED blinks, add `init_uart_clock=48000000` and `core_freq=250`
before trusting the serial baud. The PL011 divisor assumes 48 MHz.

The boot FAT root for this test should contain `start4.elf`,
`fixup4.dat`, `bcm2711-rpi-4-b.dtb`, `config.txt`, and `kernel8.img`.

### Blink test

On desktop2, from a current tree:

```
sudo apt-get install gcc-aarch64-linux-gnu
./scripts/fetch-pi4-boot.sh blinktest
```

Overwrite `boot/pi4/card/start4.elf`, `fixup4.dat`, and
`bcm2711-rpi-4-b.dtb` from the Pi OS boot FAT if that card is mounted.
Mount the bare-metal card's boot FAT and replace its root with the
staged files:

```
cp boot/pi4/card/* /path/to/bare-boot/
sync
```

Power on. After the firmware's solid green LED, GPIO 42 should blink
four times, about one second per blink, before any mailbox call. The
panel should then leave the rainbow and stay red, and the green LED
should keep blinking. A rainbow that stays, with no toggle, means
`start4.elf` still did not enter `kernel8.img`. No signal with the LED
stuck on is the previous diagnostic: it programmed the framebuffer
before the first off write.

### DNS kernel

`./scripts/build-pi4.sh`, then `./scripts/fetch-pi4-boot.sh kernel`, then
the same copy. `boot/pi4/config.txt` is the same four lines. The ACT
counts in the table above apply only after the firmware enters the image.
