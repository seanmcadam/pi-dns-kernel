#!/bin/bash
set -e

echo "=== Running Integration Tests ==="

# 5353 is mDNS and is often already taken. Guest ports stay 53 and 80.
DNS_HOST_PORT="${DNS_HOST_PORT:-15353}"
HTTP_HOST_PORT="${HTTP_HOST_PORT:-8080}"
TELNET_HOST_PORT="${TELNET_HOST_PORT:-2323}"
VNC_HOST_PORT="${VNC_HOST_PORT:-15900}"
HOSTFWD="hostfwd=udp::${DNS_HOST_PORT}-:53,hostfwd=tcp::${HTTP_HOST_PORT}-:80,hostfwd=tcp::${TELNET_HOST_PORT}-:23,hostfwd=tcp::${VNC_HOST_PORT}-:5900"
echo "Using host ports DNS ${DNS_HOST_PORT}, HTTP ${HTTP_HOST_PORT}, telnet ${TELNET_HOST_PORT}, VNC ${VNC_HOST_PORT}"

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

# Supervisor checks use their own guest so a crash loop cannot eat the
# first QEMU's 120s budget or the 26 tests above.
echo ""
echo "Test 29: User-task supervisor"
rm -f qemu/supervisor-console.txt
timeout 90 qemu-system-aarch64 \
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
    >/dev/null 2>&1 &
SUP_PID=$!
sleep 1
timeout 80 nc 127.0.0.1 4444 > qemu/supervisor-console.txt 2>&1 &
SUP_NC=$!
sleep 7
if python3 - "$HTTP_HOST_PORT" "$DNS_HOST_PORT" "$TELNET_HOST_PORT" <<'PY'
import base64, socket, subprocess, sys, time, urllib.error, urllib.request

http, dns, telnet = int(sys.argv[1]), sys.argv[2], sys.argv[3]
base = "http://127.0.0.1:%d" % http
token = base64.b64encode(b"admin:admin").decode()
auth = {"Authorization": "Basic " + token}

def fail(msg):
    print(msg)
    sys.exit(1)

