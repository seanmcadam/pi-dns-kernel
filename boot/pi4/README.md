# Pi 4 boot progress

The firmware rainbow means `start4.elf` ran and the kernel has not yet
replaced the framebuffer. This image turns that screen on as soon as the
mailbox answers. The HDMI console is the boot indicator. The green
activity LED is not a progress counter.

`hdmi_force_hotplug` is not in the generated `config.txt`, the blink-test
handoff, or the tryboot preset. On 0.1.67 that line made a power-on boot
report 640x480 and left the rainbow up. The network updater still does
not rewrite the live `config.txt`. If that line was copied onto the card,
it has to be removed by mounting the card.

A watchdog or tryboot reboot releases the framebuffer (mailbox tag
`0x00048001`) and tries the blank tag (`0x00040002`) first. Both are
bounded by the mailbox timeout and do not hold the reset. If boot then
sees a 0x0 size, it power-cycles firmware power domain 5, waits, reads
EDID block 0 (`0x00030020`), and queries the size again. A size the
firmware reports is kept. Only a size that is still missing asks for
1280x720 and then 640x480. `/status` shows `hdmi_state`, `hdmi_mode`,
and `hdmi_recoveries`.

Domain 5 is the old single-HDMI power id. Pi 4 has two HDMI ports, and
the firmware may answer that the device does not exist. Blank is not
used on a good boot: sending it while a mode already existed dropped
this board to no signal. The release and power replies are logged so a
refused tag is visible. Virt and Pi 5 do not run this path.

On power-up the green LED flickers, stays solid for several seconds, and
may blink once as the firmware reads the card. That pattern is
`start4.elf`, not this kernel. Error codes repeat (four short flashes
means `start*.elf` was not found, seven short flashes means the kernel
file was not found). A single half-second flash matches the firmware
finishing the read. Raspberry Pi OS on another card reaches a login
prompt on this board, so the board, the monitor, the cable, and the
EEPROM are fine.

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
call pin 130 the activity LED. This image does not pulse either pin
during boot. GPIO 42 is what lights the green LED on current boards,
and only a fault drives it. Virt and Pi 5 do not touch the pin.

The raw file starts with a Linux arm64 Image header. The first instruction
is a PC-relative branch over that header. Nothing before the MMU is on
loads an absolute high address. `kernel_address=0x80000` asks for the
link address. If the firmware still enters at `0x200000`, this image
copies itself to `0x80000` and continues there.

`VBAR_EL1` and `VBAR_EL2` point at a physical vector table before the
cache invalidate returns and before the MMU is enabled. A fault in that
window lights GPIO 42 once and waits. It does not blink a stage number.

## LED

The boot-stage blink sequence and the idle heartbeat are gone. They
existed to show progress before the HDMI console worked, and each
counted group cost a quarter-second per flash plus a 1.5 s gap. The
console is the boot indicator now. GPIO 42 stays off through a normal
boot. The pinned row says `led:off`.

A fault lights the pin once and leaves it on (`led:fault`). The physical
vector stub does that before the framebuffer exists, and then waits.
A kernel panic paints `LOCKUP halt` and lights the pin. After the idle
loop starts, a core that has taken a timer tick and then stops
checkpointing for 2 seconds freezes the panel with `STALL` plus that
core's id and PC. The report goes straight to the UART data register
and the framebuffer, with no log queue and no console lock. Before the
idle loop the window is 4 seconds while timer IRQs are arriving and 15
seconds when they are not. A heap allocation that does not fit
paints `LOCKUP heap` and returns failure; it does not halt and does not
light the pin by itself. None of these blink. The paint does not take
the console lock and does not allocate, so a stuck painter still leaves
a line. The status refresh overwrites that line only if the idle loop
is still running. Virt and Pi 5 do not touch GPIO.

UART setup writes `GPFSEL1` (GPIO 14 and 15), not `GPFSEL4`. A single
half-second flash with the rainbow still up is the firmware, not this
image. Solid green after the console has been updating means the pin
was turned on by a fault, or the core stopped while the pin was already
on. Dark while the console is still scrolling is a healthy boot.

## HDMI network rows

The top rows of the framebuffer do not scroll. The first shows the
GENET revision (raw word and decoded version; silicon major 6 is version
5), link up or down, speed, MAC, DHCP state (`wait`, `leased`, `timeout`,
`static`, or `off`), the IPv4 address, and the LED label. The next row
is `tx` frames this driver queued, `rx` frames it accepted, and `rxE`
descriptors it rejected. The fourth row is the per-core heartbeat:
each core's age in milliseconds since its last checkpoint, `z` when
that core is in `wfi`, or `-` before it has checkpointed.

Linux `bcmgenet_mii_config` sets `ID_MODE_DIS` only for
`PHY_INTERFACE_MODE_RGMII`. `rgmii-rxid` leaves it clear, so the MAC
inserts the TX delay and the PHY inserts the RX delay. The PHY TX-clock
shadow (register 0x1c, shadow 3, bit 9) is cleared so the delay is not
applied twice. At 100 Mbit the MAC suppresses its internal delay. To
try that, set `network.speed` to `100` in `CONFIG.JSN` (or build with
`--features phy-100`). `0` is autonegotiation. `network.arp_secs` is how
often a gratuitous ARP is sent after the address is installed. The Pi 4
default when the key is absent is 2 seconds. `0` sends one.
Probe steps are also logged as `[OK]`, `[WARN]`, and `[FAIL]` lines in
the scrolling region. `nic=` in
the `[status]` line is a GIC interrupt count. Zero there does not mean
the controller is missing. QEMU raspi4b has no GENET block, so that
image prints `[FAIL] GENET revision register is not present` and
`[WARN] No NIC`.

Pi 4 `phy-mode` is `rgmii-rxid`. The PHY adds the RX clock delay and the
MAC adds the 2 ns TX delay (`ID_MODE_DIS` clear, `RGMII_MODE_EN` set).
The only transmit ring is queue 16, and it owns all 256 TDMA
descriptors (U-Boot's layout). They live at GENET+`0x4000`, 12 bytes
each: length/status, address low, address high. The producer is ring
offset `0x0C` and the consumer is `0x08`. The consumer is not written.
The software write pointer is the word index of the next free
descriptor. Only 32 frame buffers are kept, so at most 32 descriptors
are in flight; the index into the 256-deep ring is the free-running
producer. The receive ring is the same depth and has one buffer per
descriptor, because the MAC is the producer and will fill every
descriptor before it waits. GENET's GIC lines are masked in the handler and re-armed from
the poll path only while the interrupt summary is clear.

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
PL011 on. `hdmi_force_hotplug` stays out. On 0.1.67 it made a power-on
boot report 640x480 and left the rainbow up. A soft reboot that still
sees 0x0 is handled in the kernel: release the framebuffer before
reset, then power-cycle and re-query at the next boot. The console is
output-only: this does not enable a keyboard, a mouse, or the KMS
overlay. These lines stay out until the
image is known to start: `os_check=0`, `uart_2ndstage=1`,
`core_freq=250`, `init_uart_clock=48000000`, `disable_overscan=1`,
`arm_boost=1`, and `dtoverlay=vc4-kms-v3d`.
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
the same copy. `boot/pi4/config.txt` is those four lines.
`hdmi_force_hotplug` stays out. The ACT
counts in the table above apply only after the firmware enters the image.
