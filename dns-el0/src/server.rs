//! Resolver and DNS server. Same answers as the old kernel loop: local
//! records, cache, upstream retry, and a blocklist that the kernel stores.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};
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
const SET_UPSTREAM_HEALTH: u64 = 60;
const CONFIG_PROBE_SECS: u64 = 62;
const NET_READY: u64 = 63;
const SET_DNS_LOAD: u64 = 65;

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
    walk: crate::health::Walk,
    original: Vec<u8>,
}

#[derive(Clone, Copy)]
struct ProbeSlot {
    ip: [u8; 4],
    sent_ms: u64,
    last_ms: u64,
    in_flight: bool,
}

impl ProbeSlot {
    const fn empty() -> Self {
        Self {
            ip: [0; 4],
            sent_ms: 0,
            last_ms: 0,
            in_flight: false,
        }
    }
}

#[derive(Clone, Copy)]
struct ProbeJob {
    index: usize,
    ip: [u8; 4],
    query: [u8; crate::health::PROBE_LEN],
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
    // Socket calls sleep until the NIC has data, so this thread cannot
    // poll the client socket, the upstream socket, and TCP in one loop.
    // Each wait lives on its own thread. The mutex covers the cache and
    // the pending table only. UDP sends run on send_worker, after the
    // lock is dropped, so a forward to a dead resolver cannot stall the
    // client recv loop. Local and cached answers are queued there too
    // and go out before any forward in the same batch.
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
        health: crate::health::Health::new(0),
        probes: [ProbeSlot::empty(); crate::health::MAX],
        out_replies: Vec::new(),
        out_forwards: Vec::new(),
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
    let _ = crate::thread::spawn(send_worker, thread_stack(3), 0);
    let _ = crate::thread::spawn(cache_reaper, thread_stack(4), 0);

    loop {
        let Some((packet, ip, port)) = udp_recv(dns) else {
            yield_now();
            continue;
        };
        let mut batch = Vec::new();
        batch.push((packet, ip, port));
        while batch.len() < 32 {
            let Some(more) = udp_try_recv(dns) else {
                break;
            };
            batch.push(more);
        }
        crate::thread::mutex_lock(&STATE_LOCK);
        unsafe {
            if let Some(shared) = SHARED.as_mut() {
                for (packet, ip, port) in batch {
                    handle_client(
                        &packet,
                        ip,
                        port,
                        &mut shared.cache,
                        &mut shared.pending,
                        &mut shared.next_id,
                        &mut shared.out_replies,
                        &mut shared.out_forwards,
                        &mut shared.traffic,
                        &mut shared.health,
                    );
                }
                publish(&shared.cache);
                publish_load(shared.pending.len());
                poke_send();
            }
        }
        crate::thread::mutex_unlock(&STATE_LOCK);
        // This thread sends before it blocks in recv again. On one core
        // the send worker does not run while this thread is inside that
        // recv, so a local answer would sit until the next query.
        flush_outbound();
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
    health: crate::health::Health,
    probes: [ProbeSlot; crate::health::MAX],
    out_replies: Vec<(Vec<u8>, [u8; 4], u16)>,
    out_forwards: Vec<(Vec<u8>, [u8; 4])>,
}

static mut SHARED: *mut Shared = core::ptr::null_mut();
static UPSTREAM_FD: AtomicI64 = AtomicI64::new(0);
static STATE_LOCK: AtomicU32 = AtomicU32::new(0);
static SEND_KICK: AtomicU32 = AtomicU32::new(0);
/// One plus the index of the upstream that last answered. Zero means none
/// yet. A non-zero initializer would land in `.data` and the raw image
/// would include the gap out to that section, which does not fit the
/// 512 KiB code window.
static PREFERRED: AtomicU32 = AtomicU32::new(0);
static FAILOVER_KEY: AtomicU32 = AtomicU32::new(0);
static FAILOVER_MS: AtomicU64 = AtomicU64::new(0);
/// Upstream attempts that waited `UPSTREAM_WAIT_MS` and were retired.
/// Published with the pending-table length. Must stay zero-initialized.
static UPSTREAM_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

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
        let Some(first) = udp_recv(fd) else {
            yield_now();
            continue;
        };
        let mut batch = Vec::new();
        batch.push(first);
        while batch.len() < 32 {
            let Some(more) = udp_try_recv(fd) else {
                break;
            };
            batch.push(more);
        }
        crate::thread::mutex_lock(&STATE_LOCK);
        unsafe {
            if let Some(shared) = SHARED.as_mut() {
                for (packet, ip, port) in batch {
                    take_upstream(
                        &packet,
                        ip,
                        port,
                        &mut shared.cache,
                        &mut shared.pending,
                        &mut shared.health,
                        &mut shared.probes,
                        &mut shared.out_replies,
                    );
                }
                publish(&shared.cache);
                publish_load(shared.pending.len());
                poke_send();
            }
        }
        crate::thread::mutex_unlock(&STATE_LOCK);
        flush_outbound();
    }
}