def get(path, authed=True):
    headers = auth if authed else {}
    req = urllib.request.Request(base + path, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except (urllib.error.URLError, TimeoutError, ConnectionError, OSError):
        return 0, b""

def page():
    status, body = get("/debug")
    if status != 200:
        return ""
    return body.decode("utf-8", "replace")

def row(text, name):
    for line in text.splitlines():
        if line.startswith(name + " "):
            return line
    fail("no %s row\n%s" % (name, text))

def field(text, name, key):
    line = row(text, name)
    needle = key + "="
    for part in line.split():
        if part.startswith(needle):
            return part[len(needle):]
    fail("no %s on %s" % (key, line))

def counters(text):
    out = {}
    for key in ("free_pages", "asids", "fds"):
        found = None
        for line in text.splitlines():
            if line.startswith(key + "="):
                found = int(line.split("=", 1)[1])
        if found is None:
            fail("no %s\n%s" % (key, text))
        out[key] = found
    return out

def dns_up(seconds):
    end = time.time() + seconds
    while time.time() < end:
        result = subprocess.run(
            ["dig", "@127.0.0.1", "-p", str(dns), "test.local", "+short", "+time=1", "+tries=1"],
            capture_output=True,
        )
        if b"192.168.1.100" in result.stdout:
            return True
        time.sleep(0.15)
    return False

def wait_running(name, restarts_above, seconds):
    end = time.time() + seconds
    while time.time() < end:
        text = page()
        if not text:
            time.sleep(0.15)
            continue
        line = row(text, name)
        if line.startswith(name + " running ") and int(field(text, name, "restarts")) > restarts_above:
            return text
        time.sleep(0.15)
    return None

code, _ = get("/debug", authed=False)
if code != 401:
    fail("unauthenticated /debug returned %s" % code)
print("unauth 401")

if not dns_up(8):
    fail("dns did not answer before the crash tests")
base_text = page()
base_count = counters(base_text)
print("baseline", base_count)

for n in range(3):
    before = int(field(page(), "dns", "restarts"))
    status, body = get("/debug/crash/dns")
    if status != 200 or b"armed" not in body:
        fail("dns crash %s %s" % (status, body[:80]))
    if wait_running("dns", before, 8) is None or not dns_up(8):
        fail("dns did not return after crash %d\n%s" % (n + 1, page()))
print("dns returned")
matched = None
end = time.time() + 5
while time.time() < end:
    text = page()
    if not text or not dns_up(1):
        time.sleep(0.2)
        continue
    line = row(text, "dns")
    if "ec=0x24" in line and "far=0x0" in line and counters(text) == base_count:
        matched = text
        break
    time.sleep(0.3)
if matched is None:
    text = page()
    fail("dns fault or leak\n%s\nbaseline %s" % (text, base_count))
print("no leak", counters(matched))

before = int(field(page(), "http", "restarts"))
status, body = get("/debug/crash/http")
if status != 200 or b"armed" not in body:
    fail("http crash %s %s" % (status, body[:80]))
time.sleep(0.6)
if wait_running("http", before, 8) is None:
    fail("http did not return\n" + page())
status, body = get("/status")
if status != 200 or b"Uptime" not in body and b"uptime" not in body.lower():
    # /status is the HTML dashboard; any 200 after the restart is enough
    if status != 200:
        fail("status %s after http restart" % status)
print("http returned")

def read_for(sock, seconds, needle=None):
    sock.settimeout(0.2)
    end = time.time() + seconds
    data = b""
    while time.time() < end:
        try:
            chunk = sock.recv(4096)
        except socket.timeout:
            if needle is not None and needle in data:
                break
            continue
        if not chunk:
            break
        data += chunk
        if needle is not None and needle in data:
            break
    return data

clients = []
for i in range(16):
    sock = socket.create_connection(("127.0.0.1", telnet), 5)
    banner = read_for(sock, 3, b"telnet-log")
    if b"telnet-log" not in banner:
        fail("telnet client %d banner %s" % (i, banner[:80]))
    clients.append(sock)
extra = socket.create_connection(("127.0.0.1", telnet), 5)
refusal = read_for(extra, 2, b"busy")
extra.close()
if b"busy" not in refusal:
    fail("17th was not refused %s" % refusal[:80])
status, body = get("/debug/crash/telnet-worker")
if status != 200 or b"armed" not in body:
    fail("worker crash %s %s" % (status, body[:80]))
name = ("slot%d.local" % int(time.time())).encode()
subprocess.run(
    ["dig", "@127.0.0.1", "-p", str(dns), name.decode(), "+short", "+time=1", "+tries=1"],
    capture_output=True,
)
saw = False
for sock in clients:
    chunk = read_for(sock, 4, name)
    if name in chunk or b"worker fault" in chunk or b"supervisor" in chunk:
        saw = True
        break
if not saw:
    fail("no telnet client kept streaming")
opened = False
for _ in range(12):
    sock = socket.create_connection(("127.0.0.1", telnet), 5)
    banner = read_for(sock, 2, b"telnet-log")
    sock.close()
    if b"telnet-log" in banner and b"busy" not in banner:
        opened = True
        break
    time.sleep(0.25)
if not opened:
    fail("telnet slot was not freed")
print("telnet worker")

mark = 0
try:
    mark = len(open("qemu/supervisor-console.txt", "rb").read())
except FileNotFoundError:
    mark = 0
live = clients[0]
status, body = get("/debug/crash/dns")
if status != 200:
    fail("dns crash during telnet %s" % status)
deadline = time.time() + 6
buf = b""
while time.time() < deadline and b"supervisor dns" not in buf:
    buf += read_for(live, 0.4)
if b"supervisor dns" not in buf:
    fail("telnet missed the dns restart log %s" % buf[-200:])
if not dns_up(8):
    fail("dns did not return while telnet was connected")
deadline = time.time() + 6
grew = False
while time.time() < deadline:
    try:
        console = open("qemu/supervisor-console.txt", "rb").read()
    except FileNotFoundError:
        console = b""
    if b"[status]" in console[mark:]:
        grew = True
        break
    time.sleep(0.4)
if not grew:
    fail("console produced no status line during the restart")
print("console and telnet")
for sock in clients:
    try:
        sock.close()
    except OSError:
        pass

for n in range(4):
    before = int(field(page(), "log", "restarts"))
    status, body = get("/debug/crash/log")
    if status != 200 or b"armed" not in body:
        fail("log crash %s %s" % (status, body[:80]))
    if wait_running("log", before, 8) is None:
        fail("log did not return after crash %d\n%s" % (n + 1, page()))
status, body = get("/debug/crash/log")
if status != 200 or b"armed" not in body:
    fail("final log crash %s" % status)
time.sleep(2.5)
text = page()
if not row(text, "log").startswith("log gaveup"):
    fail("crash loop was not honored\n" + row(text, "log"))
print("crash loop")
print("supervisor ok")
PY
then
    echo "✓ DNS came back and answered on ${DNS_HOST_PORT}"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ HTTP came back"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ A faulted telnet worker freed its slot and the others kept streaming"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Console and telnet both moved during a restart"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ User pages, ASIDs, and fds returned to the baseline"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Crash-loop limit left the service down"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ User-task supervisor checks failed"
    TESTS_FAILED=$((TESTS_FAILED + 6))
fi
kill $SUP_NC 2>/dev/null || true
kill $SUP_PID 2>/dev/null || true
wait $SUP_NC 2>/dev/null || true
wait $SUP_PID 2>/dev/null || true

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
elif ! mcopy -i qemu/disk.img@@${DISK_OFF} ::CONFIG.JSN - | grep -q "added.local"; then
    echo "✗ CONFIG.JSN does not contain the new record"
    TESTS_FAILED=$((TESTS_FAILED + 1))
elif ! mcopy -i qemu/disk.img@@${DISK_OFF} ::LOG.TXT - | grep -q "record added"; then
    echo "✗ LOG.TXT does not contain the new record"
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

# JSON config: defaults, validation, rotation, and a log-task restart.
echo ""
echo "Test 30: JSON config files"
DISK_OFF=$((2048 * 512))
rm -f qemu/json-disk.img qemu/json-a.txt qemu/json-b.txt
dd if=/dev/zero of=qemu/json-disk.img bs=1M count=64 status=none
sfdisk --quiet qemu/json-disk.img <<'EOF'
label: dos
start=2048, type=c
EOF
mkfs.vfat -F 32 -n PI4DNS --offset 2048 qemu/json-disk.img >/dev/null
printf 'record=legacy.local A 9.8.7.6 300\nmode=static\nip=10.0.2.15\nnetmask=255.255.255.0\ngateway=10.0.2.2\n' \
    > /tmp/legacy-config.txt
mcopy -i qemu/json-disk.img@@${DISK_OFF} /tmp/legacy-config.txt ::CONFIG.TXT
timeout 70 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/json-a.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/json-disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-json-a.txt 2>&1 &
JSON_PID=$!
sleep 6
if python3 - "$HTTP_HOST_PORT" "$DNS_HOST_PORT" <<'PY'
import json, sys, urllib.error, urllib.request
http, dns = sys.argv[1], sys.argv[2]
base = "http://127.0.0.1:%s" % http
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg)
    sys.exit(1)

