#!/usr/bin/env python3
"""Random DNS query stress test for the pi4-dns-kernel device.

Standard library only. Sends a mix of local-record, real-domain (upstream
forward + cache) and random-name queries over UDP at a target rate for a set
duration, prints stats every 10 s, and watches /status for reboots.

The Pi command, from the machine that can reach the board:

    python3 scripts/dns_stress.py --server 172.18.3.137 --duration 3600 --qps 50

A short check:

    python3 scripts/dns_stress.py --server 172.18.3.137 --duration 60 --qps 50

`--fail-local-loss 5` exits 1 when more than 5% of local names are lost.
`--require-counters` exits 1 unless /status has rx_drops, rx_overruns,
rx_queued, forward_full, reply_drops, and dns_inflight (the forward queue
depth). QEMU's integration run uses both.
"""
import argparse, base64, json, os, random, select, socket, string, struct, sys, time, urllib.request

QTYPES = {"A": 1, "NS": 2, "CNAME": 5, "MX": 15, "TXT": 16, "AAAA": 28, "ANY": 255}
LOCAL = ["test.local", "pi4.local", "www.local"]
REAL = ["google.com", "cloudflare.com", "github.com", "wikipedia.org", "amazon.com",
        "example.com", "debian.org", "rust-lang.org", "raspberrypi.com", "apple.com",
        "microsoft.com", "netflix.com", "reddit.com", "x.com", "kernel.org"]
RCODES = {0: "NOERROR", 1: "FORMERR", 2: "SERVFAIL", 3: "NXDOMAIN", 4: "NOTIMP", 5: "REFUSED"}
COUNTERS = ("rx_drops", "rx_overruns", "rx_queued", "forward_full", "reply_drops", "dns_inflight")

def rand_label(lo=3, hi=12):
    return "".join(random.choice(string.ascii_lowercase + string.digits) for _ in range(random.randint(lo, hi)))

def pick_name():
    r = random.random()
    if r < 0.30:
        return random.choice(LOCAL), "local"
    if r < 0.60:
        return random.choice(REAL), "real"
    if r < 0.80:
        return f"{rand_label()}.{random.choice(REAL)}", "rand-sub"
    if r < 0.92:
        return f"{rand_label()}.local", "rand-local"
    labels = [rand_label(1, 63) for _ in range(random.randint(2, 4))]
    return ".".join(labels) + ".com", "long"

def build_query(qid, name, qtype):
    hdr = struct.pack(">HHHHHH", qid, 0x0100, 1, 0, 0, 0)
    q = b"".join(bytes([len(p)]) + p.encode() for p in name.split(".") if p) + b"\x00"
    return hdr + q + struct.pack(">HH", qtype, 1)

def fuzz_packet():
    kind = random.random()
    if kind < 0.3:
        return os.urandom(random.randint(1, 11))
    if kind < 0.6:
        return os.urandom(random.randint(12, 512))
    q = bytearray(build_query(random.randint(0, 65535), "fuzz.example.com", 1))
    for _ in range(random.randint(1, 4)):
        q[random.randrange(len(q))] = random.randint(0, 255)
    return bytes(q)

def status(server, port, user, pw):
    req = urllib.request.Request(f"http://{server}:{port}/status")
    req.add_header("Authorization", "Basic " + base64.b64encode(f"{user}:{pw}".encode()).decode())
    with urllib.request.urlopen(req, timeout=5) as r:
        return json.loads(r.read())

def counter_text(st):
    parts = []
    for key in COUNTERS:
        if key in st:
            parts.append(f"{key}={st.get(key)}")
    return " ".join(parts) if parts else "counters absent"

def pct(vals, p):
    if not vals:
        return 0.0
    s = sorted(vals)
    return s[min(len(s) - 1, int(len(s) * p))]

