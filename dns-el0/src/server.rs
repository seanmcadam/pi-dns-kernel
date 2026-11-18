//! Resolver and DNS server. Same answers as the old kernel loop: local
//! records, cache, upstream retry, and a blocklist that the kernel stores.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use dns_cache::DnsCache;
use dns_protocol::{DnsPacket, QueryType, RData, RecordClass, RecordType, ResourceRecord};

const LOG: u64 = 0;
const YIELD: u64 = 1;
const EXIT: u64 = 3;
const TIME_MS: u64 = 4;
const UDP_BIND: u64 = 5;
const UDP_RECV: u64 = 6;
const UDP_SEND: u64 = 7;
const TCP_LISTEN: u64 = 8;
const TCP_ACCEPT: u64 = 9;
const TCP_RECV: u64 = 10;
const TCP_SEND: u64 = 11;
const TCP_CLOSE: u64 = 12;
const CONFIG_RECORDS: u64 = 13;
const CONFIG_UPSTREAMS: u64 = 14;
const CONFIG_BLOCKLIST: u64 = 15;
const CONFIG_CACHE_SIZE: u64 = 16;
const LOG_POST: u64 = 28;
const CONFIG_CACHE_REAP: u64 = 50;
const TIMER_TICKS: u64 = 51;
const TIMER_FREQ: u64 = 52;
const SET_CACHE_STATS: u64 = 53;
const CACHE_PLANT: u64 = 54;
const SET_DNS_TOP: u64 = 57;

const UPSTREAM_PORT: u16 = 53535;
const UPSTREAM_WAIT_MS: u64 = 2_000;
const MAX_PENDING: usize = 8;

struct PendingQuery {
    client_id: u16,
    upstream_id: u16,
    client_ip: [u8; 4],
    client_port: u16,
    name: String,
    qtype: u16,
    qclass: u16,
    sent_ms: u64,
    upstream_idx: usize,
    original: Vec<u8>,
}

struct Record {
    name: String,
    kind: String,
    value: String,
    ttl: u32,
}

pub fn run() -> ! {
    let dns = udp_bind(53);
    let upstream = udp_bind(UPSTREAM_PORT);
    let tcp = tcp_listen(53);
    if dns < 0 || upstream < 0 {
        let _ = log_line("dns-el0: bind failed");
        panic_rt::note_text(b"dns-el0: bind failed");
        exit(1);
    }
    if tcp < 0 {
        let _ = log_line("dns-el0: tcp listen failed");
        panic_rt::note_text(b"dns-el0: tcp listen failed");
    }
    let cache_size = sys(CONFIG_CACHE_SIZE, 0, 0, 0, 0, 0, 0).clamp(1, 1024) as usize;
    let cache = DnsCache::new(cache_size);
    publish(&cache);
    let mut pending: Vec<PendingQuery> = Vec::new();
    let mut next_id: u16 = 1;
    let mut tcp_fd: i64 = -1;
    let mut tcp_buf: Vec<u8> = Vec::new();
    // Socket calls now sleep until the NIC has data, so this thread cannot
    // poll the client socket, the upstream mailbox, and TCP in one loop.
    // Each wait lives on its own thread and they share the cache under one
    // futex mutex. Sends happen while that mutex is held; a send completes
    // in the network owner without waiting for a peer, so the lock is not
    // held across a packet round trip.
    let shared = Box::new(Shared {
        cache,
        pending,
        next_id,
        dns,
        upstream,
        tcp,
        tcp_fd,
        tcp_buf,
        traffic: dns_cache::top::Traffic::new(),
    });
    unsafe {
        SHARED = Box::into_raw(shared);
    }
    UPSTREAM_FD.store(upstream, Ordering::Release);
    let mut worker = 0;
    while worker < 2 {
        let top = thread_stack(worker);
        let _ = crate::thread::spawn(upstream_worker, top, worker as u64);
        worker += 1;
    }
    let _ = crate::thread::spawn(tcp_worker, thread_stack(2), 0);
    let _ = crate::thread::spawn(retry_worker, thread_stack(3), 0);
    let _ = crate::thread::spawn(cache_reaper, thread_stack(4), 0);

    loop {
        if let Some((packet, ip, port)) = udp_recv(dns) {
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    handle_client(
                        &packet,
                        ip,
                        port,
                        shared.dns,
                        shared.upstream,
                        &mut shared.cache,
                        &mut shared.pending,
                        &mut shared.next_id,
                        &mut shared.traffic,
                    );
                    publish(&shared.cache);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
        } else {
            yield_now();
        }
    }
}

