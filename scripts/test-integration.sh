#!/bin/bash
set -e

echo "=== Running Integration Tests ==="

# 5353 is mDNS and is often already taken. Guest ports stay 53 and 80.
DNS_HOST_PORT="${DNS_HOST_PORT:-15353}"
HTTP_HOST_PORT="${HTTP_HOST_PORT:-8080}"
HOSTFWD="hostfwd=udp::${DNS_HOST_PORT}-:53,hostfwd=tcp::${HTTP_HOST_PORT}-:80"
echo "Using host ports DNS ${DNS_HOST_PORT} and HTTP ${HTTP_HOST_PORT}"

# Build kernel first
./scripts/build.sh

# Start QEMU in background
echo "Starting QEMU..."
timeout 120 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -m 512M \
    -kernel boot/kernel8.img \
    -serial mon:tcp:127.0.0.1:4444,server,nowait \
    -nographic \
    -display none \
    -netdev user,id=net0,${HOSTFWD} \
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
   grep -q "HTTP server listening on port 80" qemu/boot-output.txt && \
   grep -q "MMU and caches enabled" qemu/boot-output.txt && \
   grep -q "Entered at EL1" qemu/boot-output.txt && \
   grep -q "Running at EL1" qemu/boot-output.txt; then
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
    if timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short > qemu/dns-test-local.txt 2>&1; then
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
    if timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} pi4.local +short > qemu/dns-pi4-local.txt 2>&1; then
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
    if timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} www.local CNAME +short > qemu/dns-cname.txt 2>&1; then
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
    if timeout 5 curl -s -u admin:admin http://127.0.0.1:${HTTP_HOST_PORT}/status > qemu/http-status.txt 2>&1; then
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
    if timeout 5 curl -s -u admin:admin http://127.0.0.1:${HTTP_HOST_PORT}/ > qemu/http-root.txt 2>&1; then
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
    HTTP_CODE=$(timeout 5 curl -s -o /dev/null -w "%{http_code}" http://127.0.0.1:${HTTP_HOST_PORT}/ 2>&1 || echo "000")
    if [ "$HTTP_CODE" = "401" ]; then
        echo "✓ HTTP authentication required (401 Unauthorized)"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ Expected 401, got HTTP $HTTP_CODE"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Test 8: forward a real external name and serve the second query from cache
echo ""
echo "Test 8: Upstream forward and cache for example.com"
if command -v dig &> /dev/null; then
    if timeout 12 dig @127.0.0.1 -p ${DNS_HOST_PORT} example.com A +time=8 +tries=1 > qemu/dns-example.txt 2>&1; then
        if grep -E 'IN[[:space:]]+A[[:space:]]+[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+' qemu/dns-example.txt >/dev/null; then
            echo "✓ example.com forwarded:"
            grep -E 'IN[[:space:]]+A[[:space:]]+' qemu/dns-example.txt
            timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} example.com A +short +time=3 +tries=1 > qemu/dns-example-cached.txt 2>&1 || true
            if timeout 5 curl -s -u admin:admin http://127.0.0.1:${HTTP_HOST_PORT}/status | grep -Eq '"cache_entries":[1-9]'; then
                echo "✓ Second query can be served from the cache"
                cat qemu/dns-example-cached.txt
                TESTS_PASSED=$((TESTS_PASSED + 1))
            else
                echo "✗ Cache did not record the upstream answer"
                TESTS_FAILED=$((TESTS_FAILED + 1))
            fi
        else
            echo "✗ example.com did not return an A record"
            cat qemu/dns-example.txt
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    else
        echo "✗ example.com query failed"
        cat qemu/dns-example.txt
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Test 15: the virtual timer must advance while this QEMU is still up.
echo ""
echo "Test 15: Virtual timer tick rate"
if timeout 5 curl -s -u admin:admin http://127.0.0.1:${HTTP_HOST_PORT}/status > qemu/status-a.txt; then
    sleep 3
    if timeout 5 curl -s -u admin:admin http://127.0.0.1:${HTTP_HOST_PORT}/status > qemu/status-b.txt \
        && python3 - qemu/status-a.txt qemu/status-b.txt <<'PY'
