#!/bin/bash
set -e

KERNEL="boot/kernel8.img"

if [ ! -f "$KERNEL" ]; then
    echo "Kernel not found. Run ./scripts/build.sh first."
    exit 1
fi

echo "=== Running kernel in QEMU ==="
echo "Using 'virt' machine with virtio-net for network testing"
echo "Press Ctrl-A X to exit QEMU"
echo ""

# Use virt machine with virtio-net and user networking
# Port forwarding: host:5353 -> guest:53 (DNS), host:8080 -> guest:80 (HTTP)
qemu-system-aarch64 \
    -M virt \
    -cpu cortex-a72 \
    -m 512M \
    -kernel "$KERNEL" \
    -serial stdio \
    -nographic \
    -display none \
    -netdev user,id=net0,hostfwd=udp::5353-:53,hostfwd=tcp::8080-:80 \
    -device virtio-net-device,netdev=net0