/// Send queued replies and forwards. The thread that queued them calls
/// this after dropping the state lock, so a one-core guest does not leave
/// the answer waiting on another thread. `send_worker` does the same for
/// anything that is still queued when this thread has gone back to recv.
fn flush_outbound() {
    let mut replies = Vec::new();
    let mut forwards = Vec::new();
    let mut dns = -1i64;
    let mut upstream = -1i64;
    crate::thread::mutex_lock(&STATE_LOCK);
    unsafe {
        if let Some(shared) = SHARED.as_mut() {
            core::mem::swap(&mut replies, &mut shared.out_replies);
            core::mem::swap(&mut forwards, &mut shared.out_forwards);
            dns = shared.dns;
            upstream = shared.upstream;
        }
    }
    crate::thread::mutex_unlock(&STATE_LOCK);
    for (bytes, ip, port) in replies {
        udp_send(dns, &bytes, ip, port);
    }
    for (bytes, ip) in forwards {
        if !udp_send(upstream, &bytes, ip, 53) {
            note_busy(ip);
        }
    }
}

extern "C" fn send_worker(_index: u64) {
    loop {
        crate::thread::mutex_lock(&STATE_LOCK);
        let seen = SEND_KICK.load(Ordering::Acquire);
        let idle = unsafe {
            SHARED.as_ref().map(|shared| shared.out_replies.is_empty() && shared.out_forwards.is_empty()).unwrap_or(true)
        };
        crate::thread::mutex_unlock(&STATE_LOCK);
        if !idle {
            flush_outbound();
            continue;
        }
        let _ = crate::thread::futex_wait(&SEND_KICK, seen);
    }
}

fn poke_send() {
    SEND_KICK.fetch_add(1, Ordering::Release);
    let _ = crate::thread::futex_wake(&SEND_KICK, 1);
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
            let mut outbound = None;
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    shared.tcp_buf.extend_from_slice(&tmp[..n as usize]);
                    outbound = take_tcp_query(shared);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
            flush_outbound();
            if let Some(bytes) = outbound {
                send_tcp(accepted, &bytes);
                break;
            }
        }
    }
}

/// `Some` is a finished answer. The caller sends it after dropping the lock.
fn take_tcp_query(shared: &mut Shared) -> Option<Vec<u8>> {
    if shared.tcp_buf.len() < 2 {
        return None;
    }
    let need = u16::from_be_bytes([shared.tcp_buf[0], shared.tcp_buf[1]]) as usize;
    if need == 0 || shared.tcp_buf.len() < 2 + need {
        return None;
    }
    let query = shared.tcp_buf[2..2 + need].to_vec();
    shared.tcp_buf.drain(..2 + need);
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
        &mut shared.health,
    );
    publish(&shared.cache);
    if !forwards.is_empty() {
        queue_forwards(&mut shared.out_forwards, forwards);
        poke_send();
    }
    if let Some((bytes, _, _)) = replies.into_iter().next() {
        shared.tcp_fd = -1;
        return Some(bytes);
    }
    None
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
    cache: &mut DnsCache,
    pending: &mut Vec<PendingQuery>,
    next_id: &mut u16,
    replies: &mut Vec<(Vec<u8>, [u8; 4], u16)>,
    forwards: &mut Vec<(Vec<u8>, [u8; 4])>,
    traffic: &mut dns_cache::top::Traffic,
    health: &mut crate::health::Health,
) {
    let mut one_reply = Vec::new();
    let mut one_forward = Vec::new();
    handle_bytes(
        packet,
        ip,
        port,
        cache,
        pending,
        next_id,
        &mut one_reply,
        &mut one_forward,
        true,
        traffic,
        health,
    );
    queue_replies(replies, one_reply);
    queue_forwards(forwards, one_forward);
}