import json, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
du = b["uptime"] - a["uptime"]
dt = b["ticks"] - a["ticks"]
print(f"uptime {a['uptime']} -> {b['uptime']} (delta {du})")
print(f"ticks {a['ticks']} -> {b['ticks']} (delta {dt}) hz {b['tick_hz']}")
# Host sleep is 3s. Integer uptime can move by 2 or 3, and TCG under load
# can run a bit slow or a bit fast. 100 Hz over that window is a few hundred
# ticks. Bounds stay wide on purpose.
ok = b["tick_hz"] == 100 and b["ticks"] > 0 and 1 <= du <= 6 and 80 <= dt <= 800
sys.exit(0 if ok else 1)
PY
    then
        echo "✓ Uptime and timer ticks advanced at about 100 Hz"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ Timer rate was not about 100 Hz"
        echo "--- first ---"; cat qemu/status-a.txt
        echo "--- second ---"; cat qemu/status-b.txt
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
else
    echo "✗ Could not read /status for the timer test"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Cleanup
echo ""
echo "Stopping QEMU..."
kill $QEMU_PID 2>/dev/null || true
wait $QEMU_PID 2>/dev/null || true

# Test 9: DHCP on user-net, then static fallback with no server
echo ""
echo "Test 9: DHCP client and static fallback"
./scripts/build.sh --features dhcp-default >/tmp/build-dhcp.txt
rm -f qemu/dhcp-serial.txt qemu/dhcp-fallback.txt
timeout 20 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/dhcp-serial.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0 \
    -device virtio-net-device,netdev=net0 \
    >/tmp/qemu-dhcp.txt 2>&1 &
DHCP_PID=$!
sleep 6
kill $DHCP_PID 2>/dev/null || true
wait $DHCP_PID 2>/dev/null || true
if grep -q "DHCP configured 10.0.2.15" qemu/dhcp-serial.txt; then
    echo "✓ QEMU user-net DHCP leased 10.0.2.15"
    grep "DHCP configured" qemu/dhcp-serial.txt
    timeout 20 qemu-system-aarch64 \
        -M virt,gic-version=2 -cpu cortex-a72 -m 512M \
        -kernel boot/kernel8.img \
        -serial file:qemu/dhcp-fallback.txt \
        -display none -monitor none -nographic \
        -netdev socket,id=net0,mcast=230.0.0.1:1234 \
        -device virtio-net-device,netdev=net0 \
        >/tmp/qemu-dhcp-fallback.txt 2>&1 &
    FALLBACK_PID=$!
    sleep 6
    kill $FALLBACK_PID 2>/dev/null || true
    wait $FALLBACK_PID 2>/dev/null || true
    if grep -q "DHCP timeout, using static 10.0.2.15" qemu/dhcp-fallback.txt; then
        echo "✓ DHCP timeout fell back to static 10.0.2.15"
        grep "DHCP timeout" qemu/dhcp-fallback.txt
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ DHCP fallback did not apply the static address"
        cat qemu/dhcp-fallback.txt
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
else
    echo "✗ DHCP did not configure 10.0.2.15"
    cat qemu/dhcp-serial.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Test 10: records added in the web UI survive a reboot
echo ""
echo "Test 10: FAT32 record persistence across reboot"
./scripts/build.sh >/tmp/build-static.txt
rm -f qemu/disk.img
dd if=/dev/zero of=qemu/disk.img bs=1M count=64 status=none
sfdisk --quiet qemu/disk.img <<'EOF'
label: dos
start=2048, type=c
EOF
mkfs.vfat -F 32 -n PI4DNS --offset 2048 qemu/disk.img >/dev/null
DISK_OFF=$((2048 * 512))
rm -f qemu/persist-a.txt qemu/persist-b.txt
timeout 40 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/persist-a.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-persist-a.txt 2>&1 &
PERSIST_PID=$!
sleep 5
curl -s -u admin:admin --max-time 5 -X POST \
    --data 'name=added.local&type=A&value=9.9.9.9&ttl=120' \
    http://127.0.0.1:${HTTP_HOST_PORT}/records > qemu/record-add.txt || true
# A second save must also succeed. One leaked volume slot makes this
# TooManyOpenVolumes even when the first response looked fine.
curl -s -u admin:admin --max-time 5 -X POST \
    --data 'name=tmp-again.local&type=A&value=1.2.3.9&ttl=60' \
    http://127.0.0.1:${HTTP_HOST_PORT}/records > qemu/record-add-2.txt || true
for i in 1 2 3 4 5 6 7 8; do
    curl -s -u admin:admin --max-time 5 -X POST \
        --data "name=tmp${i}.local&type=A&value=1.2.3.${i}&ttl=60" \
        http://127.0.0.1:${HTTP_HOST_PORT}/records >/dev/null || true
