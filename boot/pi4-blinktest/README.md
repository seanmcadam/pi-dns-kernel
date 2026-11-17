# Pi 4 blink and HDMI diagnostic

This is not the DNS kernel. The image is about 1.4 KB of assembly. It
answers one question: did the firmware enter our file, and which LED
path can this board see?

The image does not use the stack, the MMU, or the timer. Code addresses
are PC-relative, so the same file runs if the firmware enters at
`0x80000` or at `0x200000`. Device addresses are constants in the low
peripheral window (`0xFE000000`).

## What it does

1. Asks the mailbox for a 640x480 framebuffer and fills it solid red.
   The panel leaves the rainbow and stays red. A solid color that is not
   red still means the image is running; the pixel-order tag is what
   makes it red.
2. Blinks GPIO 42, active high, with a busy loop. On a 1.5 GHz
   Cortex-A72 that is about one cycle per second. This is the green ACT
   LED on current Pi 4 boards.
3. Drives firmware GPIO expander pin 130 through mailbox
   `SET_GPIO_CONFIG` (`0x00038043`) and `SET_GPIO_STATE` (`0x00038041`).
   The state is the opposite of GPIO 42, so the expander LED is on while
   GPIO 42 is off. On current boards pin 130 is the red power LED.

| What you see | What it means |
| --- | --- |
| Panel turns solid red | This image is running |
| Green LED blinks about once a second | GPIO 42 is that LED |
| The other LED blinks out of phase with the green one | Expander pin 130 is that LED |
| Red panel, neither LED moves | Code runs; neither pin is the LED you are watching |
| Rainbow stays, one blink of about half a second, then dark | The firmware did not enter this file |

## Build on desktop2

The script uses the system AArch64 binutils. It does not use rustc.

```
sudo apt-get install gcc-aarch64-linux-gnu
cd pi4-dns-kernel
./scripts/build-pi4-blinktest.sh
```

That writes:

- `boot/pi4-blinktest/kernel8.img`
- `boot/pi4-blinktest/config.txt`

`config.txt` is a copy of `boot/pi4/config.txt`: `arm_64bit=1`,
`kernel=kernel8.img`, `kernel_address=0x80000`, `os_check=0`. It does not
set `arm_peri_high`.

## Copy to the card

Mount the boot FAT partition of the card the Pi 4 actually reads. From
the repo:

```
cp boot/pi4-blinktest/kernel8.img /path/to/boot/
cp boot/pi4-blinktest/config.txt /path/to/boot/
sync
```

Both files replace the ones already on that partition. Eject the card,
put it in the Pi 4, and power on. The rainbow should become a solid
color within a second of the firmware's solid green LED going out.

To put the DNS kernel back, build with `./scripts/build-pi4.sh` and copy
`boot/pi4/kernel8.img` and `boot/pi4/config.txt` the same way.
