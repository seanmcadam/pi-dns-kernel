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

# Use virt machine with virtio-net and user networking.
# 5353 is mDNS and is often already taken (avahi, Spotify, Brave).
# Guest DNS stays on 53 and HTTP stays on 80.
DNS_HOST_PORT="${DNS_HOST_PORT:-15353}"
HTTP_HOST_PORT="${HTTP_HOST_PORT:-8080}"
TELNET_HOST_PORT="${TELNET_HOST_PORT:-2323}"
VNC_HOST_PORT="${VNC_HOST_PORT:-15900}"
echo "Host DNS port ${DNS_HOST_PORT} -> guest 53, host HTTP port ${HTTP_HOST_PORT} -> guest 80, host telnet port ${TELNET_HOST_PORT} -> guest 23, host VNC port ${VNC_HOST_PORT} -> guest 5900"

qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -smp 4 \
    -m 512M \
    -kernel "$KERNEL" \
    -serial stdio \
    -nographic \
    -display none \
    -netdev user,id=net0,hostfwd=udp::${DNS_HOST_PORT}-:53,hostfwd=tcp::${HTTP_HOST_PORT}-:80,hostfwd=tcp::${TELNET_HOST_PORT}-:23,hostfwd=tcp::${VNC_HOST_PORT}-:5900 \
    -device virtio-net-device,netdev=net0