struct Shared {
    cache: DnsCache,
    pending: Vec<PendingQuery>,
    next_id: u16,
    dns: i64,
    upstream: i64,
    tcp: i64,
    tcp_fd: i64,
    tcp_buf: Vec<u8>,
    traffic: dns_cache::top::Traffic,
}

static mut SHARED: *mut Shared = core::ptr::null_mut();
static UPSTREAM_FD: AtomicI64 = AtomicI64::new(0);
static STATE_LOCK: AtomicU32 = AtomicU32::new(0);
static RETRY_KICK: AtomicU32 = AtomicU32::new(0);

fn thread_stack(index: u64) -> u64 {
    (crate::HEAP_VA + crate::HEAP_SIZE
        - crate::PAGE
        - (index as usize) * (crate::STACK_BYTES + crate::PAGE)) as u64
}

extern "C" fn upstream_worker(_index: u64) {
    loop {
        let fd = UPSTREAM_FD.load(Ordering::Acquire);
        if fd <= 0 {
            yield_now();
            continue;
        }
        let Some((packet, _ip, _port)) = udp_recv(fd) else {
            yield_now();
            continue;
        };
        crate::thread::mutex_lock(&STATE_LOCK);
        unsafe {
            if let Some(shared) = SHARED.as_mut() {
                take_upstream(&packet, shared.dns, &mut shared.cache, &mut shared.pending);
                publish(&shared.cache);
            }
        }
        crate::thread::mutex_unlock(&STATE_LOCK);
    }
}

extern "C" fn retry_worker(_index: u64) {
    loop {
        crate::thread::mutex_lock(&STATE_LOCK);
        let mut busy = false;
        unsafe {
            if let Some(shared) = SHARED.as_mut() {
                if !shared.pending.is_empty() {
                    retry_expired(
                        shared.upstream,
                        shared.dns,
                        &mut shared.pending,
                        &mut shared.next_id,
                    );
                    busy = !shared.pending.is_empty();
                }
            }
        }
        let seen = RETRY_KICK.load(Ordering::Acquire);
        crate::thread::mutex_unlock(&STATE_LOCK);
        if busy {
            yield_now();
        } else {
            let _ = crate::thread::futex_wait(&RETRY_KICK, seen);
        }
    }
}

fn poke_retry() {
    RETRY_KICK.fetch_add(1, Ordering::Release);
    let _ = crate::thread::futex_wake(&RETRY_KICK, 1);
}

extern "C" fn tcp_worker(_index: u64) {
    loop {
        let listen = unsafe { SHARED.as_ref().map(|shared| shared.tcp).unwrap_or(-1) };
        if listen < 0 {
            yield_now();
            continue;
        }
        let accepted = sys(TCP_ACCEPT, listen as u64, 0, 0, 0, 0, 0);
        if accepted < 0 {
            yield_now();
            continue;
        }
        crate::thread::mutex_lock(&STATE_LOCK);
        unsafe {
            if let Some(shared) = SHARED.as_mut() {
                shared.tcp_fd = accepted;
                shared.tcp_buf.clear();
            }
        }
        crate::thread::mutex_unlock(&STATE_LOCK);
        loop {
            let mut tmp = [0u8; 512];
            let n = sys(
                TCP_RECV,
                accepted as u64,
                tmp.as_mut_ptr() as u64,
                tmp.len() as u64,
                0,
                0,
                0,
            );
            if n <= 0 {
                let _ = sys(TCP_CLOSE, accepted as u64, 0, 0, 0, 0, 0);
                break;
            }
            let mut done = false;
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    shared.tcp_buf.extend_from_slice(&tmp[..n as usize]);
                    done = take_tcp_query(shared);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
            if done {
                break;
            }
        }
    }
}