def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--server", default="172.18.3.137")
    ap.add_argument("--port", type=int, default=53)
    ap.add_argument("--duration", type=float, default=3600, help="seconds (default 3600 = 1 hour)")
    ap.add_argument("--qps", type=float, default=50, help="target queries per second")
    ap.add_argument("--timeout", type=float, default=2.0, help="seconds before a query counts as lost")
    ap.add_argument("--fuzz", type=float, default=0.0, help="fraction of malformed packets, e.g. 0.02")
    ap.add_argument("--http-port", type=int, default=80)
    ap.add_argument("--user", default="admin")
    ap.add_argument("--password", default="admin")
    ap.add_argument("--interval", type=float, default=10, help="stats print interval, seconds")
    ap.add_argument("--seed", type=int)
    ap.add_argument("--a-only", action="store_true",
                    help="send only A queries, so local names hit the zone")
    ap.add_argument("--fail-local-loss", type=float, default=None,
                    help="exit 1 when local-name loss exceeds this percent")
    ap.add_argument("--require-counters", action="store_true",
                    help="exit 1 unless /status publishes the load counters")
    ap.add_argument("--max-p99", type=float, default=None,
                    help="exit 1 when overall p99 latency exceeds this many milliseconds")
    ap.add_argument("--max-busy-ratio", type=float, default=None,
                    help="exit 1 when net_busy_ms grows by more than this fraction of wall time")
    ap.add_argument("--irq-moves", action="store_true",
                    help="exit 1 unless nic_irqs increases through the second half of the run")
    a = ap.parse_args()
    if a.seed is not None:
        random.seed(a.seed)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.setblocking(False)
    dest = (a.server, a.port)
    pending = {}
    tot = {"sent": 0, "ok": 0, "lost": 0, "fuzz": 0, "bad": 0}
    rc = {}
    cats = {}
    lat_all, lat_win = [], []
    win = {"sent": 0, "ok": 0, "lost": 0}

    def log(msg):
        print(time.strftime("%H:%M:%S ") + msg, flush=True)

    try:
        st = status(a.server, a.http_port, a.user, a.password)
        last_uptime, last_uptime_t = st.get("uptime", 0), time.time()
        log(f"start: device status={st.get('status')} uptime={last_uptime}s; {a.qps} qps for {a.duration:.0f}s")
        log("start: " + counter_text(st))
        start_irqs = st.get("nic_irqs")
        start_busy = st.get("net_busy_ms")
    except Exception as e:
        last_uptime, last_uptime_t = None, time.time()
        start_irqs = None
        start_busy = None
        log(f"start: /status unreachable ({e}); continuing with DNS only")

    start = time.time()
    end = start + a.duration
    next_send = start
    next_report = start + a.interval
    next_status = start + 60
    mid_at = start + a.duration / 2
    mid_irqs = None
    reboots = 0
    gap = 1.0 / a.qps

    try:
        while True:
            now = time.time()
            if now >= end and not pending:
                break
            while now < end and next_send <= now:
                if a.fuzz and random.random() < a.fuzz:
                    try:
                        sock.sendto(fuzz_packet(), dest); tot["fuzz"] += 1
                    except OSError:
                        pass
                else:
                    qid = random.randint(0, 65535)
                    while qid in pending:
                        qid = random.randint(0, 65535)
                    name, cat = pick_name()
                    qt = QTYPES["A"] if a.a_only or random.random() >= 0.3 else random.choice(list(QTYPES.values()))
                    try:
                        sock.sendto(build_query(qid, name, qt), dest)
                        pending[qid] = (now, cat)
                        tot["sent"] += 1; win["sent"] += 1
                        cats.setdefault(cat, [0, 0])[0] += 1
                    except OSError:
                        tot["bad"] += 1
                next_send += gap
            r, _, _ = select.select([sock], [], [], 0.005)
            while r:
                try:
                    data, _ = sock.recvfrom(4096)
                except BlockingIOError:
                    break
                except OSError:
                    break
                if len(data) >= 12:
                    qid, flags = struct.unpack(">HH", data[:4])
                    if qid in pending:
                        t0, cat = pending.pop(qid)
                        ms = (time.time() - t0) * 1000
                        lat_all.append(ms); lat_win.append(ms)
                        tot["ok"] += 1; win["ok"] += 1
                        cats[cat][1] += 1
                        name = RCODES.get(flags & 0xF, str(flags & 0xF))
                        rc[name] = rc.get(name, 0) + 1
                r, _, _ = select.select([sock], [], [], 0)
            now = time.time()
            for qid in [q for q, (t0, _) in pending.items() if now - t0 > a.timeout]:
                pending.pop(qid); tot["lost"] += 1; win["lost"] += 1
            if a.irq_moves and mid_irqs is None and now >= mid_at:
                try:
                    mid_irqs = status(a.server, a.http_port, a.user, a.password).get("nic_irqs")
                    log(f"mid: nic_irqs={mid_irqs}")
                except Exception as e:
                    log(f"mid: /status unreachable ({e})")
            if now >= next_status:
                next_status = now + 60
                try:
                    st = status(a.server, a.http_port, a.user, a.password)
                    up = st.get("uptime", 0)
                    if last_uptime is not None and up + 5 < last_uptime + (now - last_uptime_t) - 60:
                        reboots += 1
                        log(f"!!! device uptime dropped: was {last_uptime}s, now {up}s (reboot/restart)")
                    last_uptime, last_uptime_t = up, now
                    if st.get("status") != "running":
                        log(f"!!! device status={st.get('status')}")
                    log("status: " + counter_text(st))
                except Exception as e:
                    log(f"!!! /status unreachable: {e}")
            if now >= next_report:
                next_report += a.interval
                lossp = 100.0 * win["lost"] / max(1, win["sent"])
                log(f"sent={win['sent']} ok={win['ok']} lost={win['lost']} ({lossp:.1f}%) "
                    f"p50={pct(lat_win,.5):.1f}ms p99={pct(lat_win,.99):.1f}ms inflight={len(pending)} total={tot['sent']}")
                win = {"sent": 0, "ok": 0, "lost": 0}; lat_win = []
    except KeyboardInterrupt:
        log("interrupted")

    el = time.time() - start
    log("==== summary ====")
    log(f"duration {el:.0f}s, sent {tot['sent']} ({tot['sent']/max(el,1):.1f} qps), answered {tot['ok']}, "
        f"lost {tot['lost']} ({100.0*tot['lost']/max(1,tot['sent']):.2f}%), fuzz {tot['fuzz']}, send errors {tot['bad']}")
    log(f"latency p50={pct(lat_all,.5):.1f}ms p90={pct(lat_all,.9):.1f}ms p99={pct(lat_all,.99):.1f}ms max={max(lat_all) if lat_all else 0:.1f}ms")
    log("rcodes " + ", ".join(f"{k}={v}" for k, v in sorted(rc.items())))
    log("by type " + ", ".join(f"{k} {v[1]}/{v[0]}" for k, v in sorted(cats.items())))
    log(f"device reboots/restarts seen: {reboots}")
    end_status = None
    try:
        end_status = status(a.server, a.http_port, a.user, a.password)
        log(f"end: device status={end_status.get('status')} uptime={end_status.get('uptime')}s cache_entries={end_status.get('cache_entries')}")
        log("end: " + counter_text(end_status))
    except Exception as e:
        log(f"end: /status unreachable ({e})")
        return 1
    local = cats.get("local", [0, 0])
    local_loss = 100.0 * (local[0] - local[1]) / local[0] if local[0] else 100.0
    if a.fail_local_loss is not None:
        if local[0] == 0 or local_loss > a.fail_local_loss:
            log(f"local loss {local_loss:.2f}% exceeds {a.fail_local_loss}% ({local[1]}/{local[0]})")
            return 1
    if a.require_counters:
        missing = [key for key in COUNTERS if end_status is None or key not in end_status]
        if missing:
            log("missing counters " + " ".join(missing))
            return 1
    if a.max_p99 is not None:
        p99 = pct(lat_all, 0.99)
        if p99 > a.max_p99:
            log(f"p99 {p99:.1f}ms exceeds {a.max_p99:.0f}ms")
            return 1
    if a.irq_moves:
        end_irqs = None if end_status is None else end_status.get("nic_irqs")
        if not isinstance(start_irqs, int) or not isinstance(mid_irqs, int) or not isinstance(end_irqs, int):
            log(f"nic_irqs samples start={start_irqs} mid={mid_irqs} end={end_irqs}")
            return 1
        if not (end_irqs > mid_irqs >= start_irqs):
            log(f"nic_irqs stalled start={start_irqs} mid={mid_irqs} end={end_irqs}")
            return 1
        log(f"nic_irqs {start_irqs} -> {mid_irqs} -> {end_irqs}")
    if a.max_busy_ratio is not None:
        end_busy = None if end_status is None else end_status.get("net_busy_ms")
        if not isinstance(start_busy, int) or not isinstance(end_busy, int):
            log(f"net_busy_ms samples start={start_busy} end={end_busy}")
            return 1
        ratio = (end_busy - start_busy) / max(el * 1000.0, 1.0)
        log(f"net_busy_ms {start_busy} -> {end_busy} ({ratio:.2f} of wall)")
        if ratio > a.max_busy_ratio:
            log(f"network core busy ratio {ratio:.2f} exceeds {a.max_busy_ratio:.2f}")
            return 1
    return 0

if __name__ == "__main__":
    sys.exit(main())