fn queue_replies(queue: &mut Vec<(Vec<u8>, [u8; 4], u16)>, more: Vec<(Vec<u8>, [u8; 4], u16)>) {
    for item in more {
        if queue.len() >= 128 {
            break;
        }
        queue.push(item);
    }
}

fn queue_forwards(queue: &mut Vec<(Vec<u8>, [u8; 4])>, more: Vec<(Vec<u8>, [u8; 4])>) {
    for item in more {
        if queue.len() >= 64 {
            break;
        }
        queue.push(item);
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
    health: &mut crate::health::Health,
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
    health.sync(&upstreams);
    let preferred = PREFERRED.load(Ordering::Relaxed);
    let preferred = if preferred == 0 {
        0
    } else {
        preferred as usize - 1
    };
    let Some(walk) = crate::health::start_walk(health, &upstreams, preferred) else {
        if let Some(bytes) = error_response(&query, 2) {
            replies.push((bytes, ip, port));
        }
        return;
    };
    let Some(dest) = walk.current_ip() else {
        if let Some(bytes) = error_response(&query, 2) {
            replies.push((bytes, ip, port));
        }
        return;
    };
    if !remember_client || pending.len() >= MAX_PENDING {
        if let Some(bytes) = error_response(&query, 2) {
            replies.push((bytes, ip, port));
        }
        return;
    }

    let upstream_id = next_upstream_id(next_id);
    let mut forwarded = packet.to_vec();
    set_id(&mut forwarded, upstream_id);
    let upstream_idx = upstreams.iter().position(|ip| *ip == dest).unwrap_or(0);
    pending.push(PendingQuery {
        client_id: query.header.id,
        upstream_id,
        client_ip: ip,
        client_port: port,
        name,
        qtype,
        qclass,
        sent_ms: now_ms,
        upstream_idx,
        walk,
        original: packet.to_vec(),
    });
    forwards.push((forwarded, dest));
}

/// Queue the next try for anything that has waited `UPSTREAM_WAIT_MS`.
/// The send itself happens on `send_worker`. Returns whether anything was
/// queued.
fn retry_expired(
    pending: &mut Vec<PendingQuery>,
    next_id: &mut u16,
    health: &mut crate::health::Health,
    replies: &mut Vec<(Vec<u8>, [u8; 4], u16)>,
    forwards: &mut Vec<(Vec<u8>, [u8; 4])>,
) -> bool {
    if pending.is_empty() {
        return false;
    }
    let now = time_ms();
    let mut due = false;
    let mut index = 0;
    while index < pending.len() {
        if now.saturating_sub(pending[index].sent_ms) >= UPSTREAM_WAIT_MS {
            due = true;
            break;
        }
        index += 1;
    }
    if !due {
        return false;
    }
    let upstreams = load_upstreams();
    health.sync(&upstreams);
    let mut queued = false;
    index = 0;
    while index < pending.len() {
        if now.saturating_sub(pending[index].sent_ms) < UPSTREAM_WAIT_MS {
            index += 1;
            continue;
        }
        let step = pending[index].walk.on_timeout(health, now);
        UPSTREAM_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
        if step.log_down {
            note_state(false, step.failed);
        }
        if let Some(dest) = step.next {
            let id = next_upstream_id(next_id);
            let mut forwarded = pending[index].original.clone();
            set_id(&mut forwarded, id);
            note_failover(Some(step.failed), dest);
            pending[index].upstream_id = id;
            pending[index].upstream_idx = upstreams.iter().position(|ip| *ip == dest).unwrap_or(0);
            pending[index].sent_ms = now;
            let mut one = Vec::new();
            one.push((forwarded, dest));
            queue_forwards(forwards, one);
            queued = true;
            index += 1;
        } else {
            let item = pending.remove(index);
            note_exhausted(step.failed);
            if let Some(bytes) = servfail_from_query(&item.original) {
                let mut one = Vec::new();
                one.push((bytes, item.client_ip, item.client_port));
                queue_replies(replies, one);
                queued = true;
            }
        }
    }
    publish_health(health, &upstreams);
    queued
}

fn take_upstream(
    packet: &[u8],
    from_ip: [u8; 4],
    from_port: u16,
    cache: &mut DnsCache,
    pending: &mut Vec<PendingQuery>,
    health: &mut crate::health::Health,
    probes: &mut [ProbeSlot],
    replies: &mut Vec<(Vec<u8>, [u8; 4], u16)>,
) {
    if !crate::health::is_dns_response(packet, from_port) {
        return;
    }
    let id = packet_id(packet);
    if crate::health::is_probe_id(id) {
        let index = crate::health::probe_index(id);
        if index < probes.len() && probes[index].ip == from_ip && probes[index].ip != [0, 0, 0, 0] {
            let ip = probes[index].ip;
            // A late reply still proves the server answered. Clearing
            // in_flight keeps the 2s timer from counting that same probe.
            probes[index].in_flight = false;
            let upstreams = load_upstreams();
            health.sync(&upstreams);
            let change = health.note_success(ip, time_ms());
            if change.log {
                note_state(true, ip);
            }
            if change.changed {
                publish_health(health, &upstreams);
            }
        }
        return;
    }
    let Some(pos) = pending.iter().position(|item| {
        item.upstream_id == id && item.walk.current_ip() == Some(from_ip)
    }) else {
        return;
    };
    let item = pending.remove(pos);
    let upstreams = load_upstreams();
    health.sync(&upstreams);
    if let Some(ip) = item.walk.current_ip() {
        let change = health.note_success(ip, time_ms());
        if change.log {
            note_state(true, ip);
        }
        if change.changed {
            publish_health(health, &upstreams);
        }
    }
    let mut packet = packet.to_vec();
    set_id(&mut packet, item.client_id);
    remember(cache, &item.name, item.qtype, item.qclass, &packet);
    let remembered = item.upstream_idx as u32 + 1;
    let previous = PREFERRED.swap(remembered, Ordering::Relaxed);
    if previous != remembered {
        if let Some(ip) = load_upstreams().get(item.upstream_idx).copied() {
            post_upstream(b"dns upstream ", ip, b"");
        }
    }
    let mut one = Vec::new();
    one.push((packet, item.client_ip, item.client_port));
    queue_replies(replies, one);
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
        let mut queued = false;
        crate::thread::mutex_lock(&STATE_LOCK);
        unsafe {
            if let Some(shared) = SHARED.as_mut() {
                if retry_expired(
                    &mut shared.pending,
                    &mut shared.next_id,
                    &mut shared.health,
                    &mut shared.out_replies,
                    &mut shared.out_forwards,
                ) {
                    poke_send();
                    queued = true;
                }
                publish_load(shared.pending.len());
            }
        }
        crate::thread::mutex_unlock(&STATE_LOCK);
        if queued {
            flush_outbound();
        }
        let now_ms = time_ms();
        if last_top == 0 || now_ms.saturating_sub(last_top) >= 200 {
            let mut raw = [0u8; 576];
            let mut n = 0;
            let mut jobs = [None; crate::health::MAX];
            crate::thread::mutex_lock(&STATE_LOCK);
            unsafe {
                if let Some(shared) = SHARED.as_mut() {
                    n = shared.traffic.write(now_ms, &mut raw);
                    plan_probes(shared, now_ms, &mut jobs);
                }
            }
            crate::thread::mutex_unlock(&STATE_LOCK);
            if n > 0 {
                let _ = sys(SET_DNS_TOP, raw.as_ptr() as u64, n as u64, 0, 0, 0, 0);
            }
            let fd = UPSTREAM_FD.load(Ordering::Acquire);
            let mut failed = [false; crate::health::MAX];
            for (index, job) in jobs.iter().enumerate() {
                if let Some(job) = job {
                    if fd <= 0 || !udp_send(fd, &job.query, job.ip, 53) {
                        failed[index] = true;
                    }
                }
            }
            if failed.iter().any(|item| *item) {
                crate::thread::mutex_lock(&STATE_LOCK);
                unsafe {
                    if let Some(shared) = SHARED.as_mut() {
                        clear_unsent(shared, &jobs, &failed);
                    }
                }
                crate::thread::mutex_unlock(&STATE_LOCK);
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

/// Decide which upstreams need a probe. The send itself happens after
/// the DNS lock is released, so a probe cannot stall a client lookup.
fn plan_probes(shared: &mut Shared, now: u64, jobs: &mut [Option<ProbeJob>; crate::health::MAX]) {
    if !crate::health::probes_allowed(sys(NET_READY, 0, 0, 0, 0, 0, 0) != 0) {
        return;
    }
    let secs = sys(CONFIG_PROBE_SECS, 0, 0, 0, 0, 0, 0);
    let interval = if (1..=3_600).contains(&secs) {
        secs as u64 * 1_000
    } else {
        5_000
    };
    let upstreams = load_upstreams();
    shared.health.sync(&upstreams);
    let count = upstreams.len().min(crate::health::MAX);
    let mut i = 0;
    while i < count {
        let ip = upstreams[i];
        if shared.probes[i].ip != ip {
            shared.probes[i] = ProbeSlot {
                ip,
                sent_ms: 0,
                last_ms: 0,
                in_flight: false,
            };
        }
        let timed_out = shared.probes[i].in_flight
            && now.saturating_sub(shared.probes[i].sent_ms) >= UPSTREAM_WAIT_MS;
        if timed_out {
            shared.probes[i].in_flight = false;
            let change = shared.health.note_failure(ip, now);
            if change.log {
                note_state(false, ip);
            }
        }
        let due = shared.probes[i].last_ms == 0
            || now.saturating_sub(shared.probes[i].last_ms) >= interval;
        if due && !shared.probes[i].in_flight {
            shared.probes[i].in_flight = true;
            shared.probes[i].sent_ms = now;
            shared.probes[i].last_ms = now;
            jobs[i] = Some(ProbeJob {
                index: i,
                ip,
                query: crate::health::probe_query(crate::health::probe_id(i)),
            });
        }
        i += 1;
    }
    while i < crate::health::MAX {
        shared.probes[i] = ProbeSlot::empty();
        i += 1;
    }
    publish_health(&shared.health, &upstreams);
}

fn clear_unsent(shared: &mut Shared, jobs: &[Option<ProbeJob>; crate::health::MAX], failed: &[bool; crate::health::MAX]) {
    let mut i = 0;
    while i < crate::health::MAX {
        if failed[i] {
            if let Some(job) = &jobs[i] {
                if shared.probes[job.index].ip == job.ip {
                    shared.probes[job.index].in_flight = false;
                    shared.probes[job.index].last_ms = 0;
                    note_busy(job.ip);
                }
            }
        }
        i += 1;
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
    crate::health::next_client_id(next)
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
    udp_recv_mode(fd, false)
}

/// One datagram if the socket already has one. Does not sleep.
fn udp_try_recv(fd: i64) -> Option<(Vec<u8>, [u8; 4], u16)> {
    udp_recv_mode(fd, true)
}

fn udp_recv_mode(fd: i64, try_once: bool) -> Option<(Vec<u8>, [u8; 4], u16)> {
    if fd < 0 {
        return None;
    }
    let mut buf = [0u8; 1500];
    let mut addr = [0u8; 8];
    let n = sys(
        UDP_RECV,
        fd as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        addr.as_mut_ptr() as u64,
        u64::from(try_once),
        0,
    );
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

fn note_state(up: bool, ip: [u8; 4]) {
    if up {
        post_upstream(b"dns upstream up ", ip, b"");
    } else {
        post_upstream(b"dns upstream down ", ip, b"");
    }
}

fn publish_load(pending: usize) {
    let _ = sys(
        SET_DNS_LOAD,
        pending as u64,
        UPSTREAM_TIMEOUTS.load(Ordering::Relaxed),
        0,
        0,
        0,
        0,
    );
}

fn publish_health(health: &crate::health::Health, ips: &[[u8; 4]]) {
    let mut buf = [0u8; 1 + crate::health::MAX * 5];
    let n = health.pack(ips, &mut buf);
    if n == 0 {
        return;
    }
    let _ = sys(
        SET_UPSTREAM_HEALTH,
        buf.as_ptr() as u64,
        n as u64,
        0,
        0,
        0,
        0,
    );
}

fn note_failover(from: Option<[u8; 4]>, to: [u8; 4]) {
    let Some(from) = from else {
        return;
    };
    let key = u32::from_be_bytes(from) ^ u32::from_be_bytes(to).rotate_left(1);
    if !failover_due(key) {
        return;
    }
    let mut buf = [0u8; 80];
    let mut n = 0;
    let head = b"dns upstream timeout ";
    copy_bytes(&mut buf, &mut n, head);
    push_ip(&mut buf, &mut n, from);
    copy_bytes(&mut buf, &mut n, b" -> ");
    push_ip(&mut buf, &mut n, to);
    post_bytes(&buf[..n]);
}

fn note_exhausted(ip: [u8; 4]) {
    let key = u32::from_be_bytes(ip) ^ 0x4558_0000;
    if !failover_due(key) {
        return;
    }
    post_upstream(b"dns upstream exhausted ", ip, b"");
}

fn note_busy(ip: [u8; 4]) {
    let key = u32::from_be_bytes(ip) ^ 0x4255_0000;
    if !failover_due(key) {
        return;
    }
    post_upstream(b"dns upstream busy ", ip, b"");
}

fn failover_due(key: u32) -> bool {
    let now = time_ms();
    let prev = FAILOVER_KEY.load(Ordering::Relaxed);
    let then = FAILOVER_MS.load(Ordering::Relaxed);
    if prev == key && now.saturating_sub(then) < 5_000 {
        return false;
    }
    FAILOVER_KEY.store(key, Ordering::Relaxed);
    FAILOVER_MS.store(now, Ordering::Relaxed);
    true
}

fn post_upstream(prefix: &[u8], ip: [u8; 4], suffix: &[u8]) {
    let mut buf = [0u8; 80];
    let mut n = 0;
    copy_bytes(&mut buf, &mut n, prefix);
    push_ip(&mut buf, &mut n, ip);
    copy_bytes(&mut buf, &mut n, suffix);
    post_bytes(&buf[..n]);
}

fn copy_bytes(buf: &mut [u8], n: &mut usize, bytes: &[u8]) {
    for &byte in bytes {
        if *n >= buf.len() {
            return;
        }
        buf[*n] = byte;
        *n += 1;
    }
}

fn push_ip(buf: &mut [u8], n: &mut usize, ip: [u8; 4]) {
    for (index, octet) in ip.iter().enumerate() {
        if index > 0 {
            copy_bytes(buf, n, b".");
        }
        push_dec(buf, n, *octet as u32);
    }
}

fn push_dec(buf: &mut [u8], n: &mut usize, mut value: u32) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    if value == 0 {
        copy_bytes(buf, n, b"0");
        return;
    }
    while value > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    copy_bytes(buf, n, &tmp[i..]);
}

fn post_bytes(bytes: &[u8]) {
    let len = bytes.len().min(80);
    let _ = sys(LOG_POST, bytes.as_ptr() as u64, len as u64, 0, 0, 0, 0);
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