fn take_tcp_query(shared: &mut Shared) -> bool {
    if shared.tcp_buf.len() < 2 {
        return false;
    }
    let need = u16::from_be_bytes([shared.tcp_buf[0], shared.tcp_buf[1]]) as usize;
    if need == 0 || shared.tcp_buf.len() < 2 + need {
        return false;
    }
    let query = shared.tcp_buf[2..2 + need].to_vec();
    shared.tcp_buf.drain(..2 + need);
    let conn = shared.tcp_fd;
    let mut replies = Vec::new();
    let mut forwards = Vec::new();
    handle_bytes(
        &query,
        [0, 0, 0, 0],
        0,
        &mut shared.cache,
        &mut shared.pending,
        &mut shared.next_id,
        &mut replies,
        &mut forwards,
        false,
        &mut shared.traffic,
    );
    publish(&shared.cache);
    for (bytes, ip) in forwards {
        udp_send(shared.upstream, &bytes, ip, 53);
    }
    for (bytes, _ip, _port) in replies {
        send_tcp(conn, &bytes);
        let _ = sys(TCP_CLOSE, conn as u64, 0, 0, 0, 0, 0);
        shared.tcp_fd = -1;
        return true;
    }
    false
}

fn send_tcp(fd: i64, payload: &[u8]) {
    if payload.len() > 4000 {
        return;
    }
    let mut frame = Vec::with_capacity(payload.len() + 2);
    let len = (payload.len() as u16).to_be_bytes();
    frame.extend_from_slice(&len);
    frame.extend_from_slice(payload);
    let mut off = 0;
    while off < frame.len() {
        let n = sys(
            TCP_SEND,
            fd as u64,
            frame[off..].as_ptr() as u64,
            (frame.len() - off) as u64,
            0,
            0,
            0,
        );
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
    let _ = sys(TCP_CLOSE, fd as u64, 0, 0, 0, 0, 0);
}

fn handle_client(
    packet: &[u8],
    ip: [u8; 4],
    port: u16,
    dns: i64,
    upstream: i64,
    cache: &mut DnsCache,
    pending: &mut Vec<PendingQuery>,
    next_id: &mut u16,
    traffic: &mut dns_cache::top::Traffic,
) {
    let mut replies = Vec::new();
    let mut forwards = Vec::new();
    handle_bytes(
        packet,
        ip,
        port,
        cache,
        pending,
        next_id,
        &mut replies,
        &mut forwards,
        true,
        traffic,
    );
    for (bytes, to_ip, to_port) in replies {
        udp_send(dns, &bytes, to_ip, to_port);
    }
    for (bytes, dest) in forwards {
        udp_send(upstream, &bytes, dest, 53);
    }
}

fn handle_bytes(
    packet: &[u8],
    ip: [u8; 4],
    port: u16,
    cache: &mut DnsCache,
    pending: &mut Vec<PendingQuery>,
    next_id: &mut u16,
    replies: &mut Vec<(Vec<u8>, [u8; 4], u16)>,
    forwards: &mut Vec<(Vec<u8>, [u8; 4])>,
    remember_client: bool,
    traffic: &mut dns_cache::top::Traffic,
) {
    let now_ms = time_ms();
    let query = match DnsPacket::parse(packet) {
        Ok(query) => query,
        Err(_) => return,
    };
    if query.header.qr || query.questions.is_empty() {
        return;
    }
    let question = &query.questions[0];
    let name = normalize_name(&question.name);
    post_query(&name);
    let qtype = question.qtype as u16;
    traffic.note(name.as_bytes(), qtype, &ip);
    let qclass = question.qclass as u16;

    if blocked(&name) {
        if let Some(bytes) = error_response(&query, 3) {
            replies.push((bytes, ip, port));
        }
        return;
    }

    if let Some(answer) = lookup_local(&question.name, question.qtype) {
        let mut response = DnsPacket::new(query.header.id);
        response.header.qr = true;
        response.header.aa = true;
        response.header.rd = query.header.rd;
        response.header.ra = true;
        response.questions.push(question.clone());
        response.answers.push(answer);
        if let Ok(bytes) = response.to_bytes() {
            replies.push((bytes, ip, port));
        }
        return;
    }

    if let Some(mut cached) = cached_response(cache, &name, qtype, qclass, timer_ticks()) {
        set_id(&mut cached, query.header.id);
        replies.push((cached, ip, port));
        return;
    }

    let upstreams = load_upstreams();
    if !remember_client || upstreams.is_empty() || pending.len() >= MAX_PENDING {
        if let Some(bytes) = error_response(&query, 2) {
            replies.push((bytes, ip, port));
        }
        return;
    }

    let upstream_id = next_upstream_id(next_id);
    let mut forwarded = packet.to_vec();
    set_id(&mut forwarded, upstream_id);
    let dest = upstreams[0];
    pending.push(PendingQuery {
        client_id: query.header.id,
        upstream_id,
        client_ip: ip,
        client_port: port,
        name,
        qtype,
        qclass,
        sent_ms: now_ms,
        upstream_idx: 0,
        original: packet.to_vec(),
    });
    forwards.push((forwarded, dest));
    poke_retry();
}

fn retry_expired(upstream_fd: i64, dns_fd: i64, pending: &mut Vec<PendingQuery>, next_id: &mut u16) {
    if pending.is_empty() {
        return;
    }
    let now = time_ms();
    let upstreams = load_upstreams();
    let mut index = 0;
    while index < pending.len() {
        if now.saturating_sub(pending[index].sent_ms) < UPSTREAM_WAIT_MS {
            index += 1;
            continue;
        }
        let next = pending[index].upstream_idx + 1;
        if next < upstreams.len() {
            let id = next_upstream_id(next_id);
            let mut forwarded = pending[index].original.clone();
            set_id(&mut forwarded, id);
            let dest = upstreams[next];
            pending[index].upstream_id = id;
            pending[index].upstream_idx = next;
            pending[index].sent_ms = now;
            udp_send(upstream_fd, &forwarded, dest, 53);
            index += 1;
        } else {
            let item = pending.remove(index);
            if let Some(bytes) = servfail_from_query(&item.original) {
                udp_send(dns_fd, &bytes, item.client_ip, item.client_port);
            }
        }
    }
}

fn take_upstream(packet: &[u8], dns_fd: i64, cache: &mut DnsCache, pending: &mut Vec<PendingQuery>) {
    if packet.len() < 12 {
        return;
    }
    let id = packet_id(packet);
    let Some(pos) = pending.iter().position(|item| item.upstream_id == id) else {
        return;
    };
    let item = pending.remove(pos);
    let mut packet = packet.to_vec();
    set_id(&mut packet, item.client_id);
    remember(cache, &item.name, item.qtype, item.qclass, &packet);
    udp_send(dns_fd, &packet, item.client_ip, item.client_port);
}

fn lookup_local(name: &str, qtype: QueryType) -> Option<ResourceRecord> {
    let wanted = normalize_name(name);
    for record in load_records() {
        if normalize_name(&record.name) != wanted {
            continue;
        }
        let type_matches = match (record.kind.as_str(), qtype) {
            ("A", QueryType::A) => true,
            ("AAAA", QueryType::AAAA) => true,
            ("CNAME", QueryType::CNAME) => true,
            ("PTR", QueryType::PTR) => true,
            (_, QueryType::ANY) => true,
            _ => false,
        };
        if !type_matches {
            continue;
        }
        return Some(ResourceRecord {
            name: name.to_string(),
            rtype: match record.kind.as_str() {
                "AAAA" => RecordType::AAAA,
                "CNAME" => RecordType::CNAME,
                "PTR" => RecordType::PTR,
                _ => RecordType::A,
            },
            rclass: RecordClass::IN,
            ttl: record.ttl,
            rdata: parse_rdata(&record.kind, &record.value),
        });
    }
    None
}

fn parse_rdata(kind: &str, value: &str) -> RData {
    match kind {
        "A" => {
            let mut parts = value.split('.');
            let parse = |text: Option<&str>| text.and_then(|part| part.parse().ok()).unwrap_or(0);
            RData::A([parse(parts.next()), parse(parts.next()), parse(parts.next()), parse(parts.next())])
        }
        "AAAA" => RData::AAAA([0; 16]),
        "CNAME" => RData::CNAME(value.to_string()),
        "PTR" => RData::PTR(value.to_string()),
        _ => RData::A([0, 0, 0, 0]),
    }
}

fn cached_response(
    cache: &mut DnsCache,
    name: &str,
    qtype: u16,
    qclass: u16,
    now: u64,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; 512];
    cache
        .get(name, qtype, qclass, now, &mut buf)
        .map(|n| buf[..n].to_vec())
}

