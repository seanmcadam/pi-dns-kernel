#!/bin/bash
# Build the standalone Pi 4 diagnostic image. Needs binutils, not rustc:
#   sudo apt-get install gcc-aarch64-linux-gnu
set -euo pipefail
cd "$(dirname "$0")/.."

AS=aarch64-linux-gnu-as
LD=aarch64-linux-gnu-ld
OBJCOPY=aarch64-linux-gnu-objcopy
for tool in "$AS" "$LD" "$OBJCOPY"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing $tool (install gcc-aarch64-linux-gnu)" >&2
        exit 1
    fi
done

mkdir -p boot/pi4-blinktest
"$AS" -o /tmp/blinktest.o boot/pi4-blinktest/blinktest.s
"$LD" -T boot/pi4-blinktest/link.ld --nmagic \
    -o /tmp/blinktest.elf /tmp/blinktest.o
"$OBJCOPY" -O binary /tmp/blinktest.elf boot/pi4-blinktest/kernel8.img
cp boot/pi4/config.txt boot/pi4-blinktest/config.txt

python3 - <<'PY'
import pathlib
img = pathlib.Path("boot/pi4-blinktest/kernel8.img").read_bytes()
if img[0x38:0x3C] != bytes([0x41, 0x52, 0x4D, 0x64]):
    raise SystemExit("kernel8.img is missing the Linux arm64 magic at offset 0x38")
if int.from_bytes(img[0:4], "little") >> 26 != 0x5:
    raise SystemExit("first instruction is not a B")
print(f"blink test image: {len(img)} bytes")
PY
echo "SD card files: boot/pi4-blinktest/kernel8.img and boot/pi4-blinktest/config.txt"
echo "Copy both onto the boot partition. Steps are in boot/pi4-blinktest/README.md"