done
sleep 1
kill $PERSIST_PID 2>/dev/null || true
wait $PERSIST_PID 2>/dev/null || true
sync
if grep -q "TooManyOpenVolumes" qemu/record-add.txt qemu/record-add-2.txt qemu/persist-a.txt; then
    echo "✗ FAT volume slot leaked (TooManyOpenVolumes)"
    echo "--- record-add ---"; cat qemu/record-add.txt
    echo "--- record-add-2 ---"; cat qemu/record-add-2.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
elif ! grep -q "Record added: added.local" qemu/record-add.txt; then
    echo "✗ Web UI did not add added.local"
    cat qemu/record-add.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
elif ! grep -q "Record added: tmp-again.local" qemu/record-add-2.txt; then
    echo "✗ Second save failed"
    cat qemu/record-add-2.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
elif ! mcopy -i qemu/disk.img@@${DISK_OFF} ::CONFIG.TXT - | grep -q "added.local A 9.9.9.9"; then
    echo "✗ CONFIG.TXT does not contain the new record"
    TESTS_FAILED=$((TESTS_FAILED + 1))
elif ! mcopy -i qemu/disk.img@@${DISK_OFF} ::LOG.OLD - | grep -q "record added"; then
    echo "✗ LOG.OLD does not contain a rotated log line"
    TESTS_FAILED=$((TESTS_FAILED + 1))
else
    timeout 30 qemu-system-aarch64 \
        -M virt,gic-version=2 -cpu cortex-a72 -m 512M \
        -kernel boot/kernel8.img \
        -serial file:qemu/persist-b.txt \
        -display none -monitor none -nographic \
        -netdev user,id=net0,${HOSTFWD} \
        -device virtio-net-device,netdev=net0 \
        -drive file=qemu/disk.img,if=none,format=raw,id=hd0,cache=writethrough \
        -device virtio-blk-device,drive=hd0 \
        >/tmp/qemu-persist-b.txt 2>&1 &
    PERSIST_PID=$!
    sleep 5
    REBOOT_RESULT=$(timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} added.local +short +time=3 +tries=1 || true)
    STILL_THERE=$(timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short +time=3 +tries=1 || true)
    echo "added.local after reboot: $REBOOT_RESULT"
    if echo "$REBOOT_RESULT" | grep -q "9.9.9.9" && echo "$STILL_THERE" | grep -q "192.168.1.100" \
        && grep -q "Loaded config from filesystem" qemu/persist-b.txt; then
        echo "✓ added.local survived reboot"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ Record did not survive reboot"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
    kill $PERSIST_PID 2>/dev/null || true
    wait $PERSIST_PID 2>/dev/null || true
fi

# Test 12 and 13: exception report, and a write to .text must fault.
fault_case() {
    local name="$1"
    local path="$2"
    local marker="$3"
    local iss="$4"
    local serial="qemu/fault-${name}.txt"
    rm -f "$serial"
    timeout 30 qemu-system-aarch64 \
        -M virt,gic-version=2 -cpu cortex-a72 -m 512M \
        -kernel boot/kernel8.img \
        -serial "file:${serial}" \
        -display none -monitor none -nographic \
        -netdev user,id=net0,${HOSTFWD} \
        -device virtio-net-device,netdev=net0 \
        >/tmp/qemu-fault-${name}.txt 2>&1 &
    local pid=$!
    sleep 5
    # The endpoint is behind the same basic auth as the dashboard.
    local unauth
    unauth=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 \
        "http://127.0.0.1:${HTTP_HOST_PORT}${path}" || true)
    if [ "$unauth" != "401" ]; then
        echo "✗ ${name} fault endpoint without auth returned ${unauth}"
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        TESTS_FAILED=$((TESTS_FAILED + 1))
        return
    fi
    curl -s -u admin:admin --max-time 5 \
        "http://127.0.0.1:${HTTP_HOST_PORT}${path}" >/dev/null || true
    sleep 1
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    if grep -q "=== EXCEPTION ===" "$serial" \
        && grep -q "ESR_EL1:" "$serial" \
        && grep -q "ELR_EL1:" "$serial" \
        && grep -q "FAR_EL1:" "$serial" \
        && grep -q "SPSR_EL1:" "$serial" \
        && grep -q "x0:" "$serial" \
        && grep -q "x30:" "$serial" \
        && grep -q "halting" "$serial" \
        && grep -q "$marker" "$serial" \
        && grep -q "$iss" "$serial"; then
        echo "✓ Fault report for ${name}"
        grep -E "type:|ESR_EL1:|ISS:|FAR_EL1:|ELR_EL1:" "$serial"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ Fault report for ${name} was incomplete"
        cat "$serial"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
}