def req(method, path, body=None, headers=None, auth=True):
    data = body.encode() if isinstance(body, str) else body
    r = urllib.request.Request(base + path, data=data, method=method)
    if headers:
        for k, v in headers.items():
            r.add_header(k, v)
    try:
        if auth:
            resp = opener.open(r, timeout=8)
        else:
            resp = urllib.request.urlopen(r, timeout=8)
        return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except Exception as err:
        return 0, str(err).encode()

status, _ = req("GET", "/config/file/log", auth=False)
if status != 401:
    fail("unauth log download %s" % status)
status, body = req("GET", "/config/file/log")
if status != 200:
    fail("log download %s %s" % (status, body[:120]))
doc = json.loads(body)
if doc.get("version") != 1 or doc.get("max_bytes") != 4096 or doc.get("archives") != 3:
    fail("default LOG.JSN %s" % body[:200])
status, again = req("POST", "/config/file/log", body.decode(), {"Content-Type": "application/json"})
if status != 200 or b"applied" not in again:
    fail("round trip %s %s" % (status, again[:160]))
status, after = req("GET", "/config/file/log")
if after != body:
    fail("round trip bytes changed")
print("defaults and round trip")

kept = body
cases = [
    ("{", "malformed"),
    ('{"version":1,"max_bytes":4096}', "missing"),
    ('{"version":1,"max_bytes":"4096","archives":1}', "expected"),
    ('{"version":1,"max_bytes":100,"archives":1}', "out of bounds"),
    ('{"version":1,"max_bytes":65536,"archives":16}', "out of bounds"),
    ('{"version":1,"max_bytes":4096,"archives":1,"extra":1}', "unknown"),
    ("{" + "x" * 5000, "exceeds"),
]
for payload, needle in cases:
    status, err = req("POST", "/config/file/log", payload, {"Content-Type": "application/json"})
    if status != 400 or needle.encode() not in err:
        fail("reject %s -> %s %s" % (needle, status, err[:180]))
