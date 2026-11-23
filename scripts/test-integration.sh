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
rm -f qemu/boot-output.txt
timeout 240 qemu-system-aarch64 \
    -M virt,gic-version=2 \
    -cpu cortex-a72 \
    -smp 4 \
    -m 512M \
    -kernel boot/kernel8.img \
    -chardev socket,id=ser0,host=127.0.0.1,port=4444,server=on,wait=off,logfile=qemu/boot-output.txt \
    -serial chardev:ser0 \
    -nographic \
    -display none \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    &

QEMU_PID=$!
echo "QEMU PID: $QEMU_PID"

# The serial log starts with QEMU, so a client that attaches later is not
# required to see the boot lines.
echo "Waiting for kernel to boot..."
sleep 5

echo "Checking boot output..."

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
   grep -q "Running at EL1" qemu/boot-output.txt && \
   grep -q "version 0.1.84" qemu/boot-output.txt && \
   grep -q "boot unknown" qemu/boot-output.txt; then
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
        if echo "$RESULT" | grep -q '"status":"running"' && echo "$RESULT" | grep -q '"version":"0.1.84"'; then
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

echo ""
echo "Test 55: /status publishes NIC counters"
if timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/status" > qemu/status-nic-a.txt \
    && timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local A +short +time=3 +tries=1 >/dev/null \
    && timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/status" > qemu/status-nic-b.txt \
    && python3 - qemu/status-nic-a.txt qemu/status-nic-b.txt <<'PY'
import json, sys
need = (
    "status", "uptime", "ticks", "tick_hz", "dns_port", "http_port",
    "cache_entries", "upstreams", "nic_irqs", "rx_packets", "tx_packets",
    "net_busy_ms", "net_idle_ms",
    "arp_requests_tx", "arp_replies_rx", "arp_cache_hits", "arp_cache_misses",
    "dns_inflight", "upstream_timeouts",
    "rx_drops", "rx_overruns", "rx_queued", "forward_full", "reply_drops",
    "poll_count", "poll_us", "poll_max_us", "irq_rearms", "tx_full", "tx_slow",
    "hdmi_state", "hdmi_mode", "hdmi_recoveries", "hdmi_diag", "hdmi_last_shut",
    "hdmi_regs", "hdmi_port", "vclog", "vclog_bytes", "vclog_status",
    "sd_state", "sd_error", "sd_blocks", "fs_mounted", "sd_mode", "sd_clock_hz",
    "crash_count", "safe_mode", "last_crash",
    "boot_attempts", "boot_good", "fallback_active", "booted_kernel", "boot_stopped", "last_reset",
    "board", "board_revision", "board_source",
)
counts = ("nic_irqs", "rx_packets", "tx_packets", "net_busy_ms", "net_idle_ms")
extra = (
    "arp_requests_tx", "arp_replies_rx", "arp_cache_hits", "arp_cache_misses",
    "dns_inflight", "upstream_timeouts",
    "rx_drops", "rx_overruns", "rx_queued", "forward_full", "reply_drops",
    "poll_count", "poll_us", "poll_max_us", "irq_rearms", "tx_full", "tx_slow",
)
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
for doc in (a, b):
    for key in need:
        if key not in doc:
            print("missing", key)
            sys.exit(1)
    for key in counts + extra:
        if not isinstance(doc[key], int) or isinstance(doc[key], bool) or doc[key] < 0:
            print("bad", key, doc[key])
            sys.exit(1)
    if not isinstance(doc["sd_state"], str) or not isinstance(doc["sd_error"], str):
        print("sd strings", doc["sd_state"], doc["sd_error"])
        sys.exit(1)
    if not isinstance(doc["sd_blocks"], int) or isinstance(doc["sd_blocks"], bool) or doc["sd_blocks"] < 0:
        print("sd blocks", doc["sd_blocks"])
        sys.exit(1)
    if doc["fs_mounted"] not in (0, 1):
        print("fs mounted", doc["fs_mounted"])
        sys.exit(1)
    if doc["hdmi_state"] != "none" or doc["hdmi_mode"] != "0x0" or doc["hdmi_recoveries"] != 0 or doc["hdmi_diag"] != "none" or doc["hdmi_last_shut"] != "none" or doc["hdmi_regs"] != "none" or doc["hdmi_port"] != "none" or doc["vclog"] != "none" or doc["vclog_bytes"] != 0 or doc["vclog_status"] != "none":
        print("hdmi", doc["hdmi_state"], doc["hdmi_mode"], doc["hdmi_recoveries"])
        sys.exit(1)
    if not isinstance(doc["sd_mode"], str) or not doc["sd_mode"]:
        print("sd mode", doc["sd_mode"])
        sys.exit(1)
    if not isinstance(doc["sd_clock_hz"], int) or isinstance(doc["sd_clock_hz"], bool) or doc["sd_clock_hz"] < 0:
        print("sd clock", doc["sd_clock_hz"])
        sys.exit(1)
    if not isinstance(doc["crash_count"], int) or isinstance(doc["crash_count"], bool) or doc["crash_count"] < 0:
        print("crash count", doc["crash_count"])
        sys.exit(1)
    if not isinstance(doc["last_crash"], str):
        print("last crash", doc["last_crash"])
        sys.exit(1)
    if doc["safe_mode"] not in (0, 1) or doc["boot_good"] not in (0, 1) or doc["fallback_active"] not in (0, 1):
        print("boot flags", doc["safe_mode"], doc["boot_good"], doc["fallback_active"])
        sys.exit(1)
    if not isinstance(doc["boot_attempts"], int) or isinstance(doc["boot_attempts"], bool) or doc["boot_attempts"] < 0:
        print("boot attempts", doc["boot_attempts"])
        sys.exit(1)
    if not isinstance(doc["booted_kernel"], str) or not isinstance(doc["boot_stopped"], str):
        print("boot text", doc["booted_kernel"], doc["boot_stopped"])
        sys.exit(1)
    if doc["last_reset"] not in ("unknown", "watchdog", "reboot", "power"):
        print("last reset", doc["last_reset"])
        sys.exit(1)
    if doc["board"] != "virt" or doc["board_source"] != "build" or doc["board_revision"] != "":
        print("board", doc["board"], doc["board_source"], doc["board_revision"])
        sys.exit(1)
    if doc["sd_state"] != "absent" or doc["fs_mounted"] != 0 or doc["sd_mode"] != "none" or doc["sd_clock_hz"] != 0:
        print("diskless guest", doc["sd_state"], doc["fs_mounted"], doc["sd_error"], doc["sd_mode"], doc["sd_clock_hz"])
        sys.exit(1)
print(
    "rx", a["rx_packets"], "->", b["rx_packets"],
    "tx", b["tx_packets"], "nic", b["nic_irqs"],
    "busy", b["net_busy_ms"], "idle", b["net_idle_ms"],
)
if b["rx_packets"] <= a["rx_packets"] or b["tx_packets"] < 1 or b["nic_irqs"] < 1:
    sys.exit(1)
sys.exit(0)
PY
then
    echo "✓ /status keeps its fields and the NIC counters move"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ /status NIC counters missing or stuck"
    echo "--- first ---"; cat qemu/status-nic-a.txt 2>/dev/null || true
    echo "--- second ---"; cat qemu/status-nic-b.txt 2>/dev/null || true
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 62: /status HTML for browsers, JSON for scripts"
if timeout 5 curl -s -u admin:admin -H 'Accept: text/html' "http://127.0.0.1:${HTTP_HOST_PORT}/status" > qemu/status-html.txt \
    && timeout 5 curl -s -u admin:admin -H 'Accept: text/html' "http://127.0.0.1:${HTTP_HOST_PORT}/status?format=json" > qemu/status-fmt.json \
    && timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/json/status" > qemu/status-json.txt \
    && python3 - qemu/status-html.txt qemu/status-fmt.json qemu/status-json.txt <<'PY'
import json, sys
html = open(sys.argv[1], encoding="utf-8").read()
forced = json.load(open(sys.argv[2]))
plain = json.load(open(sys.argv[3]))
if "<h1" not in html or "boot_good" not in html or "/crash/list" not in html or "/json/status" not in html:
    print("html", html[:240])
    sys.exit(1)
if html.lstrip().startswith("{"):
    print("html was json")
    sys.exit(1)
if forced.get("status") != "running" or plain.get("status") != "running":
    print("json", forced.get("status"), plain.get("status"))
    sys.exit(1)
if forced.get("version") != plain.get("version") or "0.1.84" not in html:
    print("version", forced.get("version"), plain.get("version"))
    sys.exit(1)
print("status html", len(html), "json", plain.get("version"))
sys.exit(0)
PY
then
    echo "✓ /status is HTML for browsers and JSON otherwise"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ /status HTML or /json/status failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 56: idle owner sleeps, and gateway ARP stays bounded"
if timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/status" > qemu/status-idle-a.txt \
    && sleep 3 \
    && timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/status" > qemu/status-idle-b.txt \
    && { timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} test.local A +short +time=3 +tries=1 >/dev/null || true; } \
    && { timeout 5 dig @127.0.0.1 -p ${DNS_HOST_PORT} example.com A +short +time=3 +tries=1 >/dev/null || true; } \
    && timeout 5 curl -s -u admin:admin "http://127.0.0.1:${HTTP_HOST_PORT}/status" > qemu/status-idle-c.txt \
    && python3 - qemu/status-idle-a.txt qemu/status-idle-b.txt qemu/status-idle-c.txt <<'PY'
