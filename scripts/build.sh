#!/bin/bash
set -e

cd "$(dirname "$0")/.."

# `./scripts/build.sh` and `./scripts/build.sh --features dhcp-default`
# build the QEMU virt image. `./scripts/build.sh pi4` builds the Pi 4 image.
# `./scripts/build.sh pi5` builds the Pi 5 image.
BOARD=virt
if [ "${1:-}" = "virt" ] || [ "${1:-}" = "pi4" ] || [ "${1:-}" = "pi5" ]; then
    BOARD="$1"
    shift
fi

if [ "$BOARD" = "pi4" ]; then
    echo "=== Building Raspberry Pi 4 kernel (untested on hardware) ==="
    cargo build --release --target aarch64-unknown-none -p pi4-dns-kernel \
        --no-default-features --features board-pi4 "$@"
    mkdir -p boot/pi4
    aarch64-linux-gnu-objcopy -O binary \
        target/aarch64-unknown-none/release/kernel8 boot/pi4/kernel8.img
    cat > boot/pi4/config.txt <<'EOF'
# Raspberry Pi 4 boot file for this kernel.
# The ELF is linked at 0xFFFF000000080000 (TTBR1). The firmware loads the
# raw kernel8.img at physical 0x80000. This image has not been tested on hardware.
arm_64bit=1
enable_uart=1
uart_2ndstage=1
kernel=kernel8.img
core_freq=250
EOF
    START_ADDR=$(aarch64-linux-gnu-nm target/aarch64-unknown-none/release/kernel8 | awk '/ _start$/ {print $1}')
    echo "_start = 0x${START_ADDR}"
    echo "$START_ADDR" | grep -Eq '^0*ffff000000080000$'
    echo "Kernel size: $(stat -c%s boot/pi4/kernel8.img) bytes"
    echo "SD card files: boot/pi4/kernel8.img and boot/pi4/config.txt"
    echo "This build has not been tested on Raspberry Pi hardware."
    exit 0
fi

if [ "$BOARD" = "pi5" ]; then
    echo "=== Building Raspberry Pi 5 kernel (untested on hardware) ==="
    cargo build --release --target aarch64-unknown-none -p pi4-dns-kernel \
        --no-default-features --features board-pi5 "$@"
    mkdir -p boot/pi5
    aarch64-linux-gnu-objcopy -O binary \
        target/aarch64-unknown-none/release/kernel8 boot/pi5/kernel_2712.img
    cat > boot/pi5/config.txt <<'EOF'
# Raspberry Pi 5 boot file for this kernel.
# The ELF is linked at 0xFFFF000000200000 (TTBR1). The firmware loads the
# raw kernel_2712.img at physical 0x200000. kernel_address asks for that
# load address explicitly. This image has not been tested on hardware.
# QEMU has no Pi 5 machine. RP1 Ethernet is not driven.
arm_64bit=1
kernel=kernel_2712.img
kernel_address=0x200000
EOF
    START_ADDR=$(aarch64-linux-gnu-nm target/aarch64-unknown-none/release/kernel8 | awk '/ _start$/ {print $1}')
    echo "_start = 0x${START_ADDR}"
    echo "$START_ADDR" | grep -Eq '^0*ffff000000200000$'
    echo "Kernel size: $(stat -c%s boot/pi5/kernel_2712.img) bytes"
    echo "SD card files: boot/pi5/kernel_2712.img and boot/pi5/config.txt"
    echo "This build has not been tested on Raspberry Pi hardware."
    exit 0
fi

echo "=== Building virt kernel ==="
cargo build --release --target aarch64-unknown-none -p pi4-dns-kernel "$@"

echo "Creating kernel8.img..."
aarch64-linux-gnu-objcopy -O binary \
    target/aarch64-unknown-none/release/kernel8 boot/kernel8.img

echo "Creating kernel8.asm (disassembly)..."
aarch64-linux-gnu-objdump -d target/aarch64-unknown-none/release/kernel8 > boot/kernel8.asm 2>/dev/null || \
    echo "Warning: Could not create disassembly (objdump not found)"

echo "Kernel size: $(stat -f%z boot/kernel8.img 2>/dev/null || stat -c%s boot/kernel8.img) bytes"
echo "Build complete! kernel8.img is in boot/"
