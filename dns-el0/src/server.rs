//! Resolver and DNS server. Same answers as the old kernel loop: local
//! records, cache, upstream retry, and a blocklist that the kernel stores.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use dns_cache::{CacheEntry, DnsCache};
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
const SET_CACHE_ENTRIES: u64 = 17;
const LOG_POST: u64 = 28;

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
        exit(1);
    }
    let cache_size = sys(CONFIG_CACHE_SIZE, 0, 0, 0, 0, 0, 0).max(1) as usize;
    let mut cache = DnsCache::new(cache_size);
    let mut pending: Vec<PendingQuery> = Vec::new();
    let mut next_id: u16 = 1;
    let mut tcp_fd: i64 = -1;
    let mut tcp_buf: Vec<u8> = Vec::new();
    publish_cache(cache.len());
    UPSTREAM_FD.store(upstream, Ordering::Release);
    let mut worker = 0;
    while worker < 2 {
        let top = 0x1_000A_0000u64 + (1536 * 1024) - (worker as u64 + 1) * (16 * 1024);
        let _ = crate::thread::spawn(upstream_worker, top, worker as u64);
        worker += 1;
    }

    loop {
        while let Some((packet, ip, port)) = udp_recv(dns) {
            handle_client(
                &packet,
                ip,
                port,
                dns,
                upstream,
                &mut cache,
                &mut pending,
                &mut next_id,
            );
        }
        drain_upstream(dns, &mut cache, &mut pending);
        service_tcp(tcp, &mut tcp_fd, &mut tcp_buf, &mut cache, &mut pending, &mut next_id, upstream, dns);
        retry_expired(upstream, dns, &mut pending, &mut next_id);
        KICK.fetch_add(1, Ordering::Release);
        let _ = crate::thread::futex_wake(&KICK, 2);
        yield_now();
    }
}

static UPSTREAM_FD: AtomicI64 = AtomicI64::new(0);
static KICK: AtomicU32 = AtomicU32::new(0);
static MAIL_LOCK: AtomicU32 = AtomicU32::new(0);

struct Mail {
    used: bool,
    len: u16,
    ip: [u8; 4],
    port: u16,
    data: [u8; 512],
}

static mut MAIL: [Mail; 8] = [
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
    Mail { used: false, len: 0, ip: [0; 4], port: 0, data: [0; 512] },
];

extern "C" fn upstream_worker(_index: u64) {
    loop {
        let fd = UPSTREAM_FD.load(Ordering::Acquire);
        if fd <= 0 {
            let seen = KICK.load(Ordering::Acquire);
            let _ = crate::thread::futex_wait(&KICK, seen);
            continue;
        }
        if let Some((packet, ip, port)) = udp_recv(fd) {
            deposit(&packet, ip, port);
            continue;
        }
        let seen = KICK.load(Ordering::Acquire);
        let _ = crate::thread::futex_wait(&KICK, seen);
    }
}

fn deposit(packet: &[u8], ip: [u8; 4], port: u16) {
    let copy = packet.len().min(512);
    loop {
        crate::thread::mutex_lock(&MAIL_LOCK);
        let stored = unsafe {
            let mut index = 0;
            while index < MAIL.len() {
                if !MAIL[index].used {
                    MAIL[index].used = true;
                    MAIL[index].len = copy as u16;
                    MAIL[index].ip = ip;
                    MAIL[index].port = port;
                    MAIL[index].data[..copy].copy_from_slice(&packet[..copy]);
                    break;
                }
                index += 1;
            }
            index < MAIL.len()
        };
        crate::thread::mutex_unlock(&MAIL_LOCK);
        if stored {
            return;
        }
        yield_now();
    }
}

fn drain_upstream(dns: i64, cache: &mut DnsCache, pending: &mut Vec<PendingQuery>) {
    loop {
        let mut packet = [0u8; 512];
        let mut len = 0usize;
        crate::thread::mutex_lock(&MAIL_LOCK);
        unsafe {
            let mut index = 0;
            while index < MAIL.len() {
                if MAIL[index].used {
                    len = MAIL[index].len as usize;
                    packet[..len].copy_from_slice(&MAIL[index].data[..len]);
                    MAIL[index].used = false;
                    break;
                }
                index += 1;
            }
        }
        crate::thread::mutex_unlock(&MAIL_LOCK);
        if len == 0 {
            return;
        }
        take_upstream(&packet[..len], dns, cache, pending);
    }
}

fn service_tcp(
    listen: i64,
    conn: &mut i64,
    buf: &mut Vec<u8>,
    cache: &mut DnsCache,
    pending: &mut Vec<PendingQuery>,
    next_id: &mut u16,
    upstream: i64,
    dns: i64,
) {
    if listen < 0 {
        return;
    }
    if *conn < 0 {
        let accepted = sys(TCP_ACCEPT, listen as u64, 0, 0, 0, 0, 0);
        if accepted >= 0 {
            *conn = accepted;
            buf.clear();
        }
        return;
    }
    let mut tmp = [0u8; 512];
    let n = sys(TCP_RECV, *conn as u64, tmp.as_mut_ptr() as u64, tmp.len() as u64, 0, 0, 0);
    if n == 0 {
        let _ = sys(TCP_CLOSE, *conn as u64, 0, 0, 0, 0, 0);
        *conn = -1;
        buf.clear();
        return;
    }
    if n < 0 {
        return;
    }
    buf.extend_from_slice(&tmp[..n as usize]);
    if buf.len() < 2 {
        return;
    }
    let need = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < 2 + need || need == 0 {
        return;
    }
    let query = buf[2..2 + need].to_vec();
    buf.drain(..2 + need);
    let mut replies = Vec::new();
    let mut forwards = Vec::new();
    handle_bytes(&query, [0, 0, 0, 0], 0, cache, pending, next_id, &mut replies, &mut forwards, false);
    for (bytes, _ip, _port) in replies {
        send_tcp(*conn, &bytes);
        *conn = -1;
        break;
    }
    for (bytes, ip) in forwards {
        udp_send(upstream, &bytes, ip, 53);
    }
    let _ = dns;
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
) {
    let mut replies = Vec::new();
    let mut forwards = Vec::new();
    handle_bytes(packet, ip, port, cache, pending, next_id, &mut replies, &mut forwards, true);
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
) {
    let now = time_ms();
    let seconds = now / 1000;
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

    if let Some(mut cached) = cached_response(cache, &name, qtype, seconds) {
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
        sent_ms: now,
        upstream_idx: 0,
        original: packet.to_vec(),
    });
    forwards.push((forwarded, dest));
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
    remember(cache, &item.name, item.qtype, &packet, time_ms() / 1000);
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

fn cached_response(cache: &DnsCache, name: &str, qtype: u16, seconds: u64) -> Option<Vec<u8>> {
    cache.get(name, qtype, seconds).map(|entry| entry.data.clone())
}

fn remember(cache: &mut DnsCache, name: &str, qtype: u16, packet: &[u8], seconds: u64) {
    if packet.len() < 12 || packet[2] & 0x02 != 0 {
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
    cache.insert(CacheEntry {
        name: name.to_string(),
        record_type: qtype,
        data: packet.to_vec(),
        ttl,
        timestamp: seconds,
    });
    publish_cache(cache.len());
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

fn publish_cache(len: usize) {
    let _ = sys(SET_CACHE_ENTRIES, len as u64, 0, 0, 0, 0, 0);
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