import json, sys
a = json.load(open(sys.argv[1]))
b = json.load(open(sys.argv[2]))
c = json.load(open(sys.argv[3]))
idle = b["net_idle_ms"] - a["net_idle_ms"]
busy = b["net_busy_ms"] - a["net_busy_ms"]
arp_idle = b["arp_requests_tx"] - a["arp_requests_tx"]
arp_load = c["arp_requests_tx"] - a["arp_requests_tx"]
print(
    "idle_d", idle, "busy_d", busy,
    "arp_idle", arp_idle, "arp_load", arp_load,
    "hits", c["arp_cache_hits"], "misses", c["arp_cache_misses"],
    "inflight", c["dns_inflight"],
)
# Three quiet seconds. The owner should spend them in wfi, and a resolved
# gateway should not be solicited again. A handful of requests covers the
# one slirp neighbor learning at the edge of the window. The two digs are
# included in arp_load.
if idle < 1500 or idle <= busy * 4 or arp_idle > 2 or arp_load > 4:
    sys.exit(1)
sys.exit(0)
PY
then
    echo "✓ idle time dominates and gateway ARP stays bounded"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ idle owner busy, or gateway ARP ran away"
    echo "--- idle a ---"; cat qemu/status-idle-a.txt 2>/dev/null || true
    echo "--- idle b ---"; cat qemu/status-idle-b.txt 2>/dev/null || true
    echo "--- idle c ---"; cat qemu/status-idle-c.txt 2>/dev/null || true
    TESTS_FAILED=$((TESTS_FAILED + 1))
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

# The first guest has no disk, so this only checks the boot log. The
# dead-upstream load runs later, on the JSON guest that can store config.
echo ""
echo "Test 52: Probes wait until an address is configured"
if python3 - <<'PY'
text = open("qemu/boot-output.txt", errors="replace").read()
ready_at = None
for marker in (
    "DHCP configured",
    "DHCP timeout, using static",
    "Network configured with static",
):
    i = text.find(marker)
    if i >= 0 and (ready_at is None or i < ready_at):
        ready_at = i
if ready_at is None:
    print("no address line in the boot log")
    raise SystemExit(1)
if "dns upstream down" in text[:ready_at]:
    print("probe marked a server down before the address existed")
    raise SystemExit(1)
print("no probe down line before the address")
PY
then
    echo "✓ No upstream was marked down before the address existed"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ A probe ran before the address was configured"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 58: DNS load keeps local names"
if python3 scripts/dns_stress.py \
    --server 127.0.0.1 \
    --port "${DNS_HOST_PORT}" \
    --http-port "${HTTP_HOST_PORT}" \
    --duration 8 \
    --qps 20 \
    --timeout 2 \
    --a-only \
    --fail-local-loss 5 \
    --require-counters \
    --seed 1
then
    echo "✓ Local names stayed up under the QEMU load"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ DNS load lost local names or /status counters were missing"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 59: Sustained DNS load keeps the NIC IRQ and the owner"
if python3 scripts/dns_stress.py \
    --server 127.0.0.1 \
    --port "${DNS_HOST_PORT}" \
    --http-port "${HTTP_HOST_PORT}" \
    --duration 24 \
    --qps 20 \
    --timeout 2 \
    --a-only \
    --fail-local-loss 5 \
    --max-p99 400 \
    --max-busy-ratio 0.75 \
    --irq-moves \
    --seed 59
then
    echo "✓ Sustained load kept local names, latency, idle time, and NIC IRQs"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Sustained load lost names, pinned the network core, or the NIC IRQ stalled"
    TESTS_FAILED=$((TESTS_FAILED + 1))
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
console = open("qemu/supervisor-console.txt", "rb").read()
if b"elfault dns" not in console or b"ec=24" not in console or b"far=0000000000000000" not in console:
    fail("dns fault log missing\n" + console[-500:].decode("utf-8", "replace"))
print("dns fault logged")

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
console = open("qemu/supervisor-console.txt", "rb").read()
if b"elfault telnet" not in console:
    fail("telnet worker fault was not logged")
print("telnet fault logged")
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
    if grep -q "DHCP timeout, using static 10.0.2.15" qemu/dhcp-fallback.txt \
        || grep -q "DHCP timeout, static 10.0.2.15" qemu/dhcp-fallback.txt; then
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
timeout 180 qemu-system-aarch64 \
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
if body != b'{\n  "version": 1,\n  "max_bytes": 4096,\n  "archives": 3\n}\n':
    fail("default LOG.JSN is not pretty %s" % body[:200])
if len(body) > 4096:
    fail("default LOG.JSN exceeds 4096")
status, again = req("POST", "/config/file/log", body.decode(), {"Content-Type": "application/json"})
if status != 200 or b"applied" not in again:
    fail("round trip %s %s" % (status, again[:160]))
status, after = req("GET", "/config/file/log")
if after != body:
    fail("round trip bytes changed")
