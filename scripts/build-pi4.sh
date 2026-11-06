#!/bin/bash
set -e

cd "$(dirname "$0")/.."
mkdir -p boot/pi4

echo "=== Building Raspberry Pi 4 kernel (untested on hardware) ==="
cargo build --release --target aarch64-unknown-none -p pi4-dns-kernel --features pi4

aarch64-linux-gnu-objcopy -O binary \
    target/aarch64-unknown-none/release/kernel8 boot/pi4/kernel8.img

cat > boot/pi4/config.txt <<'EOF'
# Raspberry Pi 4 boot file for this kernel.
# kernel8.img is linked at 0x80000. This image has not been tested on hardware.
arm_64bit=1
enable_uart=1
uart_2ndstage=1
kernel=kernel8.img
core_freq=250
EOF

START_ADDR=$(aarch64-linux-gnu-nm target/aarch64-unknown-none/release/kernel8 | awk '/ _start$/ {print $1}')
echo "_start = 0x${START_ADDR}"
echo "$START_ADDR" | grep -Eq '^0*80000$'
echo "Kernel size: $(stat -c%s boot/pi4/kernel8.img) bytes"
echo "SD card files: boot/pi4/kernel8.img and boot/pi4/config.txt"
echo "This build has not been tested on Raspberry Pi hardware."