fn remember(cache: &mut DnsCache, name: &str, qtype: u16, qclass: u16, packet: &[u8]) {
    if packet.len() < 12 || packet.len() > 512 || packet[2] & 0x02 != 0 {
        return;
    }
    let rcode = packet[3] & 0x0f;
    if rcode != 0 && rcode != 3 {
        return;
    }
    let ttl = match DnsPacket::parse(packet) {
        Ok(parsed) if !parsed.answers.is_empty() => parsed
            .answers
            .iter()
            .map(|answer| answer.ttl)
            .min()
            .unwrap_or(30)
            .max(1),
        Ok(_) => 30,
        Err(_) => 60,
    };
    let now = timer_ticks();
    let freq = timer_freq().max(1);
    let expiry = now.saturating_add((ttl as u64).saturating_mul(freq));
    let _ = cache.insert(name, qtype, qclass, packet, expiry, now);
}

extern "C" fn cache_reaper(_arg: u64) {
    let mut last_reap = 0u64;
    let mut last_top = 0u64;
    loop {
        let now_ms = time_ms();
        if last_top == 0 || now_ms.saturating_sub(last_top) >= 200 {
            let mut raw = [0u8; 576];
            let mut n = 0;
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    n = shared.traffic.write(now_ms, &mut raw);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
            if n > 0 {
                let _ = sys(SET_DNS_TOP, raw.as_ptr() as u64, n as u64, 0, 0, 0, 0);
            }
            last_top = now_ms;
        }
        let planted = sys(CACHE_PLANT, 1, 0, 0, 0, 0, 0);
        if planted > 0 {
            let now = timer_ticks();
            let freq = timer_freq().max(1);
            let expiry = now.saturating_add((planted as u64).saturating_mul(freq));
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    let _ = shared.cache.insert(
                        "reap.invalid",
                        1,
                        1,
                        b"reap-test-record!",
                        expiry,
                        now,
                    );
                    publish(&shared.cache);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
        }
        let secs = sys(CONFIG_CACHE_REAP, 0, 0, 0, 0, 0, 0).max(1) as u64;
        let now_ms = time_ms();
        if last_reap == 0 || now_ms.saturating_sub(last_reap) >= secs.saturating_mul(1000) {
            let now = timer_ticks();
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    shared.cache.reap(now);
                    publish(&shared.cache);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
            last_reap = now_ms;
        }
        yield_now();
    }
}

