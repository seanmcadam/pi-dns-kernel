#!/bin/bash
# Launch the DNS kernel in QEMU for local manual testing.
#
# Usage: ./run-local.sh [--no-build] [--fresh-disk] [--no-disk]
#   --no-build    skip ./scripts/build.sh and use the existing boot/kernel8.img
#   --fresh-disk  recreate the FAT32 disk image (wipes saved config and logs)
#   --no-disk     run without a disk (nothing is persisted)
#
# Environment overrides:
#   DNS_HOST_PORT  (default 15353)  host UDP port forwarded to guest DNS port 53
#   HTTP_HOST_PORT (default 8080)   host TCP port forwarded to guest HTTP port 80
#   DISK_IMG       (default qemu/manual-disk.img)
#
# Exit QEMU with Ctrl-A then X.
set -euo pipefail

cd "$(dirname "$0")"

BUILD=1
FRESH_DISK=0
USE_DISK=1
for arg in "$@"; do
    case "$arg" in
        --no-build)   BUILD=0 ;;
        --fresh-disk) FRESH_DISK=1 ;;
        --no-disk)    USE_DISK=0 ;;
        -h|--help)    sed -n '2,15p' "$0"; exit 0 ;;
        *) echo "Unknown option: $arg (try --help)"; exit 1 ;;
    esac
done

DNS_HOST_PORT="${DNS_HOST_PORT:-15353}"
HTTP_HOST_PORT="${HTTP_HOST_PORT:-8080}"
DISK_IMG="${DISK_IMG:-qemu/manual-disk.img}"
KERNEL="boot/kernel8.img"

command -v qemu-system-aarch64 >/dev/null || {
    echo "qemu-system-aarch64 not found. Install it with: sudo apt install qemu-system-arm"
    exit 1
}

if [ "$BUILD" -eq 1 ]; then
    ./scripts/build.sh
fi
[ -f "$KERNEL" ] || { echo "$KERNEL not found. Run ./scripts/build.sh first."; exit 1; }

DISK_ARGS=()
if [ "$USE_DISK" -eq 1 ]; then
    mkdir -p "$(dirname "$DISK_IMG")"
    if [ "$FRESH_DISK" -eq 1 ] || [ ! -f "$DISK_IMG" ]; then
        for tool in sfdisk mkfs.vfat; do
            command -v "$tool" >/dev/null || { echo "$tool not found (needs util-linux and dosfstools)."; exit 1; }
        done
        echo "Creating 64 MB FAT32 disk at $DISK_IMG"
        rm -f "$DISK_IMG"
        dd if=/dev/zero of="$DISK_IMG" bs=1M count=64 status=none
        sfdisk --quiet "$DISK_IMG" <<'PART'
label: dos
start=2048, type=c
PART
        mkfs.vfat -F 32 -n PI4DNS --offset 2048 "$DISK_IMG" >/dev/null
    fi
    DISK_ARGS=(
        -drive "file=$DISK_IMG,if=none,format=raw,id=hd0,cache=writethrough"
        -device virtio-blk-device,drive=hd0
    )
fi

echo ""
echo "=== pi4-dns-kernel in QEMU ==="
echo "DNS:  dig @127.0.0.1 -p ${DNS_HOST_PORT} example.com"
echo "Web:  http://127.0.0.1:${HTTP_HOST_PORT}/"
if [ "$USE_DISK" -eq 1 ]; then
    echo "Disk: $DISK_IMG (inspect with: mdir -i ${DISK_IMG}@@1048576 ::)"
else
    echo "Disk: none, nothing will be saved"
fi
echo "Exit: Ctrl-A then X"
echo ""

exec qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -smp 4 \
    -m 512M \
    -kernel "$KERNEL" \
    -nographic \
    -serial mon:stdio \
    -netdev "user,id=net0,hostfwd=udp::${DNS_HOST_PORT}-:53,hostfwd=tcp::${HTTP_HOST_PORT}-:80" \
    -device virtio-net-device,netdev=net0 \
    "${DISK_ARGS[@]}"
