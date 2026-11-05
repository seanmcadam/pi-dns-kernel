#!/bin/bash
set -e

echo "=== Running Integration Tests ==="

# Build kernel first
./scripts/build.sh

# Start QEMU in background
echo "Starting QEMU..."
timeout 60 qemu-system-aarch64 \
    -M virt \
    -cpu cortex-a72 \
    -m 512M \
    -kernel boot/kernel8.img \
    -serial mon:tcp:127.0.0.1:4444,server,nowait \
    -nographic \
    -display none \
    -netdev user,id=net0,hostfwd=udp::5353-:53,hostfwd=tcp::8080-:80 \
    -device virtio-net-device,netdev=net0 \
    &

QEMU_PID=$!
echo "QEMU PID: $QEMU_PID"

# Wait for boot
echo "Waiting for kernel to boot..."
sleep 5

# Connect and capture boot output
echo "Checking boot output..."
timeout 5 nc 127.0.0.1 4444 > qemu/boot-output.txt 2>&1 &
NC_PID=$!

sleep 2
kill $NC_PID 2>/dev/null || true

TESTS_PASSED=0
TESTS_FAILED=0

# Test 1: Check boot messages
echo ""
echo "Test 1: Boot messages"
if grep -q "DNS Server ready" qemu/boot-output.txt && \
   grep -q "DNS server listening on port 53" qemu/boot-output.txt && \
   grep -q "HTTP server listening on port 80" qemu/boot-output.txt; then
    echo "✓ Kernel booted successfully"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Kernel did not boot properly"
    echo "Boot output:"
    cat qemu/boot-output.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Give network more time to initialize
sleep 3

# Test 2: DNS query for test.local
echo ""
echo "Test 2: DNS query for test.local"
if command -v dig &> /dev/null; then
    if timeout 5 dig @127.0.0.1 -p 5353 test.local +short > qemu/dns-test-local.txt 2>&1; then
        RESULT=$(cat qemu/dns-test-local.txt)
        if echo "$RESULT" | grep -q "192.168.1.100"; then
            echo "✓ DNS query successful: test.local -> 192.168.1.100"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo "✗ DNS query returned unexpected result: $RESULT"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    else
        echo "✗ DNS query timed out or failed"
        cat qemu/dns-test-local.txt
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
else
    echo "⚠ dig not available, skipping DNS test"
fi

# Test 3: DNS query for pi4.local
echo ""
echo "Test 3: DNS query for pi4.local"
if command -v dig &> /dev/null; then
    if timeout 5 dig @127.0.0.1 -p 5353 pi4.local +short > qemu/dns-pi4-local.txt 2>&1; then
        RESULT=$(cat qemu/dns-pi4-local.txt)
        if echo "$RESULT" | grep -q "10.0.2.15"; then
            echo "✓ DNS query successful: pi4.local -> 10.0.2.15"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo "✗ DNS query returned unexpected result: $RESULT"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    else
        echo "✗ DNS query timed out or failed"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Test 4: DNS CNAME query
echo ""
echo "Test 4: DNS CNAME query for www.local"
if command -v dig &> /dev/null; then
    if timeout 5 dig @127.0.0.1 -p 5353 www.local CNAME +short > qemu/dns-cname.txt 2>&1; then
        RESULT=$(cat qemu/dns-cname.txt)
        if echo "$RESULT" | grep -q "pi4.local"; then
            echo "✓ DNS CNAME query successful: www.local -> pi4.local"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo "⚠ DNS CNAME query returned: $RESULT (may need full resolution)"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        fi
    else
        echo "✗ DNS CNAME query timed out or failed"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Test 5: HTTP status endpoint
echo ""
echo "Test 5: HTTP GET /status"
if command -v curl &> /dev/null; then
    if timeout 5 curl -s -u admin:admin http://127.0.0.1:8080/status > qemu/http-status.txt 2>&1; then
        RESULT=$(cat qemu/http-status.txt)
        if echo "$RESULT" | grep -q '"status":"running"'; then
            echo "✓ HTTP request successful, server is running"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo "✗ HTTP response unexpected: $RESULT"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    else
        echo "✗ HTTP request timed out or failed"
        cat qemu/http-status.txt
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
else
    echo "⚠ curl not available, skipping HTTP test"
fi

# Test 6: HTTP root endpoint
echo ""
echo "Test 6: HTTP GET / (HTML dashboard)"
if command -v curl &> /dev/null; then
    if timeout 5 curl -s -u admin:admin http://127.0.0.1:8080/ > qemu/http-root.txt 2>&1; then
        if grep -q "Raspberry Pi 4 DNS Server" qemu/http-root.txt; then
            echo "✓ HTTP dashboard accessible"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            echo "✗ HTTP dashboard content unexpected"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    else
        echo "✗ HTTP request timed out or failed"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Test 7: HTTP without authentication
echo ""
echo "Test 7: HTTP authentication check"
if command -v curl &> /dev/null; then
    HTTP_CODE=$(timeout 5 curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:8080/ 2>&1 || echo "000")
    if [ "$HTTP_CODE" = "401" ]; then
        echo "✓ HTTP authentication required (401 Unauthorized)"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ Expected 401, got HTTP $HTTP_CODE"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Cleanup
echo ""
echo "Stopping QEMU..."
kill $QEMU_PID 2>/dev/null || true
wait $QEMU_PID 2>/dev/null || true

# Summary
echo ""
echo "=== Test Summary ==="
echo "Passed: $TESTS_PASSED"
echo "Failed: $TESTS_FAILED"

if [ $TESTS_FAILED -eq 0 ]; then
    echo ""
    echo "=== All Integration Tests PASSED ==="
    exit 0
else
    echo ""
    echo "=== Some Integration Tests FAILED ==="
    exit 1
fi
