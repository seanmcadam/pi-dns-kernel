#!/bin/bash
set -e

KERNEL="boot/kernel8.img"

if [ ! -f "$KERNEL" ]; then
    echo "Kernel not found. Run ./scripts/build.sh first."
    exit 1
fi

echo "=== Running kernel in QEMU ==="
echo "Press Ctrl-A X to exit QEMU"
echo ""

# Note: raspi4b model in QEMU has limited support
# Using raspi3b which has better support for now
# Real Pi4 hardware will use the actual binary

qemu-system-aarch64 \
    -M raspi3b \
    -kernel "$KERNEL" \
    -serial stdio \
    -nographic \
    -display none