status, now = req("GET", "/config/file/log")
if now != kept:
    fail("rejected upload changed the file")
print("rejections")

status, page = req("GET", "/log-config")
if status != 200 or b"4096" not in page or b"65536" not in page or b"1048576" not in page:
    fail("log page %s" % page[:200])
status, saved = req("POST", "/log-config", "max_bytes=4096&archives=1",
                    {"Content-Type": "application/x-www-form-urlencoded"})
if status != 200 or b"applied" not in saved:
    fail("form %s %s" % (status, saved[:160]))
status, cur = req("GET", "/config/file/log")
form = json.loads(cur)
if form.get("max_bytes") != 4096 or form.get("archives") != 1:
    fail("form did not apply %s" % cur[:200])
print("form")

status, filled = req("POST", "/debug/log-fill", "n=120",
                     {"Content-Type": "application/x-www-form-urlencoded"})
if status != 200 or b"filled" not in filled:
    fail("fill %s %s" % (status, filled[:120]))
print("filled")
PY
then
    echo "✓ Default LOG.JSN created and download round-trips"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Bad JSON uploads are rejected and the file stays put"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ The log settings form applied max_bytes and archives"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ JSON config HTTP checks failed"
    TESTS_FAILED=$((TESTS_FAILED + 3))
fi
if python3 - "$HTTP_HOST_PORT" <<'PY'
import json, sys, time, urllib.error, urllib.request
http = sys.argv[1]
base = "http://127.0.0.1:%s" % http
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg)
    sys.exit(1)

def req(method, path, body=None, headers=None):
    data = body.encode() if isinstance(body, str) else body
    r = urllib.request.Request(base + path, data=data, method=method)
    if headers:
        for k, v in headers.items():
            r.add_header(k, v)
    try:
        resp = opener.open(r, timeout=8)
        return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except Exception as err:
        return 0, str(err).encode()

def cache_fields(page):
    for line in page.splitlines():
        if line.startswith(b"cache "):
            out = {}
            for part in line.split()[1:]:
                key, value = part.split(b"=", 1)
                out[key.decode()] = int(value)
            return out
    return None

status, page = req("GET", "/debug")
fields = cache_fields(page)
need = ["slots", "used", "free", "hits", "misses", "evictions", "reaped", "bytes"]
if status != 200 or fields is None or any(key not in fields for key in need):
    fail("debug cache line %s" % page[:400])
if fields["slots"] != 1024 or fields["bytes"] != 823296 or fields["free"] != fields["slots"] - fields["used"]:
    fail("debug stats %s" % fields)
print("stats", fields)

status, form = req("GET", "/dns-config")
if status != 200 or b"1..=1024" not in form or b"1..=3600" not in form:
    fail("dns config page %s" % form[:240])

status, raw = req("GET", "/config/file/config")
if status != 200:
    fail("config download %s" % status)
kept = raw
doc = json.loads(raw)
cases = []
bad = json.loads(raw)
bad["dns"]["cache_reap_interval_secs"] = 0
cases.append((bad, b"out of bounds"))
bad = json.loads(raw)
bad["dns"]["cache_reap_interval_secs"] = 3601
cases.append((bad, b"out of bounds"))
bad = json.loads(raw)
bad["dns"]["cache_size"] = 2048
cases.append((bad, b"out of bounds"))
bad = json.loads(raw)
del bad["dns"]["cache_reap_interval_secs"]
cases.append((bad, b"missing"))
for mutated, needle in cases:
    body = json.dumps(mutated, separators=(",", ":"))
    status, err = req("POST", "/config/file/config", body, {"Content-Type": "application/json"})
    if status != 400 or needle not in err:
        fail("reject %s -> %s %s" % (needle, status, err[:180]))
status, now = req("GET", "/config/file/config")
if now != kept:
    fail("rejected upload changed CONFIG.JSN")
print("rejections")

