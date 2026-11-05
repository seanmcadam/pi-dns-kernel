#!/bin/bash
set -e

echo "=== Running Integration Tests ==="

# Build kernel first
./scripts/build.sh

# Start QEMU in background with telnet monitor
echo "Starting QEMU..."
qemu-system-aarch64 \
    -M raspi3b \
    -kernel boot/kernel8.img \
    -serial mon:tcp:127.0.0.1:4444,server,nowait \
    -nographic \
    -display none \
    &

QEMU_PID=$!
echo "QEMU PID: $QEMU_PID"

# Wait for boot
echo "Waiting for kernel to boot..."
sleep 3

# Connect and check for boot message
echo "Checking boot output..."
timeout 5 nc 127.0.0.1 4444 > qemu/boot-output.txt &
NC_PID=$!

sleep 2
kill $NC_PID 2>/dev/null || true

if grep -q "DNS Server ready" qemu/boot-output.txt; then
    echo "✓ Kernel booted successfully"
    TEST_RESULT=0
else
    echo "✗ Kernel did not boot properly"
    echo "Boot output:"
    cat qemu/boot-output.txt
    TEST_RESULT=1
fi

# Cleanup
echo "Stopping QEMU..."
kill $QEMU_PID 2>/dev/null || true
wait $QEMU_PID 2>/dev/null || true

if [ $TEST_RESULT -eq 0 ]; then
    echo "=== Integration tests PASSED ==="
else
    echo "=== Integration tests FAILED ==="
fi

exit $TEST_RESULT
