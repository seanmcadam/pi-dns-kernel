# Pi 4 blink and HDMI diagnostic

This is not the DNS kernel. The image is about 1.8 KB of assembly. It
answers one question: did the firmware enter our file, and can GPIO 42
toggle before the mailbox framebuffer request?

The image does not use the stack or the MMU. Code addresses are
PC-relative, so the same file runs if the firmware enters at `0x80000`
or at `0x200000`. Device addresses are constants in the low peripheral
window (`0xFE000000`). The delay reads `CNTPCT_EL0`. A `CNTFRQ_EL0` of
0 is treated as 54 MHz, which is a half-second period on a 1.5 GHz or
1.8 GHz Pi 4. A counter that does not advance returns after a fixed
number of polls.

## What it does

1. Makes GPIO 42 an output and blinks it four times. Nothing touches the
   mailbox during those blinks. This is the green ACT LED on current
   Pi 4 boards, active high.
2. Configures firmware GPIO expander pin 130 once (`SET_GPIO_CONFIG`,
   `0x00038043`). A refused call does not stop the blink.
3. Reads the current physical display size (`GET_PHYSICAL_DISPLAY`,
   `0x00040003`). When that size is sane it allocates the buffer the
   firmware is already scanning, with no set-size tags and no blank
   tag. Setting that same size is the fallback. 1280x720 is used only
   when the firmware reports no size. The bus pointer is masked with
   `0x3FFFFFFF`. A pointer outside RAM is ignored. The panel is filled
   solid red (`0x000000FF`, or `0x00FF0000` if the firmware answers
   BGR). Each mailbox wait gives up after `1<<24` polls.
4. Drives pin 130 once, then blinks GPIO 42 forever. The expander is
   not called between a set and the matching clear, so a mailbox wait
   cannot leave the green LED stuck on.

| What you see | What it means |
| --- | --- |
| Green LED blinks, then the panel turns red and the LED keeps blinking | This image is running and HDMI accepted the framebuffer |
| Green LED blinks and the panel is no signal | This image is running. The framebuffer request dropped the link |
| Rainbow stays, and the green LED never toggles | The firmware did not enter this file |

The March 2025 boot trio plus the previous build of this file did the
second row: the LED went solid on and HDMI left the rainbow. That build
sent the framebuffer request first.

## Build on desktop2

The script uses the system AArch64 binutils. It does not use rustc.

```
sudo apt-get install gcc-aarch64-linux-gnu
cd pi4-dns-kernel
./scripts/build-pi4-blinktest.sh
```

That writes `boot/pi4-blinktest/kernel8.img`. `config.txt` in the same
directory is the handoff file (`arm_64bit=1`, `kernel=kernel8.img`,
`kernel_address=0x80000`, `enable_uart=1`). The card also needs the
March 2025 `start4.elf`, `fixup4.dat`, and `bcm2711-rpi-4-b.dtb`. The
copy steps are in `boot/pi4/README.md`.

```
./scripts/fetch-pi4-boot.sh blinktest
cp boot/pi4/card/* /path/to/bare-boot/
sync
```