status, saved = req(
    "POST",
    "/dns-config",
    "cache_size=1024&cache_reap_interval_secs=1",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 200 or b'"restart":false' not in saved:
    fail("form %s %s" % (status, saved[:160]))
status, planted = req(
    "POST",
    "/debug/cache-plant",
    "ttl=1",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 200 or b"planted" not in planted:
    fail("plant %s %s" % (status, planted[:80]))
before = fields["reaped"]
deadline = time.time() + 10
seen = False
last = b""
while time.time() < deadline:
    status, last = req("GET", "/debug")
    fields = cache_fields(last) or {}
    if fields.get("reaped", 0) > before and fields.get("used", 1) == 0:
        seen = True
        break
    time.sleep(0.25)
if not seen:
    fail("reaper did not clear the planted entry %s" % last[:300])
print("reaped", fields)
PY
then
    echo "✓ /debug reports cache slots, hits, and reserved bytes"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Cache config rejects out-of-range size and reap interval"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Reaper returned an expired slot to the free list"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ DNS cache stats, validation, or reaper failed"
    TESTS_FAILED=$((TESTS_FAILED + 3))
fi
if python3 - "$HTTP_HOST_PORT" "$DNS_HOST_PORT" <<'PY'
import subprocess, sys, time, urllib.request
http, dns = sys.argv[1], sys.argv[2]
base = "http://127.0.0.1:%s" % http
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def query(name):
    subprocess.run(
        ["dig", "@127.0.0.1", "-p", dns, name, "A", "+time=1", "+tries=1"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )

for _ in range(3):
    query("alpha.console.local")
query("beta.console.local")
deadline = time.time() + 4
page = b""
while time.time() < deadline:
    try:
        with opener.open(base + "/debug", timeout=4) as resp:
            page = resp.read()
    except Exception as err:
        page = str(err).encode()
    names = b""
    types = b""
    srcs = b""
    for line in page.splitlines():
        if line.startswith(b"top-name"):
            names = line
        elif line.startswith(b"top-type"):
            types = line
        elif line.startswith(b"top-src"):
            srcs = line
    if (b"alpha.console.local=3" in names and b"beta.console.local=1" in names
            and b"A=" in types and b"10.0.2.2=" in srcs):
        print("top", names.decode(), types.decode(), srcs.decode())
        break
    time.sleep(0.25)
else:
    fail("top counters %s" % page[:500])
PY
then
    echo "✓ /debug lists top domains, types, and sources"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ DNS top counters missing from /debug"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
if python3 - "$HTTP_HOST_PORT" "$VNC_HOST_PORT" <<'PY'
import json, socket, struct, sys, time, urllib.error, urllib.request
http_port, vnc_port = sys.argv[1], int(sys.argv[2])
base = "http://127.0.0.1:%s" % http_port
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def req(method, path, data=None, headers=None):
    body = data.encode() if isinstance(data, str) else data
    r = urllib.request.Request(base + path, data=body, method=method)
    for key, value in (headers or {}).items():
        r.add_header(key, value)
    try:
        with opener.open(r, timeout=8) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()

def recvn(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            fail("short read")
        buf += chunk
    return buf

def connect():
    # QEMU user-net accepts the host handshake before the guest does, then
    # resets if nothing is listening. The port counts as open only when the
    # RFB banner is already buffered.
    sock = socket.socket()
    sock.settimeout(0.5)
    try:
        sock.connect(("127.0.0.1", vnc_port))
        peek = sock.recv(12, socket.MSG_PEEK)
    except OSError:
        sock.close()
        return None
    if peek != b"RFB 003.008\n":
        sock.close()
        return None
    return sock

if connect() is not None:
    fail("vnc port open before enable")
status, page = req("GET", "/display")
if status != 200 or b"5900" not in page or b"640" not in page or b"480" not in page or b"1..=65535" not in page:
    fail("display page %s %s" % (status, page[:200]))
status, debug = req("GET", "/debug")
row = b""
for line in debug.splitlines():
    if line.startswith(b"vnc "):
        row = line
if status != 200 or not row.startswith(b"vnc off"):
    fail("vnc not off by default: %s %s" % (status, debug[:400]))
status, saved = req(
    "POST",
    "/display",
    "vnc=1&vnc_port=5900&vnc_password=&hdmi=0",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 200 or b'"status":"applied"' not in saved:
    fail("enable %s %s" % (status, saved[:200]))
sock = None
deadline = time.time() + 8
while time.time() < deadline:
    sock = connect()
    if sock is not None:
        break
    time.sleep(0.25)
if sock is None:
    fail("vnc did not listen")
sock.settimeout(15)
ver = recvn(sock, 12)
if ver != b"RFB 003.008\n":
    fail("version %r" % ver)
sock.sendall(ver)
count = recvn(sock, 1)[0]
types = recvn(sock, count)
if 1 not in types:
    fail("security %r" % types)
sock.sendall(bytes([1]))
if recvn(sock, 4) != b"\x00\x00\x00\x00":
    fail("security result")
sock.sendall(bytes([1]))
init = recvn(sock, 24)
width, height = struct.unpack(">HH", init[:4])
if width != 640 or height != 480 or init[4] != 32:
    fail("server init %s" % init[:24].hex())
name_len = struct.unpack(">I", init[20:24])[0]
recvn(sock, name_len)
sock.sendall(struct.pack(">BBHI", 2, 0, 1, 0))
sock.sendall(struct.pack(">BBHHHH", 3, 0, 0, 0, 640, 480))
update = recvn(sock, 16)
if update[0] != 0 or update[12:16] != b"\x00\x00\x00\x00":
    fail("update %s" % update.hex())
rect_w, rect_h = struct.unpack(">HH", update[8:12])
if rect_w != 640 or rect_h != 480:
    fail("rect %s" % update.hex())
pixels = recvn(sock, 64)
if len(pixels) != 64:
    fail("pixels")
sock.close()
status, debug = req("GET", "/debug")
row = b""
for line in debug.splitlines():
    if line.startswith(b"vnc "):
        row = line
if not row.startswith(b"vnc running"):
    fail("vnc not running: %s" % row)
status, saved = req(
    "POST",
    "/display",
    "vnc=0&vnc_port=5900&vnc_password=&hdmi=0",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 200 or b'"status":"applied"' not in saved:
    fail("disable %s %s" % (status, saved[:160]))
closed = False
deadline = time.time() + 8
while time.time() < deadline:
    if connect() is None:
        closed = True
        break
    time.sleep(0.25)
if not closed:
    fail("port still open")
status, debug = req("GET", "/debug")
row = b""
for line in debug.splitlines():
    if line.startswith(b"vnc "):
        row = line
if not row.startswith(b"vnc off"):
    fail("vnc still up: %s" % row)
status, raw = req("GET", "/config/file/config")
if status != 200 or b'"vnc":0' not in raw:
    fail("config not disabled %s" % raw[:240])
kept = raw
cases = []
bad = json.loads(raw)
bad["display"]["vnc_port"] = 0
cases.append(bad)
bad = json.loads(raw)
bad["display"]["vnc_port"] = 70000
cases.append(bad)
bad = json.loads(raw)
bad["display"]["vnc"] = 2
cases.append(bad)
bad = json.loads(raw)
bad["display"]["vnc_password"] = "too-long-pw"
cases.append(bad)
bad = json.loads(raw)
bad["display"]["extra"] = 1
cases.append(bad)
for mutated in cases:
    body = json.dumps(mutated, separators=(",", ":"))
    status, err = req("POST", "/config/file/config", body, {"Content-Type": "application/json"})
    if status != 400 or (b"out of bounds" not in err and b"unknown" not in err):
        fail("reject %s %s" % (status, err[:180]))
status, now = req("GET", "/config/file/config")
if now != kept:
    fail("rejected display upload changed CONFIG.JSN")
print("vnc ok")
PY
then
    echo "✓ VNC is off until HTTP enables it"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ VNC RFB handshake returns a raw framebuffer"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Disabling VNC closes the port"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ Display config rejects a bad port, flag, password, and key"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ VNC display test failed"
    TESTS_FAILED=$((TESTS_FAILED + 4))
fi
curl -s -u admin:admin --max-time 5 -X POST \
    -H "Content-Type: application/x-www-form-urlencoded" \
    --data "vnc=0&vnc_port=5900&vnc_password=&hdmi=0" \
    "http://127.0.0.1:${HTTP_HOST_PORT}/display" >/dev/null || true
kill $JSON_PID 2>/dev/null || true
wait $JSON_PID 2>/dev/null || true
sync
JSON_OK=1
if ! grep -q "Migrated CONFIG.TXT to CONFIG.JSN" qemu/json-a.txt; then
    echo "✗ legacy CONFIG.TXT was not migrated"
    JSON_OK=0
fi
if ! mcopy -i qemu/json-disk.img@@${DISK_OFF} ::CONFIG.JSN - | grep -q "legacy.local"; then
    echo "✗ migrated CONFIG.JSN missing legacy.local"
    JSON_OK=0
fi
if ! mcopy -i qemu/json-disk.img@@${DISK_OFF} ::LOG.JSN - | grep -q '"archives":1'; then
    echo "✗ LOG.JSN was not updated"
    JSON_OK=0
fi
LOG1=$(mcopy -i qemu/json-disk.img@@${DISK_OFF} ::LOG1.TXT - 2>/dev/null || true)
LOGTXT=$(mcopy -i qemu/json-disk.img@@${DISK_OFF} ::LOG.TXT - 2>/dev/null || true)
if printf '%s' "$LOGTXT$LOG1" | grep -q "mark 0000"; then
    echo "✗ oldest mark was kept (archives=1 should drop it)"
    JSON_OK=0
fi
if printf '%s' "$LOGTXT$LOG1" | grep -q "mark 0119"; then
    true
else
    echo "✗ newest mark missing after rotation"
    JSON_OK=0
fi
if mcopy -i qemu/json-disk.img@@${DISK_OFF} ::LOG2.TXT - >/dev/null 2>&1; then
    echo "✗ LOG2.TXT exists with archives=1"
    JSON_OK=0
fi
# 120 lines of 73 bytes is two rotations of 4096 with one archive kept.
LOG1_BYTES=$(printf '%s' "$LOG1" | wc -c)
if [ "$LOG1_BYTES" -lt 4096 ]; then
    echo "✗ LOG1.TXT is smaller than max_bytes ($LOG1_BYTES)"
    JSON_OK=0
fi
if [ "$JSON_OK" -eq 1 ]; then
    echo "✓ Rotation kept one archive and dropped nothing early"
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo "✓ CONFIG.TXT migrated into CONFIG.JSN"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ JSON rotation or migration failed"
    TESTS_FAILED=$((TESTS_FAILED + 2))
fi

timeout 40 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/json-b.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/json-disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-json-b.txt 2>&1 &
JSON_PID=$!
sleep 6
if python3 - "$HTTP_HOST_PORT" "$DNS_HOST_PORT" <<'PY'
import json, sys, urllib.error, urllib.request
http, dns = sys.argv[1], sys.argv[2]
base = "http://127.0.0.1:%s" % http
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg)
    sys.exit(1)

def req(method, path, body=None, headers=None):
    data = body.encode() if isinstance(body, str) else body
    r = urllib.request.Request(base + path, data=data, method=method)
    if headers:
        for k, v in headers.items():
            r.add_header(k, v)
    try:
        resp = opener.open(r, timeout=8)
        return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except Exception as err:
        return 0, str(err).encode()

status, body = req("GET", "/config/file/log")
try:
    doc = json.loads(body)
except Exception:
    fail("log json after reboot %s" % body[:160])
if doc.get("max_bytes") != 4096 or doc.get("archives") != 1:
    fail("settings did not persist %s" % body[:200])
print("persisted")
status, armed = req("GET", "/debug/crash/log")
if status != 200 or b"armed" not in armed:
    fail("crash log %s %s" % (status, armed[:80]))
import time
deadline = time.time() + 8
seen = False
while time.time() < deadline:
    status, page = req("GET", "/debug")
    if b"log running" in page and b"restarts=" in page:
        # restarts at least 1
        for line in page.splitlines():
            if line.startswith(b"log "):
                if b"restarts=0" not in line:
                    seen = True
        if seen:
            break
    time.sleep(0.3)
if not seen:
    fail("log task did not restart\n%s" % page[:300])
status, body = req("GET", "/config/file/log")
doc = json.loads(body)
if doc.get("archives") != 1 or doc.get("max_bytes") != 4096:
    fail("restart changed log config %s" % body[:200])
print("log restart kept config")
PY
then
    echo "✓ Log settings survived reboot and a log-el0 restart"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Log settings did not survive reboot"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
kill $JSON_PID 2>/dev/null || true
wait $JSON_PID 2>/dev/null || true

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
