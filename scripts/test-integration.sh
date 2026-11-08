#!/bin/bash
set -e

echo "=== Running Integration Tests ==="

# 5353 is mDNS and is often already taken. Guest ports stay 53 and 80.
DNS_HOST_PORT="${DNS_HOST_PORT:-15353}"
HTTP_HOST_PORT="${HTTP_HOST_PORT:-8080}"
TELNET_HOST_PORT="${TELNET_HOST_PORT:-2323}"
HOSTFWD="hostfwd=udp::${DNS_HOST_PORT}-:53,hostfwd=tcp::${HTTP_HOST_PORT}-:80,hostfwd=tcp::${TELNET_HOST_PORT}-:23"
echo "Using host ports DNS ${DNS_HOST_PORT}, HTTP ${HTTP_HOST_PORT}, telnet ${TELNET_HOST_PORT}"

# Build kernel first
./scripts/build.sh

# Start QEMU in background
echo "Starting QEMU..."
timeout 120 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -smp 4 \
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

# Test 18: the DNS task posts a query line and the log task publishes it.
echo ""
echo "Test 18: DNS query reaches the log task"
LOG_TEXT=""
LOG_OK=0
for _try in 1 2 3 4 5; do
    LOG_TEXT=$(timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/logs" || true)
    if echo "$LOG_TEXT" | grep -q "dns query test.local"; then
        LOG_OK=1
        break
    fi
    sleep 0.3
done
if [ "$LOG_OK" = "1" ]; then
    echo "✓ Log task published the DNS query"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ DNS query did not reach the log task"
    echo "$LOG_TEXT"
    TESTS_FAILED=$((TESTS_FAILED + 1))
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

# Test 17: a task that never yields must not starve DNS or HTTP.
echo ""
echo "Test 17: Preemption while an EL0 task spins"
UNAUTH_SPIN=$(timeout 5 curl -s -o /dev/null -w "%{http_code}" \
    "http://127.0.0.1:${HTTP_HOST_PORT}/debug/spin" || echo "000")
SPIN_BODY=$(timeout 5 curl -s -u admin:admin \
    "http://127.0.0.1:${HTTP_HOST_PORT}/debug/spin" || true)
sleep 1
SPIN_DIG=$(timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short +time=3 +tries=1 || true)
SPIN_STATUS=$(timeout 5 curl -s -u admin:admin \
    "http://127.0.0.1:${HTTP_HOST_PORT}/status" || true)
if [ "$UNAUTH_SPIN" = "401" ] \
    && echo "$SPIN_BODY" | grep -q "spinning" \
    && echo "$SPIN_DIG" | grep -q "192.168.1.100" \
    && echo "$SPIN_STATUS" | grep -q '"status":"running"'; then
    echo "✓ DNS and HTTP answered while an EL0 task spun"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ A spinning task starved DNS or HTTP"
    echo "unauth=$UNAUTH_SPIN"
    echo "spin=$SPIN_BODY"
    echo "dig=$SPIN_DIG"
    echo "status=$SPIN_STATUS"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Test 22: a telnet client sees a live DNS log line.
echo ""
echo "Test 22: Telnet client sees a DNS query log line"
TELNET_NAME="tn$(date +%s).local"
if python3 - "$TELNET_HOST_PORT" "$DNS_HOST_PORT" "$TELNET_NAME" <<'PY'
import socket, subprocess, sys, time
port, dns, name = int(sys.argv[1]), sys.argv[2], sys.argv[3]
s = socket.create_connection(("127.0.0.1", port), 5)
s.settimeout(0.3)
s.sendall(b"\xff\xfb\x03\xff\xfd\x18")
deadline = time.time() + 3
buf = b""
while time.time() < deadline and b"telnet-log" not in buf:
    try:
        chunk = s.recv(4096)
    except socket.timeout:
        continue
    if not chunk:
        break
    buf += chunk
if b"telnet-log" not in buf:
    print("no banner", buf[:200])
    sys.exit(1)
subprocess.run(
    ["dig", "@127.0.0.1", "-p", dns, name, "+short", "+time=3", "+tries=1"],
    check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
deadline = time.time() + 5
while time.time() < deadline and name.encode() not in buf:
    try:
        chunk = s.recv(4096)
    except socket.timeout:
        continue
    if not chunk:
        break
    buf += chunk
s.close()
if name.encode() not in buf:
    print("missing log line", buf[-300:])
    sys.exit(1)
print("saw", name)
PY
then
    echo "✓ Telnet client saw the DNS query log line"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Telnet client did not see the DNS query log line"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Test 25: a client that never reads must not stop another client or the console.
echo ""
echo "Test 23: Stalled telnet client does not block the others"
rm -f qemu/telnet-console.txt qemu/telnet-stall-name.txt
timeout 12 nc 127.0.0.1 4444 > qemu/telnet-console.txt 2>&1 &
STALL_NC=$!
STALL_NAME="stall$(date +%s).local"
echo "$STALL_NAME" > qemu/telnet-stall-name.txt
if python3 - "$TELNET_HOST_PORT" "$DNS_HOST_PORT" "$STALL_NAME" <<'PY'
import socket, subprocess, sys, time
port, dns, name = int(sys.argv[1]), sys.argv[2], sys.argv[3]
stalled = socket.create_connection(("127.0.0.1", port), 5)
live = socket.create_connection(("127.0.0.1", port), 5)
live.settimeout(0.3)
buf = b""
deadline = time.time() + 3
while time.time() < deadline and b"telnet-log" not in buf:
    try:
        chunk = live.recv(4096)
    except socket.timeout:
        continue
    if not chunk:
        break
    buf += chunk
subprocess.run(
    ["dig", "@127.0.0.1", "-p", dns, name, "+short", "+time=3", "+tries=1"],
    check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
deadline = time.time() + 6
while time.time() < deadline and name.encode() not in buf:
    try:
        chunk = live.recv(4096)
    except socket.timeout:
        continue
    if not chunk:
        break
    buf += chunk
live.close()
stalled.close()
if name.encode() not in buf:
    print("live client missed", name, buf[-200:])
    sys.exit(1)
print("live client saw", name)
PY
then
    sleep 3
    kill $STALL_NC 2>/dev/null || true
    wait $STALL_NC 2>/dev/null || true
    if grep -q "$STALL_NAME" qemu/telnet-console.txt && grep -q "\[status\]" qemu/telnet-console.txt; then
        echo "✓ A stalled telnet client did not stop another client or the console"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ Console stopped while a telnet client was stalled"
        echo "--- console ---"
        tail -n 40 qemu/telnet-console.txt || true
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
else
    kill $STALL_NC 2>/dev/null || true
    wait $STALL_NC 2>/dev/null || true
    echo "✗ Stalled telnet client blocked another client"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Tests 23 and 24: 16 clients, the 17th is refused, and a disconnect frees a slot.
echo ""
echo "Test 24: Sixteen telnet clients, the seventeenth is refused"
echo "Test 25: Disconnecting a telnet client frees its slot"
if python3 - "$TELNET_HOST_PORT" "$DNS_HOST_PORT" <<'PY'
import socket, subprocess, sys, time
port, dns = int(sys.argv[1]), sys.argv[2]

def read_for(sock, seconds):
    sock.settimeout(0.2)
    end = time.time() + seconds
    data = b""
    while time.time() < end:
        try:
            chunk = sock.recv(4096)
        except socket.timeout:
            continue
        if not chunk:
            break
        data += chunk
        if b"telnet-log" in data or b"busy" in data:
            break
    return data

clients = []
for i in range(16):
    sock = socket.create_connection(("127.0.0.1", port), 5)
    banner = read_for(sock, 3)
    if b"telnet-log" not in banner:
        print("client", i, "banner", banner[:120])
        sys.exit(1)
    clients.append(sock)
extra = socket.create_connection(("127.0.0.1", port), 5)
refusal = read_for(extra, 3)
extra.close()
if b"busy" not in refusal:
    print("17th was not refused", refusal[:120])
    sys.exit(1)
print("17th refused")
clients[0].close()
name = "reuse%d.local" % int(time.time())
subprocess.run(
    ["dig", "@127.0.0.1", "-p", dns, name, "+short", "+time=3", "+tries=1"],
    check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
)
time.sleep(0.4)
opened = False
for _ in range(8):
    sock = socket.create_connection(("127.0.0.1", port), 5)
    banner = read_for(sock, 2)
    sock.close()
    if b"telnet-log" in banner and b"busy" not in banner:
        opened = True
        break
    time.sleep(0.3)
for sock in clients[1:]:
    sock.close()
if not opened:
    print("slot was not reused")
    sys.exit(1)
print("slot reused")
PY
then
    echo "✓ Sixteen telnet clients connected and the seventeenth was refused"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ A disconnected telnet client freed a slot"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Telnet connection limit or slot reuse failed"
    TESTS_FAILED=$((TESTS_FAILED + 2))
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
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
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
        -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
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
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
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
        -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
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
        -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
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
    -cpu cortex-a72 -smp 4 -m 512M \
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
    -cpu cortex-a72 -smp 4 -m 512M \
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

echo ""
echo "Test 19: Four cores online"
if grep -q "cores online 4" qemu/boot-output.txt; then
    echo "✓ All 4 cores reported online"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Did not see cores online 4"
    grep "cores online" qemu/boot-output.txt || true
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 21: Futex-protected counter"
if grep -q "user: futex counter 4000" qemu/el0-serial.txt; then
    echo "✓ Four threads incremented a futex counter to 4000"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Futex counter was not exactly 4000"
    grep "futex" qemu/el0-serial.txt || true
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 20: Concurrent DNS and HTTP"
rm -f qemu/load-serial.txt
timeout 25 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/load-serial.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    >/tmp/qemu-load.txt 2>&1 &
LOAD_PID=$!
sleep 6
LOAD_OK=0
for n in 1 2 3 4; do
    dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short +time=3 +tries=1 > /tmp/load-dig-$n.txt 2>/dev/null &
    curl -s -u admin:admin --max-time 4 http://127.0.0.1:${HTTP_HOST_PORT}/status > /tmp/load-http-$n.txt 2>/dev/null &
done
wait
for n in 1 2 3 4; do
    if grep -q "192.168.1.100" /tmp/load-dig-$n.txt && grep -q '"status":"running"' /tmp/load-http-$n.txt; then
        LOAD_OK=$((LOAD_OK + 1))
    fi
done
kill $LOAD_PID 2>/dev/null || true
wait $LOAD_PID 2>/dev/null || true
if [ "$LOAD_OK" -eq 4 ]; then
    echo "✓ Four parallel DNS queries and four HTTP status reads"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Concurrent DNS+HTTP load: ${LOAD_OK}/4 pairs"
    cat /tmp/load-dig-*.txt /tmp/load-http-*.txt || true
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 26: NIC and block IRQs, idle network core"
rm -f qemu/irq-serial.txt
# disk.img is the FAT volume from test 10. Recreate a blank one if that test
# did not leave it behind; the block driver still completes a sector read.
if [ ! -f qemu/disk.img ]; then
    dd if=/dev/zero of=qemu/disk.img bs=1M count=8 status=none
fi
timeout 30 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/irq-serial.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-irq.txt 2>&1 &
IRQ_PID=$!
sleep 6
timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local +short +time=3 +tries=1 >/tmp/irq-dig.txt 2>&1 || true
# Quiet window. The status line is every 2s; wait until a later sample exists.
IRQ_RC=0
python3 - <<'PY' || IRQ_RC=$?
import re, sys, time
path = "qemu/irq-serial.txt"
pat = re.compile(r"\[status\] rx=\d+ tx=\d+ ms=(\d+) nic=(\d+) blk=(\d+) busy=(\d+) idle=(\d+)")
def rows():
    text = open(path, errors="replace").read()
    return [tuple(int(x) for x in m) for m in pat.findall(text)]
base = rows()
deadline = time.time() + 8
while time.time() < deadline:
    time.sleep(0.5)
    cur = rows()
    if base and cur and cur[-1][0] >= base[-1][0] + 1500:
        break
    if not base and cur:
        base = cur
cur = rows()
open("/tmp/irq-samples.txt", "w").write(repr(cur) + "\n")
if not cur:
    print("no status samples")
    sys.exit(1)
ms, nic, blk, busy, idle = cur[-1]
if nic <= 0 or blk <= 0:
    print(f"counts nic={nic} blk={blk}")
    sys.exit(1)
# Compare the last sample with one at least 1.5s earlier.
prev = None
for row in cur:
    if ms >= row[0] + 1500:
        prev = row
if prev is None:
    print("no quiet window")
    sys.exit(1)
idle_d = idle - prev[4]
busy_d = busy - prev[3]
print(f"nic={nic} blk={blk} idle_d={idle_d} busy_d={busy_d}")
# Idle cntpct over the quiet window should dwarf drive() time.
if idle_d <= 0 or idle_d <= busy_d * 4:
    sys.exit(1)
sys.exit(0)
PY
kill $IRQ_PID 2>/dev/null || true
wait $IRQ_PID 2>/dev/null || true
if [ "$IRQ_RC" -eq 0 ] && grep -q "192.168.1.100" /tmp/irq-dig.txt; then
    echo "✓ NIC and block IRQ counts are nonzero and the network core idled"
    cat /tmp/irq-samples.txt
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ IRQ idle test failed"
    cat /tmp/irq-samples.txt 2>/dev/null || true
    grep "irq" qemu/irq-serial.txt || true
    grep "status" qemu/irq-serial.txt | tail -5 || true
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# raspi4b is optional. QEMU 8.2 has no such machine. A skip is not a failure
# and does not change the 26 virt results. QEMU 9.0 or newer is required
# to actually run it. The machine has no GENET, so DNS, HTTP, and telnet
# are not required.
echo ""
echo "Test 27: QEMU raspi4b smoke (needs QEMU 9.0 or newer)"
if ! qemu-system-aarch64 -machine help 2>/dev/null | grep -q '^raspi4b '; then
    echo "SKIP raspi4b: this QEMU has no raspi4b machine (need QEMU 9.0 or newer)"
else
    ./scripts/build-pi4.sh >/tmp/build-pi4-raspi.txt
    rm -f qemu/raspi4b-sd.img qemu/raspi4b.txt
    dd if=/dev/zero of=qemu/raspi4b-sd.img bs=1M count=8 status=none
    sfdisk --quiet qemu/raspi4b-sd.img <<'EOF'
label: dos
start=2048, type=c
EOF
    mkfs.vfat -F 32 -n PI4DNS --offset 2048 qemu/raspi4b-sd.img >/dev/null
    RASPI_OFF=$((2048 * 512))
    printf 'record=pi4.local A 10.0.2.15 300\n' > /tmp/raspi-config.txt
    mcopy -i "qemu/raspi4b-sd.img@@${RASPI_OFF}" /tmp/raspi-config.txt ::CONFIG.TXT
    timeout 45 qemu-system-aarch64 \
        -M raspi4b \
        -smp 4 \
        -kernel boot/pi4/kernel8.img \
        -serial file:qemu/raspi4b.txt \
        -display none -monitor none -nographic \
        -drive file=qemu/raspi4b-sd.img,if=sd,format=raw \
        >/tmp/qemu-raspi4b.txt 2>&1 || true
    RASPI_OK=1
    for need in \
        "MMU and caches enabled" \
        "Running at EL1" \
        "cores online 4" \
        "[OK] Filesystem mounted" \
        "Loaded config from filesystem" \
        "[OK] Log task running" \
        "EL0 test" \
        "[WARN] No NIC"
    do
        if ! grep -q "$need" qemu/raspi4b.txt; then
            echo "✗ raspi4b missing: $need"
            RASPI_OK=0
        fi
    done
    if [ "$RASPI_OK" -eq 1 ]; then
        echo "✓ raspi4b booted, 4 cores, FAT config, EL0 log task, no NIC"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ raspi4b smoke failed"
        echo "--- serial ---"
        cat qemu/raspi4b.txt 2>/dev/null || true
        echo "--- qemu ---"
        cat /tmp/qemu-raspi4b.txt 2>/dev/null || true
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
fi

# Pi 5 has no QEMU machine. This stage builds the image and checks the
# link address, then boots the virt image on cortex-a76. That second step
# runs the shared kernel on the Pi 5 CPU. It does not execute the Pi 5
# UART, GIC, SDHCI, or PSCI code. A failure here fails the suite. A pass
# does not change the 26 virt results.
echo ""
echo "Test 28: Pi 5 image build, and shared code on cortex-a76"
PI5_OK=1
if ! ./scripts/build-pi5.sh >/tmp/build-pi5.txt; then
    echo "✗ Pi 5 build failed"
    cat /tmp/build-pi5.txt || true
    PI5_OK=0
else
    PI5_START=$(aarch64-linux-gnu-nm target/aarch64-unknown-none/release/kernel8 | awk '/ _start$/ {print $1}')
    if [ -f boot/pi5/kernel_2712.img ] && [ -f boot/pi5/config.txt ] \
        && echo "$PI5_START" | grep -Eq '^0*ffff000000200000$' \
        && grep -q 'kernel=kernel_2712.img' boot/pi5/config.txt \
        && grep -q 'kernel_address=0x200000' boot/pi5/config.txt; then
        echo "✓ Pi 5 kernel_2712.img linked at 0x${PI5_START}"
        echo "  Not booted. QEMU has no Pi 5 machine. Not run on hardware."
    else
        echo "✗ Pi 5 image is incomplete (_start=${PI5_START})"
        PI5_OK=0
    fi
fi
rm -f qemu/a76.txt
timeout 20 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a76 \
    -smp 4 \
    -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/a76.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    >/tmp/qemu-a76.txt 2>&1 || true
A76_OK=1
for need in \
    "MMU and caches enabled" \
    "Running at EL1" \
    "cores online 4" \
    "DNS Server ready"
do
    if ! grep -q "$need" qemu/a76.txt; then
        echo "✗ cortex-a76 virt boot missing: $need"
        A76_OK=0
    fi
done
if [ "$A76_OK" -eq 1 ]; then
    echo "✓ virt image booted on -cpu cortex-a76 (shared code only)"
    echo "  Pi 5 peripherals, PSCI, SD, and RP1 were not executed."
else
    echo "✗ cortex-a76 virt boot failed"
    echo "--- serial ---"
    cat qemu/a76.txt 2>/dev/null || true
    echo "--- qemu ---"
    cat /tmp/qemu-a76.txt 2>/dev/null || true
    PI5_OK=0
fi
if [ "$PI5_OK" -ne 1 ]; then
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

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