fn servfail_from_query(original: &[u8]) -> Option<Vec<u8>> {
    let query = DnsPacket::parse(original).ok()?;
    error_response(&query, 2)
}

fn error_response(query: &DnsPacket, rcode: u8) -> Option<Vec<u8>> {
    let mut response = DnsPacket::new(query.header.id);
    response.header.qr = true;
    response.header.rd = query.header.rd;
    response.header.ra = true;
    response.header.rcode = rcode;
    if let Some(question) = query.questions.first() {
        response.questions.push(question.clone());
    }
    response.to_bytes().ok()
}

fn next_upstream_id(next: &mut u16) -> u16 {
    let id = *next;
    *next = id.wrapping_add(1);
    if *next == 0 {
        *next = 1;
    }
    if id == 0 { 1 } else { id }
}

fn packet_id(packet: &[u8]) -> u16 {
    u16::from_be_bytes([packet[0], packet[1]])
}

fn set_id(packet: &mut [u8], id: u16) {
    if packet.len() >= 2 {
        let bytes = id.to_be_bytes();
        packet[0] = bytes[0];
        packet[1] = bytes[1];
    }
}

fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        let lower = if byte.is_ascii_uppercase() { byte + 32 } else { byte };
        out.push(lower as char);
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

fn blocked(name: &str) -> bool {
    let text = match read_config(CONFIG_BLOCKLIST) {
        Some(text) => text,
        None => return false,
    };
    for line in text.lines() {
        if normalize_name(line) == *name {
            return true;
        }
    }
    false
}