echo ""
echo "Test 12: Data abort on the stack guard page"
fault_case guard /debug/fault/guard "debug fault: read stack guard" "Translation fault"

echo ""
echo "Test 13: Write to kernel text is a permission fault"
fault_case text /debug/fault/text "debug fault: write kernel text" "Permission fault"

echo ""
echo "Test 16: EL0 round trip, IRQ, and fault isolation"
rm -f qemu/el0-serial.txt
timeout 30 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/el0-serial.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    >/tmp/qemu-el0.txt 2>&1 &
EL0_PID=$!
sleep 8
EL0_DNS=$(timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short +time=3 +tries=1 || true)
kill $EL0_PID 2>/dev/null || true
wait $EL0_PID 2>/dev/null || true
EL0_LINE=$(grep "EL0 test:" qemu/el0-serial.txt | tail -1 || true)
echo "$EL0_LINE"
if echo "$EL0_LINE" | grep -q "log ok, yield ok, irq ok, exit 0, bad-syscall -1, bad-pointer rejected, kernel-read killed, priv killed, exec-data killed" \
    && grep -q "user: hello from el0" qemu/el0-serial.txt \
    && grep -q "user: ticks advanced" qemu/el0-serial.txt \
    && grep -q "EL0 yield: back in kernel" qemu/el0-serial.txt \
    && grep -q "EL0 log: rejected pointer" qemu/el0-serial.txt \
    && grep -q "EL0 syscall: unknown" qemu/el0-serial.txt \
    && grep -q "user: bad syscall survived" qemu/el0-serial.txt \
    && grep -q "user: bad pointer survived" qemu/el0-serial.txt \
    && grep -q "DNS Server ready" qemu/el0-serial.txt \
    && echo "$EL0_DNS" | grep -q "192.168.1.100"; then
    echo "✓ EL0 round trip, IRQ resume, bad syscall, bad pointer, and user faults"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ EL0 tests did not all pass"
    echo "dig: $EL0_DNS"
    cat qemu/el0-serial.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 14: Boot from EL2 (QEMU virtualization=on)"
rm -f qemu/el2-serial.txt
timeout 25 qemu-system-aarch64 \
    -M virt,virtualization=on,gic-version=2 \
    -cpu cortex-a72 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/el2-serial.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    >/tmp/qemu-el2.txt 2>&1 &
EL2_PID=$!
sleep 6
EL2_DNS=$(timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short +time=3 +tries=1 || true)
kill $EL2_PID 2>/dev/null || true
wait $EL2_PID 2>/dev/null || true
if grep -q "Entered at EL2" qemu/el2-serial.txt \
    && grep -q "Running at EL1" qemu/el2-serial.txt \
    && grep -q "DNS Server ready" qemu/el2-serial.txt \
    && echo "$EL2_DNS" | grep -q "192.168.1.100"; then
    echo "✓ EL2 entry dropped to EL1 and answered test.local"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ EL2 boot path failed"
    echo "dig: $EL2_DNS"
    cat qemu/el2-serial.txt
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 11: Pi 4 image (compile and SD card files only)"
./scripts/build-pi4.sh >/tmp/build-pi4.txt
START_ADDR=$(aarch64-linux-gnu-nm target/aarch64-unknown-none/release/kernel8 | awk '/ _start$/ {print $1}')
if [ -f boot/pi4/kernel8.img ] && [ -f boot/pi4/config.txt ] \
    && echo "$START_ADDR" | grep -Eq '^0*ffff000000080000$' \
    && grep -q 'kernel=kernel8.img' boot/pi4/config.txt \
    && grep -q 'arm_64bit=1' boot/pi4/config.txt; then
    echo "✓ Pi 4 kernel8.img linked at 0x${START_ADDR} with config.txt"
    echo "  This image has not been tested on hardware."
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Pi 4 image is incomplete (_start=${START_ADDR})"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
# Leave the QEMU image in boot/ for the virt machine.
./scripts/build.sh >/tmp/build-qemu-final.txt

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