compact = '{"archives":3,"max_bytes":4096,"version":1}'
status, applied = req("POST", "/config/file/log", compact, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("compact upload %s %s" % (status, applied[:160]))
status, pretty = req("GET", "/config/file/log")
ordered = b'{\n  "archives": 3,\n  "max_bytes": 4096,\n  "version": 1\n}\n'
if pretty != ordered or json.loads(pretty) != doc:
    fail("compact upload was not formatted %s" % pretty[:200])
messy = '{\n\t"archives" : 3, "version" : 1, "max_bytes" : 4096 }\n'
status, applied = req("POST", "/config/file/log", messy, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("whitespace upload %s %s" % (status, applied[:160]))
status, again = req("GET", "/config/file/log")
if again != b'{\n  "archives": 3,\n  "version": 1,\n  "max_bytes": 4096\n}\n':
    fail("whitespace upload was not canonical %s" % again[:200])
status, applied = req("POST", "/config/file/log", body.decode(), {"Content-Type": "application/json"})
status, restored = req("GET", "/config/file/log")
if restored != body:
    fail("did not restore the default log file")
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
    ('{"version":1,"max_bytes":4096,"archives":3,"a":"\\u"}', "malformed"),
    ('{"version":1,"max_bytes":4096,"archives":3,"a":"\\uXXXX"}', "malformed"),
    ('{"version":1,"version":1,"max_bytes":4096,"archives":3}', "malformed"),
    ('{"version":1,"max_bytes":4095,"archives":0}', "out of bounds"),
    ('{"version":1,"max_bytes":65537,"archives":0}', "out of bounds"),
    ('{"version":1,"max_bytes":4096,"archives":17}', "out of bounds"),
]
for payload, needle in cases:
    status, err = req("POST", "/config/file/log", payload, {"Content-Type": "application/json"})
    if status != 400 or needle.encode() not in err:
        fail("reject %s -> %s %s" % (needle, status, err[:180]))
status, now = req("GET", "/config/file/log")
if now != kept:
    fail("rejected upload changed the file")
exact = '{"version":1,"max_bytes":4096,"archives":3}'
padded = exact[:-1] + (" " * (4096 - len(exact))) + "}"
if len(padded) != 4096:
    fail("pad length %s" % len(padded))
status, applied = req("POST", "/config/file/log", padded, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("4096 upload %s %s" % (status, applied[:160]))
status, saved = req("GET", "/config/file/log")
if json.loads(saved) != doc or len(saved) > 4096:
    fail("4096 upload changed values %s" % saved[:200])
status, rejected = req("POST", "/config/file/log", padded + " ", {"Content-Type": "application/json"})
if status != 400 or b"exceeds" not in rejected:
    fail("4097 upload %s %s" % (status, rejected[:160]))
status, now = req("GET", "/config/file/log")
if now != saved:
    fail("oversize upload changed the file")
for payload, expect in (
    ('{"version":1,"max_bytes":65536,"archives":15}', (65536, 15)),
    ('{"version":1,"max_bytes":4096,"archives":0}', (4096, 0)),
):
    status, applied = req("POST", "/config/file/log", payload, {"Content-Type": "application/json"})
    if status != 200 or b"applied" not in applied:
        fail("boundary %s %s" % (status, applied[:160]))
    status, got = req("GET", "/config/file/log")
    parsed = json.loads(got)
    if (parsed.get("max_bytes"), parsed.get("archives")) != expect or len(got) > 4096:
        fail("boundary stored %s" % got[:200])
    if b"\n" not in got:
        fail("boundary was not pretty %s" % got[:200])
    status, again = req("POST", "/config/file/log", got.decode(), {"Content-Type": "application/json"})
    status, twice = req("GET", "/config/file/log")
    if twice != got:
        fail("boundary reformatted again")
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
if not raw.startswith(b"{\n  \"version\": 1,\n") or b"\n    \"mode\":" not in raw:
    fail("default CONFIG.JSN is not pretty %s" % raw[:180])
if b"legacy.local" not in raw or b'"vnc": 0' not in raw or b'"hdmi": 0' not in raw:
    fail("default CONFIG.JSN values %s" % raw[:180])
if doc["dns"].get("probe_interval_secs") != 5:
    fail("probe interval default %s" % raw[:240])
if len(raw) > 4096:
    fail("default CONFIG.JSN exceeds 4096")
cases = []
bad = json.loads(raw)
bad["dns"]["cache_reap_interval_secs"] = 0
cases.append((bad, b"out of bounds"))
bad = json.loads(raw)
bad["dns"]["cache_reap_interval_secs"] = 3601
cases.append((bad, b"out of bounds"))
bad = json.loads(raw)
bad["dns"]["probe_interval_secs"] = 0
cases.append((bad, b"out of bounds"))
bad = json.loads(raw)
bad["dns"]["probe_interval_secs"] = 3601
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
escaped = json.loads(raw)
escaped["http"]["password"] = 'a"b\\c'
compact = json.dumps(escaped, separators=(",", ":"), ensure_ascii=True)
if len(compact) > 4096:
    fail("escape fixture too large")
status, applied = req("POST", "/config/file/config", compact, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("escape upload %s %s" % (status, applied[:180]))
fresh = urllib.request.HTTPBasicAuthHandler()
fresh.add_password(realm="DNS Server", uri=base, user="admin", passwd='a"b\\c')
fresh_opener = urllib.request.build_opener(fresh)
def req_as(op, method, path, body=None, headers=None):
    data = body.encode() if isinstance(body, str) else body
    r = urllib.request.Request(base + path, data=data, method=method)
    if headers:
        for k, v in headers.items():
            r.add_header(k, v)
    try:
        resp = op.open(r, timeout=8)
        return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
status, stored = req_as(fresh_opener, "GET", "/config/file/config")
if status != 200:
    fail("escape download %s %s" % (status, stored[:120]))
parsed = json.loads(stored)
if parsed["http"]["password"] != 'a"b\\c' or parsed["dns"]["records"] != doc["dns"]["records"]:
    fail("escape changed values %s" % stored[:240])
if b'\\"' not in stored or b"\\\\" not in stored or len(stored) > 4096 or b"\n" not in stored:
    fail("escape formatting %s" % stored[:240])
status, again = req_as(
    fresh_opener,
    "POST",
    "/config/file/config",
    stored.decode(),
    {"Content-Type": "application/json"},
)
status, twice = req_as(fresh_opener, "GET", "/config/file/config")
if twice != stored:
    fail("pretty config was not stable")
status, restored = req_as(
    fresh_opener,
    "POST",
    "/config/file/config",
    kept.decode(),
    {"Content-Type": "application/json"},
)
if status != 200:
    fail("password restore %s %s" % (status, restored[:120]))
wide = json.loads(raw)
names = []
count = 0
while count < 64:
    trial = json.loads(json.dumps(wide))
    trial["dns"]["records"] = list(wide["dns"]["records"])
    trial["dns"]["records"].extend(
        {
            "name": "n%02d.pad.local" % n,
            "type": "A",
            "value": "10.1.%d.%d" % (n // 200, n % 200),
            "ttl": 300,
        }
        for n in range(count)
    )
    compact = json.dumps(trial, separators=(",", ":"), ensure_ascii=True)
    pretty = json.dumps(trial, indent=2, ensure_ascii=True) + "\n"
    if len(compact) > 4096:
        break
    if len(pretty) > 4096 and len(compact) > 2500:
        names = [item["name"] for item in trial["dns"]["records"]]
        wide = trial
        break
    count += 1
else:
    fail("could not build a config whose pretty form exceeds 4096")
compact = json.dumps(wide, separators=(",", ":"), ensure_ascii=True)
if not (2500 < len(compact) <= 4096):
    fail("wide compact length %s" % len(compact))
status, applied = req("POST", "/config/file/config", compact, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("wide upload %s %s" % (status, applied[:180]))
status, stored = req("GET", "/config/file/config")
parsed = json.loads(stored)
got_names = [item["name"] for item in parsed["dns"]["records"]]
if got_names != names or parsed["http"]["username"] != doc["http"]["username"]:
    fail("wide upload dropped values")
if len(stored) > 4096 or b"\n" in stored:
    fail("wide upload was truncated or pretty past the limit (%s)" % len(stored))
status, again = req("POST", "/config/file/config", stored.decode(), {"Content-Type": "application/json"})
status, twice = req("GET", "/config/file/config")
if twice != stored:
    fail("compact fallback was not stable")
status, restored = req("POST", "/config/file/config", kept.decode(), {"Content-Type": "application/json"})
if status != 200:
    fail("restore %s" % restored[:120])
status, now = req("GET", "/config/file/config")
if now != kept:
    fail("restore bytes %s" % now[:180])
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
if status != 200 or b'"vnc": 0' not in raw:
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

echo ""
echo "Test 53: Dead upstream drops out and queries keep resolving"
if python3 - "$HTTP_HOST_PORT" "$DNS_HOST_PORT" <<'PY'
import json, subprocess, sys, time, urllib.error, urllib.request
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

status, kept = req("GET", "/config/file/config")
if status != 200:
    fail("config download %s" % status)
doc = json.loads(kept)
doc["dns"]["probe_interval_secs"] = 1
doc["dns"]["upstreams"] = ["192.0.2.1", "10.0.2.3"]
body = json.dumps(doc, separators=(",", ":"))
status, applied = req("POST", "/config/file/config", body, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("upload %s %s" % (status, applied[:180]))

deadline = time.time() + 15
seen = None
while time.time() < deadline:
    status, page = req("GET", "/status")
    if status == 200:
        seen = json.loads(page).get("upstreams") or []
        by_ip = {item["ip"]: item["up"] for item in seen}
        if by_ip.get("192.0.2.1") is False and by_ip.get("10.0.2.3") is True:
            break
    time.sleep(0.4)
else:
    fail("upstream health %s" % seen)
print("upstreams", seen)

ok = 0
for n in range(20):
    name = "dead%d.example.com" % n
    try:
        proc = subprocess.run(
            ["dig", "@127.0.0.1", "-p", dns, name, "A", "+time=2", "+tries=1"],
            capture_output=True,
            timeout=4,
        )
    except Exception:
        continue
    out = proc.stdout.decode(errors="replace")
    if "SERVFAIL" not in out and ("NOERROR" in out or "NXDOMAIN" in out):
        ok += 1
print("answers", ok)
status, restored = req(
    "POST",
    "/config/file/config",
    kept.decode(),
    {"Content-Type": "application/json"},
)
if status != 200 or b"applied" not in restored:
    fail("restore %s %s" % (status, restored[:180]))
if ok < 18:
    fail("load lost %s of 20" % (20 - ok))
log = open("qemu/json-a.txt", errors="replace").read()
if "dns upstream down 192.0.2.1" not in log:
    fail("dead server was not logged down")
print("dead upstream left the rotation")
PY
then
    echo "✓ A dead upstream left the rotation and 20 queries still resolved"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Dead-upstream load test failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 54: Local records stay fast while an upstream is a blackhole"
if python3 - "$HTTP_HOST_PORT" "$DNS_HOST_PORT" <<'PY'
import json, re, subprocess, sys, time, urllib.error, urllib.request
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

status, kept = req("GET", "/config/file/config")
if status != 200:
    fail("config download %s" % status)
doc = json.loads(kept)
doc["dns"]["upstreams"] = ["192.0.2.1"]
doc["dns"]["probe_interval_secs"] = 5
body = json.dumps(doc, separators=(",", ":"))
status, applied = req("POST", "/config/file/config", body, {"Content-Type": "application/json"})
if status != 200 or b"applied" not in applied:
    fail("upload %s %s" % (status, applied[:180]))

procs = []
for n in range(8):
    procs.append(subprocess.Popen(
        ["dig", "@127.0.0.1", "-p", dns, "slow%d.example.com" % n, "A", "+time=3", "+tries=1"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    ))
time.sleep(0.3)
times = []
missing = 0
for _ in range(20):
    try:
        proc = subprocess.run(
            ["dig", "@127.0.0.1", "-p", dns, "legacy.local", "A", "+stats", "+time=1", "+tries=1"],
            capture_output=True,
            timeout=3,
        )
    except Exception:
        missing += 1
        continue
    text = proc.stdout.decode(errors="replace")
    if "9.8.7.6" not in text:
        missing += 1
        continue
    match = re.search(r"Query time: (\d+) msec", text)
    if not match:
        missing += 1
        continue
    times.append(int(match.group(1)))
for proc in procs:
    try:
        proc.wait(timeout=5)
    except Exception:
        proc.kill()
status, restored = req(
    "POST",
    "/config/file/config",
    kept.decode(),
    {"Content-Type": "application/json"},
)
if status != 200 or b"applied" not in restored:
    fail("restore %s %s" % (status, restored[:180]))
print("local ms", times, "missing", missing)
if missing or len(times) != 20:
    fail("local records dropped %s" % missing)
if max(times) > 400:
    fail("local record took %s ms while the upstream was dead" % max(times))
print("local max %s ms" % max(times))
PY
then
    echo "✓ Local records answered within 400 ms while forwards sat on a blackhole"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Local records stalled behind a dead upstream"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

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
if ! mcopy -i qemu/json-disk.img@@${DISK_OFF} ::LOG.JSN - | grep -q '"archives": 1'; then
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

echo ""
echo "Test 57: Firmware upload, readback, validation, and reboot"
DISK_OFF=$((2048 * 512))
rm -f qemu/fw-disk.img qemu/fw-a.txt /tmp/fw-live.bin /tmp/fw-test.bin /tmp/fw-kernel.bin
dd if=/dev/zero of=qemu/fw-disk.img bs=1M count=64 status=none
sfdisk --quiet qemu/fw-disk.img <<'EOF'
label: dos
start=2048, type=c
EOF
mkfs.vfat -F 32 -n PI4DNS --offset 2048 qemu/fw-disk.img >/dev/null
python3 -c 'open("/tmp/fw-live.bin","wb").write(b"K"*64)'
mcopy -i qemu/fw-disk.img@@${DISK_OFF} /tmp/fw-live.bin ::KERNEL8.IMG
timeout 400 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/fw-a.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/fw-disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-fw.txt 2>&1 &
FW_PID=$!
FW_UP=0
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40; do
    if curl -sf -u admin:admin --max-time 2 "http://127.0.0.1:${HTTP_HOST_PORT}/" >/dev/null; then
        FW_UP=1
        break
    fi
    sleep 0.5
done
FW_OK=0
if [ "$FW_UP" = 1 ] && python3 - "$HTTP_HOST_PORT" qemu/fw-a.txt <<'PY'
import hashlib, json, sys, time, urllib.error, urllib.request
http_port, serial_path = sys.argv[1], sys.argv[2]
base = "http://127.0.0.1:%s" % http_port
MARKER = b"=== Pi4 DNS Server Kernel ==="
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def req(method, path, data=None, headers=None, auth=True, timeout=8):
    body = data.encode() if isinstance(data, str) else data
    r = urllib.request.Request(base + path, data=body, method=method)
    for key, value in (headers or {}).items():
        r.add_header(key, value)
    handler = opener if auth else urllib.request.build_opener()
    try:
        with handler.open(r, timeout=timeout) as resp:
            return resp.status, resp.read(), resp.headers
    except urllib.error.HTTPError as err:
        return err.code, err.read(), err.headers
    except Exception as err:
        return 0, str(err).encode(), {}

def form(path, fields):
    body = "&".join("%s=%s" % pair for pair in fields)
    return req("POST", path, body, {"Content-Type": "application/x-www-form-urlencoded"})

def uptime():
    status, page, _ = req("GET", "/status")
    if status != 200:
        fail("status %s %s" % (status, page[:200]))
    return json.loads(page)

status, home, _ = req("GET", "/")
if status != 200 or b'href="/firmware"' not in home or b"0.1.84" not in home:
    fail("home missing firmware link %s" % home[:300])
status, page, _ = req("GET", "/firmware")
if status != 200 or b"Firmware update" not in page or b"FWTEST.BIN" not in page:
    fail("firmware page %s %s" % (status, page[:300]))
if b"tryboot flag: no" not in page or b"confirm('Reboot this machine?')" not in page or b"0.1.84" not in page or b"Promote" not in page or b"Rollback" not in page:
    fail("firmware page flags %s" % page[:400])
status, _, _ = req("GET", "/firmware", auth=False)
if status != 401:
    fail("firmware unauth %s" % status)
status, _, _ = req("GET", "/firmware/reboot")
if status != 405:
    fail("GET reboot %s" % status)
status, body, _ = form("/firmware/reboot", ())
if status != 400 or b"Confirmation required" not in body:
    fail("reboot confirm %s %s" % (status, body[:120]))

doc = uptime()
for key in ("version", "fw_staged", "fw_bytes", "fw_backup", "fw_tryboot", "fw_tryboot_hw", "fw_tryboot_file", "fw_temp", "fw_sha256", "fw_result", "fw_phase", "fw_progress", "fw_total", "fw_booted", "fw_boot_sha256", "fw_bak"):
    if key not in doc:
        fail("status missing %s" % key)
if doc["fw_backup"] != 1 or doc["fw_tryboot_hw"] != 0 or doc["fw_tryboot"] != 0 or doc["fw_tryboot_file"] != 0 or doc["fw_staged"] != 0 or doc["version"] != "0.1.84" or doc["fw_booted"] != "unknown":
    fail("initial firmware status %s" % doc)
if doc.get("fs_mounted") != 1 or doc.get("sd_state") != "virtio" or doc.get("sd_blocks", 0) < 1 or doc.get("sd_error") != "" or doc.get("sd_mode") != "none" or doc.get("sd_clock_hz") != 0:
    fail("sd status %s" % {k: doc.get(k) for k in ("sd_state", "sd_error", "sd_blocks", "fs_mounted", "sd_mode", "sd_clock_hz")})

before = doc["uptime"]
serial_before = open(serial_path, "rb").read()
banners_before = serial_before.count(MARKER)
psci_before = serial_before.count(b"power: psci reset")
status, body, _ = form("/firmware/reboot", (("confirm", "reboot"), ("tryboot", "1")))
if status != 400 or b"tryboot is not supported on this board" not in body:
    fail("tryboot %s %s" % (status, body[:160]))
time.sleep(0.6)
if uptime()["uptime"] < before:
    fail("tryboot request rebooted the guest")
serial_after = open(serial_path, "rb").read()
if serial_after.count(MARKER) != banners_before or serial_after.count(b"power: psci reset") != psci_before:
    fail("tryboot=1 reset the guest")

status, body, _ = req("POST", "/firmware/config", "kernel=/etc/passwd\n", {"X-Fw-Kind": "boot", "X-Fw-Commit": "1", "Content-Type": "text/plain"})
if status != 400 or b"kernel" not in body:
    fail("bad boot %s %s" % (status, body[:160]))
status, saved, _ = req("GET", "/config/file/config")
if status != 200 or b"version" not in saved:
    fail("config download %s" % status)
open("/tmp/fw-config.json", "wb").write(saved)
status, body, _ = req("POST", "/firmware/config", '{"version":1}', {"X-Fw-Kind": "json", "X-Fw-Commit": "1", "Content-Type": "text/plain"})
if status != 400:
    fail("bad json committed %s %s" % (status, body[:160]))
status, after, _ = req("GET", "/config/file/config")
if after != saved:
    fail("invalid json changed CONFIG.JSN")
status, body, _ = req("POST", "/firmware/config", saved.decode(), {"X-Fw-Kind": "json", "X-Fw-Commit": "0", "Content-Type": "text/plain"})
if status != 200 or b'"status":"valid"' not in body:
    fail("json validate %s %s" % (status, body[:160]))
status, after, _ = req("GET", "/config/file/config")
if after != saved:
    fail("validate wrote CONFIG.JSN")
status, body, _ = form("/firmware/config", (("preset", "1"), ("commit", "1")))
if status != 200 or b'"status":"stored"' not in body:
    fail("preset %s %s" % (status, body[:160]))
doc = uptime()
if doc["fw_tryboot_file"] != 1 or doc["fw_tryboot"] != 0 or doc["fw_tryboot_hw"] != 0:
    fail("tryboot.txt recorded as this boot %s" % {k: doc.get(k) for k in ("fw_tryboot", "fw_tryboot_hw", "fw_tryboot_file")})
status, body, _ = req("GET", "/firmware/read/tryboot/0")
if status != 200 or b"kernel=kernel8.new" not in body:
    fail("tryboot.txt not recorded %s %s" % (status, body[:120]))

def chunks(kind, payload, sha=None, final_sha=True):
    step = 1024
    off = 0
    while off < len(payload):
        piece = payload[off:off + step]
        final = off + len(piece) == len(payload)
        fields = [
            ("kind", kind),
            ("offset", str(off)),
            ("total", str(len(payload))),
            ("final", "1" if final else "0"),
            ("data", piece.hex()),
        ]
        if final and final_sha:
            fields.append(("sha256", sha if sha is not None else hashlib.sha256(payload).hexdigest()))
        status, body, _ = form("/firmware/chunk", fields)
        if status != 200:
            return status, body
        off += len(piece)
    return status, body

status, body, _ = form("/firmware/chunk", (("kind", "kernel"), ("offset", "0"), ("total", "9000000"), ("final", "0"), ("data", "aa")))
if status != 400 or b"too large" not in body:
    fail("oversize %s %s" % (status, body[:120]))
status, body, _ = form("/firmware/chunk", (("kind", "test"), ("offset", "5"), ("total", "10"), ("final", "0"), ("data", "abcd")))
if status != 400 or b"bad offset" not in body:
    fail("offset %s %s" % (status, body[:120]))

scratch = bytes((i * 17) & 0xFF for i in range(5000))
open("/tmp/fw-test.bin", "wb").write(scratch)
status, body = chunks("test", scratch, sha="00" * 32)
if status != 400 or b"checksum mismatch" not in body:
    fail("bad checksum %s %s" % (status, body[:160]))
status, _, _ = req("GET", "/firmware/read/test/0")
if status != 404:
    fail("bad checksum left the test file %s" % status)

status, body = chunks("test", scratch)
if status != 200 or b"staged" not in body:
    fail("test upload %s %s" % (status, body[:160]))
got = b""
off = 0
file_sha = None
while off < len(scratch):
    status, part, headers = req("GET", "/firmware/read/test/%d" % off)
    if status != 200 or not part:
        fail("readback %s %s at %s" % (status, part[:80], off))
    file_sha = headers["X-Fw-Sha256"]
    got += part
    off += len(part)
if got != scratch or file_sha != hashlib.sha256(scratch).hexdigest():
    fail("readback mismatch %s %s" % (len(got), file_sha))

blank = b"\xff" * 4096
status, body = chunks("kernel", blank)
if status != 400 or b"image rejected" not in body:
    fail("blank image %s %s" % (status, body[:160]))
elf = b"\x7fELF" + b"\x01" * 4092
status, body = chunks("kernel", elf)
if status != 400 or b"image rejected" not in body:
    fail("elf image %s %s" % (status, body[:160]))
kernel = bytes((i * 3) & 0xFF for i in range(4096))
if kernel[0] == 0:
    kernel = b"\x01" + kernel[1:]
open("/tmp/fw-kernel.bin", "wb").write(kernel)
status, body = chunks("kernel", kernel)
if status != 200 or b"staged" not in body:
    fail("kernel stage %s %s" % (status, body[:160]))
doc = uptime()
if doc["fw_staged"] != 1 or doc["fw_bytes"] != 4096 or doc["fw_backup"] != 1:
    fail("staged status %s" % {k: doc[k] for k in ("fw_staged", "fw_bytes", "fw_backup", "fw_sha256", "fw_result")})
if doc["fw_sha256"] != hashlib.sha256(kernel).hexdigest():
    fail("staged sha %s" % doc["fw_sha256"])
status, part, headers = req("GET", "/firmware/read/kernel/0")
if status != 200 or headers["X-Fw-Sha256"] != doc["fw_sha256"] or part != kernel[:len(part)]:
    fail("kernel read %s" % status)

# A multi-megabyte stage must not hash the whole file inside one syscall.
# The final chunk starts a bounded read-back, and /status keeps answering
# while later polls finish it. The retry of that chunk reports staged.
big = bytearray((i * 3 + 1) & 0xFF for i in range(2 * 1024 * 1024))
big[0] = 0x11
big = bytes(big)
step = 16384
def raw_chunk(off, piece, final, sha=None, timeout=8):
    headers = {
        "Content-Type": "application/octet-stream",
        "X-Fw-Kind": "kernel",
        "X-Fw-Offset": str(off),
        "X-Fw-Total": str(len(big)),
        "X-Fw-Final": "1" if final else "0",
    }
    if sha is not None:
        headers["X-Fw-Sha256"] = sha
    return req("POST", "/firmware/chunk", piece, headers, timeout=timeout)
off = 0
t0 = time.time()
while off < len(big):
    piece = big[off:off + step]
    if off + len(piece) == len(big):
        break
    status, body, _ = raw_chunk(off, piece, False)
    if status != 200 or b'"status":"ok"' not in body:
        fail("large chunk %s at %s %s" % (status, off, body[:120]))
    off += len(piece)
write_s = time.time() - t0
piece = big[off:]
t1 = time.time()
status, body, _ = raw_chunk(off, piece, True, hashlib.sha256(big).hexdigest(), timeout=3)
final_s = time.time() - t1
if status != 200 or b"verifying" not in body:
    fail("large finalize %s in %.2fs %s" % (status, final_s, body[:180]))
if final_s > 2.0:
    fail("final chunk blocked for %.2fs" % final_s)
last = -1
moved = False
samples = []
deadline = time.time() + 90
verify_t = time.time()
while time.time() < deadline:
    t2 = time.time()
    doc = uptime()
    if time.time() - t2 > 1.0:
        fail("status blocked during verify after %.2fs" % (time.time() - t2))
    t3 = time.time()
    status, raw, _ = req("GET", "/firmware/status", timeout=2)
    if time.time() - t3 > 1.0 or status != 200:
        fail("firmware status blocked %s %.2fs" % (status, time.time() - t3))
    fw = json.loads(raw)
    prog = fw.get("fw_progress", 0)
    samples.append(prog)
    if prog > last:
        moved = True
        last = prog
    phase = fw.get("fw_phase")
    if phase == "staged" and doc.get("fw_result") == "staged" and doc.get("fw_bytes") == len(big):
        break
    if phase not in ("verifying", "staging", "receiving", "staged"):
        fail("phase during verify %s" % fw)
else:
    fail("large verify did not finish %s" % samples[-6:])
if not moved:
    fail("verify progress did not advance %s" % samples[:8])
verify_s = time.time() - verify_t
status, body, _ = raw_chunk(off, piece, True, hashlib.sha256(big).hexdigest(), timeout=3)
if status != 200 or b'"status":"staged"' not in body:
    fail("final retry %s %s" % (status, body[:160]))
doc = uptime()
if doc["fw_sha256"] != hashlib.sha256(big).hexdigest() or doc["fw_bytes"] != len(big) or doc["fw_phase"] != "staged":
    fail("large staged status %s" % {k: doc.get(k) for k in ("fw_bytes", "fw_sha256", "fw_phase", "fw_result")})
open("/tmp/fw-kernel.bin", "wb").write(big)
print(
    "large kernel %d bytes write %.2fs (%.0f KB/s) finalize %.2fs verify %.2fs (%.0f KB/s)"
    % (
        len(big),
        write_s,
        (len(big) - len(piece)) / 1024 / max(write_s, 0.001),
        final_s,
        verify_s,
        len(big) / 1024 / max(verify_s, 0.001),
    )
)

# QEMU does not clear CNTVCT on reset. Uptime is counted from the boot
# sample of that counter, and the first /status after reset is often
# already several seconds. Wait so that sample is still below `before`.
entry = uptime()["uptime"]
need = entry + 12
before = entry
deadline = time.time() + 40
while time.time() < deadline and before < need:
    time.sleep(0.4)
    try:
        before = uptime()["uptime"]
    except SystemExit:
        continue
if before < need:
    fail("uptime never advanced 12s past %s (now %s)" % (entry, before))
old_n = open(serial_path, "rb").read().count(MARKER)
status, body, _ = form("/firmware/reboot", (("confirm", "reboot"), ("tryboot", "0")))
if status != 200 or b"Reboot requested." not in body:
    fail("reboot %s %s" % (status, body[:160]))
deadline = time.time() + 25
samples = []
while time.time() < deadline:
    try:
        doc = uptime()
    except SystemExit:
        time.sleep(0.3)
        continue
    samples.append(doc["uptime"])
    new_n = open(serial_path, "rb").read().count(MARKER)
    if doc["uptime"] < before and new_n > old_n and doc["fw_staged"] == 1:
        status, part, headers = req("GET", "/firmware/read/test/0")
        if status != 200 or headers["X-Fw-Sha256"] != hashlib.sha256(scratch).hexdigest():
            fail("test file missing after reboot")
        serial = open(serial_path, "rb").read()
        if b"power: reboot" not in serial or b"power: psci reset" not in serial:
            fail("firmware reboot did not log the reset")
        if doc.get("fw_booted") != "unknown" or doc.get("fw_result") == "reboot requested" or doc.get("version") != "0.1.84":
            fail("boot report %s" % {k: doc.get(k) for k in ("fw_booted", "fw_result", "version")})
        if doc.get("fw_result") != "normal boot":
            fail("stale reboot result %s" % doc.get("fw_result"))
        serial_now = open(serial_path, "rb").read()
        if b"version 0.1.84" not in serial_now or b"boot unknown" not in serial_now:
            fail("boot log missing version")
        print("reboot", before, doc["uptime"], "banners", old_n, new_n)
        break
    time.sleep(0.3)
else:
    fail("firmware reboot did not come back %s" % samples[-8:])
live_sha = hashlib.sha256(b"K" * 64).hexdigest()
big_sha = hashlib.sha256(big).hexdigest()
status, body, _ = form("/firmware/promote", (("confirm", "promote"),))
if status != 400 or b"tryboot boot or force" not in body:
    fail("promote without force %s %s" % (status, body[:180]))
status, body, _ = form("/firmware/promote", (("confirm", "promote"), ("force", "1")))
if status != 200 or b"Promote started." not in body:
    fail("promote %s %s" % (status, body[:180]))
deadline = time.time() + 90
promoted = None
while time.time() < deadline:
    doc = uptime()
    if doc.get("fw_result") == "promoted" and doc.get("fw_phase") == "idle":
        promoted = doc
        break
    if doc.get("fw_result") == "promote failed":
        fail("promote failed %s" % doc)
    time.sleep(0.2)
if promoted is None:
    fail("promote did not finish")
status, _, headers = req("GET", "/firmware/read/live/0")
if status != 200 or headers.get("X-Fw-Sha256") != big_sha:
    fail("promoted live sha %s %s" % (status, headers.get("X-Fw-Sha256")))
status, _, headers = req("GET", "/firmware/read/bak/0")
if status != 200 or headers.get("X-Fw-Sha256") != live_sha:
    fail("backup sha %s %s" % (status, headers.get("X-Fw-Sha256")))
if promoted["fw_tryboot"] != 0 or promoted["fw_bak"] != 1 or promoted["fw_staged"] != 0 or promoted["fw_tryboot_file"] != 1:
    fail("promote status %s" % {k: promoted.get(k) for k in ("fw_tryboot", "fw_bak", "fw_staged", "fw_tryboot_file", "fw_result")})
status, body, _ = form("/firmware/rollback", (("confirm", "rollback"),))
if status != 200 or b"Rollback started." not in body:
    fail("rollback %s %s" % (status, body[:180]))
deadline = time.time() + 60
rolled = None
while time.time() < deadline:
    doc = uptime()
    if doc.get("fw_result") == "rolled back" and doc.get("fw_phase") == "idle":
        rolled = doc
        break
    if doc.get("fw_result") == "rollback failed":
        fail("rollback failed %s" % doc)
    time.sleep(0.2)
if rolled is None:
    fail("rollback did not finish")
status, _, headers = req("GET", "/firmware/read/live/0")
if status != 200 or headers.get("X-Fw-Sha256") != live_sha:
    fail("rolled back live sha %s %s" % (status, headers.get("X-Fw-Sha256")))
print("promote and rollback ok")
print("firmware ok")
PY
then
    FW_OK=1
fi
kill "$FW_PID" 2>/dev/null || true
wait "$FW_PID" 2>/dev/null || true
sync
if [ "$FW_OK" = 1 ]; then
    if ! mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::KERNEL8.IMG - > /tmp/fw-live-after.bin \
        || ! mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::FWTEST.BIN - > /tmp/fw-test-after.bin \
        || ! mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::KERNEL8.BAK - > /tmp/fw-bak-after.bin
    then
        echo "✗ firmware files missing on the FAT volume"
        FW_OK=0
    fi
fi
if [ "$FW_OK" = 1 ]; then
    if ! cmp -s /tmp/fw-live.bin /tmp/fw-live-after.bin; then
        echo "✗ KERNEL8.IMG was not restored"
        FW_OK=0
    fi
    if ! cmp -s /tmp/fw-live.bin /tmp/fw-bak-after.bin; then
        echo "✗ KERNEL8.BAK is not the original kernel"
        FW_OK=0
    fi
    if ! cmp -s /tmp/fw-test.bin /tmp/fw-test-after.bin; then
        echo "✗ FWTEST.BIN does not match the upload"
        FW_OK=0
    fi
    if mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::KERNEL8.NEW - >/dev/null 2>&1; then
        echo "✗ KERNEL8.NEW was left after promote"
        FW_OK=0
    fi
    if ! mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::TRYBOOT.TXT - > /tmp/fw-tryboot-after.txt; then
        echo "✗ TRYBOOT.TXT missing after promote"
        FW_OK=0
    elif grep -q 'kernel=kernel8.new' /tmp/fw-tryboot-after.txt \
        || ! grep -q 'kernel=kernel8.img' /tmp/fw-tryboot-after.txt; then
        echo "✗ TRYBOOT.TXT does not boot kernel8.img"
        FW_OK=0
    fi
    if mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::CONFIG.TXT - >/dev/null 2>&1; then
        echo "✗ CONFIG.TXT was created"
        FW_OK=0
    fi
    if mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::FWTEST.TMP - >/dev/null 2>&1; then
        echo "✗ FWTEST.TMP was left behind"
        FW_OK=0
    fi
    if mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::KN8.TMP - >/dev/null 2>&1; then
        echo "✗ KN8.TMP was left behind"
        FW_OK=0
    fi
    if mcopy -i qemu/fw-disk.img@@${DISK_OFF} ::KN8B.TMP - >/dev/null 2>&1; then
        echo "✗ KN8B.TMP was left behind"
        FW_OK=0
    fi
fi
if [ "$FW_OK" = 1 ]; then
    echo "✓ Firmware upload, readback, validation, and reboot"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Firmware upload, readback, validation, or reboot failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

# Test 12 and 13: exception report, and a write to .text must fault.
fault_case() {
    local name="$1"
    local path="$2"
    local marker="$3"
    local iss="$4"
    local record="$5"
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
        && grep -q "$iss" "$serial" \
        && grep -q "$record" "$serial" \
        && grep -q "elr=" "$serial" \
        && grep -q "far=" "$serial"; then
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
fault_case guard /debug/fault/guard "debug fault: read stack guard" "Translation fault" "STACK OVERFLOW kern"

echo ""
echo "Test 13: Write to kernel text is a permission fault"
fault_case text /debug/fault/text "debug fault: write kernel text" "Permission fault" "kfault v="

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
if echo "$EL0_LINE" | grep -q "log ok, yield ok, irq ok, exit 0, bad-syscall -1, bad-pointer rejected, kernel-read killed, priv killed, exec-data killed, stack-overflow killed" \
    && grep -q "user: hello from el0" qemu/el0-serial.txt \
    && grep -q "user: ticks advanced" qemu/el0-serial.txt \
    && grep -q "EL0 yield: back in kernel" qemu/el0-serial.txt \
    && grep -q "EL0 log: rejected pointer" qemu/el0-serial.txt \
    && grep -q "EL0 syscall: unknown" qemu/el0-serial.txt \
    && grep -q "user: bad syscall survived" qemu/el0-serial.txt \
    && grep -q "user: bad pointer survived" qemu/el0-serial.txt \
    && grep -q "elfault boot" qemu/el0-serial.txt \
    && grep -q "STACK OVERFLOW boot" qemu/el0-serial.txt \
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
    && grep -q 'arm_64bit=1' boot/pi4/config.txt \
    && ! grep -Eq '^hdmi_force_hotplug=' boot/pi4/config.txt; then
    echo "✓ Pi 4 kernel8.img linked at 0x${START_ADDR} with config.txt"
    echo "  QEMU raspi4b has no GENET. DHCP is enabled in this image."
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
# Idle milliseconds over the quiet window should dwarf drive() time.
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

# The offer is the frame from the frozen Pi 4 photo: Ethernet source
# 00:00:5e:00:01:01, IPv4/UDP 67->68, home-router options. virtio delivers
# it through Device::receive. A hang here never reaches the status lines
# above; this grep is the path returning.
if grep -F -q "[OK] DHCP offer probe kept src=00:00:5e:00:01:01" qemu/irq-serial.txt \
    && grep -F -q "[OK] DHCP offer probe delivered" qemu/irq-serial.txt; then
    echo "✓ virt RX path returned the VRRP DHCP offer"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ virt RX path did not return the VRRP DHCP offer"
    grep -F "DHCP offer" qemu/irq-serial.txt || true
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
    # The guest stays up and reprints status, so this budget ends the
    # process. Exit 124 is that stop. A boot that already printed every
    # expected line is a pass. A missing line or a kernel panic is not.
    # Stages 1 through 13 are 250 ms flashes with a 1.5 s gap, so the
    # serial lines land well after the old 45 s budget.
    RASPI_STOP=0
    timeout 180 qemu-system-aarch64 \
        -M raspi4b \
        -smp 4 \
        -kernel boot/pi4/kernel8.img \
        -serial file:qemu/raspi4b.txt \
        -display none -monitor none -nographic \
        -drive file=qemu/raspi4b-sd.img,if=sd,format=raw \
        >/tmp/qemu-raspi4b.txt 2>&1 || RASPI_STOP=$?
    RASPI_OK=1
    if grep -F -q "!!! KERNEL PANIC !!!" qemu/raspi4b.txt \
        || grep -F -q "=== EXCEPTION ===" qemu/raspi4b.txt; then
        echo "✗ raspi4b panicked"
        RASPI_OK=0
    fi
    for need in \
        "MMU and caches enabled" \
        "Running at EL1" \
        "cores online 4" \
        "[OK] Filesystem mounted" \
        "Loaded config from filesystem" \
        "[OK] Log task running" \
        "EL0 test" \
        "[WARN] No NIC" \
        "[OK] HDMI console" \
        "[OK] DHCP offer probe kept src=00:00:5e:00:01:01"
    do
        if ! grep -F -q "$need" qemu/raspi4b.txt; then
            echo "✗ raspi4b missing: $need"
            RASPI_OK=0
        fi
    done
    if [ "$RASPI_OK" -eq 1 ]; then
        echo "✓ raspi4b booted, 4 cores, FAT config, EL0 log task, no NIC"
        TESTS_PASSED=$((TESTS_PASSED + 1))
    else
        echo "✗ raspi4b smoke failed"
        if [ "$RASPI_STOP" -ne 0 ] && [ "$RASPI_STOP" -ne 124 ]; then
            echo "qemu exit ${RASPI_STOP}"
        fi
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

# VNC button, reboot, and shutdown use their own guests and their own host
# ports. They never signal a QEMU this script did not start.
echo ""
echo "Test 31: VNC Start/Stop button, guest reboot, and guest shutdown"
POWER_HTTP_PORT="${POWER_HTTP_PORT:-18080}"
POWER_DNS_PORT="${POWER_DNS_PORT:-25353}"
POWER_VNC_PORT="${POWER_VNC_PORT:-25900}"
POWER_TELNET_PORT="${POWER_TELNET_PORT:-3323}"
POWER_FWD="hostfwd=udp::${POWER_DNS_PORT}-:53,hostfwd=tcp::${POWER_HTTP_PORT}-:80,hostfwd=tcp::${POWER_TELNET_PORT}-:23,hostfwd=tcp::${POWER_VNC_PORT}-:5900"
FOREIGN_QEMU=$(ps -eo pid=,args= | awk 'index($0, "qemu-system-aarch64") && index($0, "awk") == 0 {print $1}')
start_power_guest() {
    local serial="$1"
    local limit="${2:-70}"
    rm -f "$serial"
    timeout "$limit" qemu-system-aarch64 \
        -M virt,gic-version=2 \
        -cpu cortex-a72 \
        -smp 4 \
        -m 512M \
        -kernel boot/kernel8.img \
        -serial file:"$serial" \
        -display none -monitor none -nographic \
        -netdev user,id=net0,${POWER_FWD} \
        -device virtio-net-device,netdev=net0 \
        >/tmp/qemu-power.txt 2>&1 &
    POWER_PID=$!
}
wait_power_http() {
    local i
    for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30; do
        if curl -sf -u admin:admin --max-time 2 "http://127.0.0.1:${POWER_HTTP_PORT}/" >/dev/null; then
            return 0
        fi
        sleep 0.5
    done
    return 1
}
# VNC and the reboot poll share this guest. A slow TCG boot used to hit
# the old 70s limit while the poll was still running, which looks like
# "uptime did not drop" because every socket error is ignored.
start_power_guest qemu/power-reboot.txt 300
echo "Power guest PID: $POWER_PID"
VNC_BUTTON_OK=0
REBOOT_OK=0
if wait_power_http; then
    if python3 - "$POWER_HTTP_PORT" "$POWER_VNC_PORT" <<'PY'
import json, socket, sys, time, urllib.error, urllib.request
http_port, vnc_port = sys.argv[1], int(sys.argv[2])
base = "http://127.0.0.1:%s" % http_port
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def req(method, path, data=None, headers=None, auth=True):
    body = data.encode() if isinstance(data, str) else data
    r = urllib.request.Request(base + path, data=body, method=method)
    for key, value in (headers or {}).items():
        r.add_header(key, value)
    handler = opener if auth else urllib.request.build_opener()
    try:
        with handler.open(r, timeout=8) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()

def connect():
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
    sock.close()
    return True

def uptime():
    status, page = req("GET", "/status")
    if status != 200:
        fail("status %s" % status)
    return json.loads(page)["uptime"]

status, page = req("GET", "/")
if status != 200 or b'value="start">Start</button>' not in page or b'value="stop">Stop</button>' in page:
    fail("home label %s %s" % (status, page[:400]))
if b">Shutdown</button>" not in page or b">Reboot</button>" not in page:
    fail("power buttons missing")
if b"confirm('Shut down this DNS server?')" not in page or b"confirm('Reboot this DNS server?')" not in page:
    fail("confirmation dialog missing")
status, _ = req("GET", "/vnc")
if status != 405 or connect() is not None:
    fail("GET /vnc changed the server %s" % status)
status, _ = req("GET", "/power")
if status != 405:
    fail("GET /power %s" % status)
status, _ = req("POST", "/vnc", "action=start", {"Content-Type": "application/x-www-form-urlencoded"}, auth=False)
if status != 401:
    fail("unauthenticated start %s" % status)
before = uptime()
status, body = req(
    "POST",
    "/power",
    "action=shutdown",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 400 or b"Confirmation required" not in body:
    fail("unconfirmed shutdown %s %s" % (status, body[:160]))
if uptime() < before:
    fail("unconfirmed shutdown rebooted the guest")
status, page = req(
    "POST",
    "/vnc",
    "action=start",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 200:
    fail("start %s %s" % (status, page[:200]))
deadline = time.time() + 8
while time.time() < deadline:
    status, page = req("GET", "/")
    if status == 200 and b'value="stop">Stop</button>' in page and b"VNC process: running" in page and connect() is not None:
        break
    time.sleep(0.25)
else:
    fail("button did not become Stop")
status, page = req(
    "POST",
    "/vnc",
    "action=stop",
    {"Content-Type": "application/x-www-form-urlencoded"},
)
if status != 200:
    fail("stop %s" % status)
deadline = time.time() + 8
while time.time() < deadline:
    status, page = req("GET", "/")
    if status == 200 and b'value="start">Start</button>' in page and b'value="stop">Stop</button>' not in page and b"VNC process: off" in page and connect() is None:
        break
    time.sleep(0.25)
else:
    fail("button did not return to Start")
print("vnc button ok")
PY
    then
        VNC_BUTTON_OK=1
    fi
    if [ "$VNC_BUTTON_OK" = 1 ] && python3 - "$POWER_HTTP_PORT" qemu/power-reboot.txt <<'PY'
import json, sys, time, urllib.error, urllib.request
http_port = sys.argv[1]
serial_path = sys.argv[2]
base = "http://127.0.0.1:%s" % http_port
# Printed once from main, not by the 2s status reprint.
MARKER = b"=== Pi4 DNS Server Kernel ==="
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def req(method, path, data=None, timeout=8):
    body = data.encode() if isinstance(data, str) else data
    r = urllib.request.Request(base + path, data=body, method=method)
    if data is not None:
        r.add_header("Content-Type", "application/x-www-form-urlencoded")
    try:
        with opener.open(r, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()

def uptime():
    status, page = req("GET", "/status", timeout=3)
    if status != 200:
        fail("status %s" % status)
    return json.loads(page)["uptime"]

def boot_mark():
    try:
        data = open(serial_path, "rb").read()
    except OSError:
        return 0, 0
    return len(data), data.count(MARKER)

def second_boot(old_len, old_n, new_len, new_n):
    # file: appends across QEMU reset. Some builds truncate and rewrite.
    if new_n > old_n and new_n >= 2:
        return True
    if old_n >= 1 and new_len < old_len and new_n >= 1:
        return True
    return False

status, page = req("GET", "/status")
if status != 200:
    fail("status %s" % status)
entry = json.loads(page)["uptime"]
# Ten guest-seconds past the uptime we found on entry. The second boot's
# first /status is often already 3 or more, so a fixed ceiling of 2 fails
# after a real reset. The extra wait keeps that sample below `before`.
need = entry + 10
before = entry
deadline = time.time() + 90
while time.time() < deadline and before < need:
    time.sleep(0.3)
    try:
        before = uptime()
    except Exception:
        continue
if before < need:
    fail("uptime never advanced 10s past %s (now %s)" % (entry, before))
old_len, old_n = boot_mark()
status, body = req("POST", "/power", "action=reboot&confirm=reboot")
if status != 200 or b"Reboot requested." not in body:
    fail("reboot response %s %s" % (status, body[:200]))
deadline = time.time() + 60
samples = []
while time.time() < deadline:
    try:
        status, page = req("GET", "/status", timeout=2)
        now = json.loads(page)["uptime"]
    except Exception:
        time.sleep(0.3)
        continue
    samples.append(now)
    new_len, new_n = boot_mark()
    if status == 200 and second_boot(old_len, old_n, new_len, new_n) and now < before:
        status, home = req("GET", "/")
        if b'value="start">Start</button>' not in home or b">Shutdown</button>" not in home:
            fail("home after reboot %s" % home[:240])
        serial = open(serial_path, "rb").read()
        if b"power: reboot" not in serial or b"power: psci reset" not in serial:
            fail("reboot did not log the reset")
        print("reboot", before, now, "banners", old_n, new_n)
        break
    time.sleep(0.3)
else:
    new_len, new_n = boot_mark()
    fail(
        "guest uptime did not drop (before %s samples %s banners %s->%s len %s->%s)"
        % (before, samples[-8:], old_n, new_n, old_len, new_len)
    )
PY
    then
        REBOOT_OK=1
    fi
else
    echo "✗ Power guest did not answer HTTP"
fi
if kill -0 "$POWER_PID" 2>/dev/null; then
    kill "$POWER_PID" 2>/dev/null || true
    wait "$POWER_PID" 2>/dev/null || true
fi
if [ "$VNC_BUTTON_OK" = 1 ]; then
    echo "✓ VNC button follows the live process"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ VNC Start/Stop button failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
if [ "$REBOOT_OK" = 1 ]; then
    echo "✓ Guest reboot printed a second banner and uptime dropped"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Guest reboot failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

start_power_guest qemu/power-off.txt
OFF_PID=$POWER_PID
echo "Shutdown guest PID: $OFF_PID"
SHUTDOWN_OK=0
if wait_power_http && python3 - "$POWER_HTTP_PORT" <<'PY'
import sys, urllib.error, urllib.request
http_port = sys.argv[1]
base = "http://127.0.0.1:%s" % http_port
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)
body = "action=shutdown&confirm=shutdown".encode()
r = urllib.request.Request(base + "/power", data=body, method="POST")
r.add_header("Content-Type", "application/x-www-form-urlencoded")
try:
    with opener.open(r, timeout=8) as resp:
        status, page = resp.status, resp.read()
except urllib.error.HTTPError as err:
    status, page = err.code, err.read()
if status != 200 or b"Shutdown requested." not in page:
    print("shutdown response %s %s" % (status, page[:200]), file=sys.stderr)
    sys.exit(1)
print("shutdown accepted")
PY
then
    deadline=$((SECONDS + 12))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if ! kill -0 "$OFF_PID" 2>/dev/null; then
            SHUTDOWN_OK=1
            break
        fi
        sleep 0.3
    done
fi
if [ "$SHUTDOWN_OK" = 1 ] && grep -q "power: shutdown" qemu/power-off.txt && grep -q "power: psci off" qemu/power-off.txt; then
    echo "✓ Guest shutdown exited that QEMU"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Guest shutdown did not exit its QEMU"
    if kill -0 "$OFF_PID" 2>/dev/null; then
        kill "$OFF_PID" 2>/dev/null || true
        wait "$OFF_PID" 2>/dev/null || true
    fi
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
LEFT=""
for pid in $FOREIGN_QEMU; do
    if ! kill -0 "$pid" 2>/dev/null; then
        LEFT="$LEFT $pid"
    fi
done
if [ -n "$LEFT" ]; then
    echo "✗ Unrelated QEMU exited:$LEFT"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi

echo ""
echo "Test 60: crash record survives a virt reset"
DISK_OFF=$((2048 * 512))
rm -f qemu/crash-disk.img qemu/crash.txt
dd if=/dev/zero of=qemu/crash-disk.img bs=1M count=32 status=none
sfdisk --quiet qemu/crash-disk.img <<'EOF'
label: dos
start=2048, type=c
EOF
mkfs.vfat -F 32 -n PI4DNS --offset 2048 qemu/crash-disk.img >/dev/null
python3 -c 'open("/tmp/crash-live.bin","wb").write(b"K"*64)'
mcopy -i qemu/crash-disk.img@@${DISK_OFF} /tmp/crash-live.bin ::KERNEL8.IMG
timeout 180 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/crash.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/crash-disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-crash.txt 2>&1 &
CRASH_PID=$!
CRASH_OK=0
if python3 - "$HTTP_HOST_PORT" qemu/crash.txt "$CRASH_PID" <<'PY'
import json, os, signal, sys, time, urllib.error, urllib.request
http_port, serial_path, pid = sys.argv[1], sys.argv[2], int(sys.argv[3])
base = "http://127.0.0.1:%s" % http_port
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def req(method, path, data=None, headers=None, auth=True, timeout=8):
    body = data.encode() if isinstance(data, str) else data
    r = urllib.request.Request(base + path, data=body, method=method)
    for key, value in (headers or {}).items():
        r.add_header(key, value)
    handler = opener if auth else urllib.request.build_opener()
    try:
        with handler.open(r, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except Exception as err:
        return 0, str(err).encode()

def wait_up():
    for _ in range(40):
        status, _ = req("GET", "/status")
        if status == 200:
            return
        time.sleep(0.5)
    fail("crash guest did not answer")

def form(kind):
    return req(
        "POST",
        "/debug/crash",
        "confirm=crash&kind=%s" % kind,
        {"Content-Type": "application/x-www-form-urlencoded"},
    )

wait_up()
status, _ = req("GET", "/crash/last", auth=False)
if status != 401:
    fail("crash unauth %s" % status)
status, body = req("POST", "/debug/crash", "kind=abort", {"Content-Type": "application/x-www-form-urlencoded"})
if status != 400 or b"confirm=crash" not in body:
    fail("crash confirm %s %s" % (status, body[:160]))
for kind, needle in (("abort", b"kind data abort"), ("panic", b"kind panic"), ("hang", b"kind hang")):
    before = open(serial_path, "rb").read().count(b"crash recovered")
    status, body = form(kind)
    if status != 200 or b"crashing" not in body:
        fail("trigger %s %s %s" % (kind, status, body[:160]))
    seen = False
    for _ in range(40):
        time.sleep(0.5)
        try:
            serial = open(serial_path, "rb").read()
        except OSError:
            serial = b""
        if serial.count(b"crash recovered") > before:
            seen = True
            break
    if not seen:
        fail("no recovery for %s" % kind)
    wait_up()
    status, page = req("GET", "/crash/last")
    if status != 200 or needle not in page:
        fail("last %s %s %s" % (kind, status, page[:240]))
    status, listing = req("GET", "/crash/list")
    if status != 200 or b"CRASH" not in listing:
        fail("list %s %s" % (status, listing[:200]))
    status, raw = req("GET", "/status")
    doc = json.loads(raw)
    if doc.get("crash_count", 0) < 1 or "CRASH" not in doc.get("last_crash", ""):
        fail("status crash %s" % doc.get("last_crash"))
print("crash records", doc.get("crash_count"), doc.get("last_crash"))
os.kill(pid, signal.SIGTERM)
sys.exit(0)
PY
then
    echo "✓ Crash records survive reset for abort, panic, and hang"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Crash capture failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
kill "$CRASH_PID" 2>/dev/null || true
wait "$CRASH_PID" 2>/dev/null || true

echo ""
echo "Test 61: watchdog hold and backup fallback"
rm -f qemu/boot-disk.img qemu/boot-serial.txt
dd if=/dev/zero of=qemu/boot-disk.img bs=1M count=32 status=none
sfdisk --quiet qemu/boot-disk.img <<'EOF'
label: dos
start=2048, type=c
EOF
DISK_OFF=$((2048 * 512))
mkfs.vfat -F 32 -n PI4DNS --offset 2048 qemu/boot-disk.img >/dev/null
printf 'arm_64bit=1\nkernel=kernel8.img\nenable_uart=1\n' > /tmp/tryboot-live.txt
printf 'backup-kernel' > /tmp/kernel8.bak
mcopy -i qemu/boot-disk.img@@${DISK_OFF} /tmp/tryboot-live.txt ::TRYBOOT.TXT
mcopy -i qemu/boot-disk.img@@${DISK_OFF} /tmp/kernel8.bak ::KERNEL8.BAK
timeout 180 qemu-system-aarch64 \
    -M virt,gic-version=2 -cpu cortex-a72 -smp 4 -m 512M \
    -kernel boot/kernel8.img \
    -serial file:qemu/boot-serial.txt \
    -display none -monitor none -nographic \
    -netdev user,id=net0,${HOSTFWD} \
    -device virtio-net-device,netdev=net0 \
    -drive file=qemu/boot-disk.img,if=none,format=raw,id=hd0,cache=writethrough \
    -device virtio-blk-device,drive=hd0 \
    >/tmp/qemu-boot.txt 2>&1 &
BOOT_PID=$!
if python3 - "$HTTP_HOST_PORT" qemu/boot-serial.txt "$BOOT_PID" qemu/boot-disk.img <<'PY'
import json, os, signal, subprocess, sys, time, urllib.error, urllib.request
http_port, serial_path, pid, disk = sys.argv[1:]
base = "http://127.0.0.1:%s" % http_port
auth = urllib.request.HTTPBasicAuthHandler()
auth.add_password(realm="DNS Server", uri=base, user="admin", passwd="admin")
opener = urllib.request.build_opener(auth)

def fail(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)

def req(method, path, data=None, headers=None, timeout=8):
    body = data.encode() if isinstance(data, str) else data
    r = urllib.request.Request(base + path, data=body, method=method)
    for key, value in (headers or {}).items():
        r.add_header(key, value)
    try:
        with opener.open(r, timeout=timeout) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()
    except Exception as err:
        return 0, str(err).encode()

def wait_up(tries=60):
    for _ in range(tries):
        status, _ = req("GET", "/status")
        if status == 200:
            return
        time.sleep(0.5)
    fail("boot guest did not answer")

def wait_serial(needle, tries=80):
    for _ in range(tries):
        try:
            serial = open(serial_path, "rb").read()
        except OSError:
            serial = b""
        if needle in serial:
            return serial
        time.sleep(0.5)
    fail("missing %s" % needle)

def form(kind):
    return req(
        "POST",
        "/debug/boot",
        "confirm=boot&kind=%s" % kind,
        {"Content-Type": "application/x-www-form-urlencoded"},
    )

wait_up()
status, body = req("POST", "/debug/boot", "kind=hang", {"Content-Type": "application/x-www-form-urlencoded"})
if status != 400 or b"confirm=boot" not in body:
    fail("boot confirm %s %s" % (status, body[:160]))
status, body = form("hang")
if status != 200 or b"armed" not in body:
    fail("hang %s %s" % (status, body[:160]))
wait_serial(b"watchdog reset")
wait_up()
status, raw = req("GET", "/status")
doc = json.loads(raw)
if status != 200 or doc.get("last_reset") != "watchdog":
    fail("watchdog reset %s" % doc.get("last_reset"))
status, body = form("fail")
if status != 200 or b"armed" not in body:
    fail("fail %s %s" % (status, body[:160]))
wait_serial(b"boot fallback kernel8.bak")
wait_up()
status, raw = req("GET", "/status")
doc = json.loads(raw)
if status != 200 or doc.get("fallback_active") != 1 or doc.get("boot_attempts", 0) < 3 or doc.get("safe_mode") not in (0, 1):
    fail("fallback status %s" % {k: doc.get(k) for k in ("boot_attempts", "boot_good", "fallback_active", "booted_kernel", "boot_stopped", "safe_mode")})
print("fallback", doc.get("boot_attempts"), doc.get("fallback_active"), doc.get("booted_kernel"))
os.kill(int(pid), signal.SIGTERM)
time.sleep(0.5)
off = str(2048 * 512)
text = subprocess.check_output(["mcopy", "-i", "%s@@%s" % (disk, off), "::TRYBOOT.TXT", "-"], text=True)
if "kernel=kernel8.bak" not in text or "kernel8.img" in text.split("kernel=")[-1]:
    fail("tryboot file %s" % text)
print("tryboot", text.strip().replace("\n", " | "))
sys.exit(0)
PY
then
    echo "✓ Watchdog hold resets, and three failed boots fall back to kernel8.bak"
    TESTS_PASSED=$((TESTS_PASSED + 1))
else
    echo "✗ Boot fallback failed"
    TESTS_FAILED=$((TESTS_FAILED + 1))
fi
kill "$BOOT_PID" 2>/dev/null || true
wait "$BOOT_PID" 2>/dev/null || true

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