fn load_records() -> Vec<Record> {
    let Some(text) = read_config(CONFIG_RECORDS) else {
        return Vec::new();
    };
    let mut records = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else { continue };
        let Some(kind) = parts.next() else { continue };
        let Some(value) = parts.next() else { continue };
        let ttl = parts.next().and_then(|text| text.parse().ok()).unwrap_or(300);
        records.push(Record {
            name: name.to_string(),
            kind: kind.to_string(),
            value: value.to_string(),
            ttl,
        });
    }
    records
}

fn load_upstreams() -> Vec<[u8; 4]> {
    let Some(text) = read_config(CONFIG_UPSTREAMS) else {
        return Vec::new();
    };
    let mut list = Vec::new();
    for line in text.lines() {
        if let Some(ip) = parse_ip(line) {
            list.push(ip);
        }
    }
    list
}

fn parse_ip(text: &str) -> Option<[u8; 4]> {
    let mut parts = text.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let c = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some([a, b, c, d])
}

fn read_config(num: u64) -> Option<String> {
    let mut buf = [0u8; 2048];
    let n = sys(num, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n < 0 {
        return None;
    }
    core::str::from_utf8(&buf[..n as usize]).ok().map(|text| text.to_string())
}

fn publish(cache: &DnsCache) {
    let stats = cache.stats();
    let words = [
        stats.slots,
        stats.used,
        stats.free,
        stats.hits,
        stats.misses,
        stats.evictions,
        stats.reaped,
        stats.bytes,
    ];
    let mut raw = [0u8; 64];
    let mut index = 0;
    while index < words.len() {
        let bytes = words[index].to_le_bytes();
        let start = index * 8;
        raw[start..start + 8].copy_from_slice(&bytes);
        index += 1;
    }
    let _ = sys(SET_CACHE_STATS, raw.as_ptr() as u64, 0, 0, 0, 0, 0);
}

fn timer_ticks() -> u64 {
    sys(TIMER_TICKS, 0, 0, 0, 0, 0, 0) as u64
}

fn timer_freq() -> u64 {
    sys(TIMER_FREQ, 0, 0, 0, 0, 0, 0) as u64
}

fn udp_bind(port: u16) -> i64 {
    sys(UDP_BIND, port as u64, 0, 0, 0, 0, 0)
}

fn tcp_listen(port: u16) -> i64 {
    sys(TCP_LISTEN, port as u64, 0, 0, 0, 0, 0)
}

fn udp_recv(fd: i64) -> Option<(Vec<u8>, [u8; 4], u16)> {
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 1500];
    let mut addr = [0u8; 8];
    let n = sys(UDP_RECV, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, addr.as_mut_ptr() as u64, 0, 0);
    if n <= 0 {
        return None;
    }
    let port = u16::from_be_bytes([addr[4], addr[5]]);
    Some((buf[..n as usize].to_vec(), [addr[0], addr[1], addr[2], addr[3]], port))
}

fn udp_send(fd: i64, bytes: &[u8], ip: [u8; 4], port: u16) -> bool {
    if fd < 0 || bytes.is_empty() {
        return false;
    }
    let packed = u32::from_be_bytes(ip);
    sys(UDP_SEND, fd as u64, bytes.as_ptr() as u64, bytes.len() as u64, packed as u64, port as u64, 0) == 0
}

fn time_ms() -> u64 {
    sys(TIME_MS, 0, 0, 0, 0, 0, 0) as u64
}

fn yield_now() {
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
}

fn exit(code: u64) -> ! {
    let _ = sys(EXIT, code, 0, 0, 0, 0, 0);
    loop {}
}

fn post_query(name: &str) {
    let mut buf = [0u8; 64];
    let prefix = b"dns query ";
    let bytes = name.as_bytes();
    let take = bytes.len().min(buf.len() - prefix.len());
    buf[..prefix.len()].copy_from_slice(prefix);
    buf[prefix.len()..prefix.len() + take].copy_from_slice(&bytes[..take]);
    let _ = sys(
        LOG_POST,
        buf.as_ptr() as u64,
        (prefix.len() + take) as u64,
        0,
        0,
        0,
        0,
    );
}

fn log_line(text: &str) -> i64 {
    let bytes = text.as_bytes();
    let len = bytes.len().min(120);
    sys(LOG, bytes.as_ptr() as u64, len as u64, 0, 0, 0, 0)
}

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
        );
    }
    ret as i64
}
