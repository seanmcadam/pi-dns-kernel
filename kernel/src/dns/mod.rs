//! DNS state the kernel still owns.
//!
//! Packet parsing, the cache, upstream retries, and the blocklist check run
//! in the EL0 program. This module keeps the counters and the boot log. The
//! user program publishes the cache size through a syscall so `/status` can
//! keep reporting it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::config_manager::DnsConfig;

static CACHE_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static DNS_INFLIGHT: AtomicU64 = AtomicU64::new(0);
static DNS_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
static DNS_FORWARD_FULL: AtomicU64 = AtomicU64::new(0);
static DNS_REPLY_DROPS: AtomicU64 = AtomicU64::new(0);
static CACHE_STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static PLANT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

const TOP_CAP: usize = 576;
static TOP_GEN: AtomicU32 = AtomicU32::new(0);
static TOP_LEN: AtomicU32 = AtomicU32::new(0);
/// Packed upstream health: count, then count times (ipv4, 0|1). The DNS
/// task publishes it. `/status` reads it. Odd generation means a write
/// is in progress.
const HEALTH_CAP: usize = 1 + 8 * 5;
static HEALTH_GEN: AtomicU32 = AtomicU32::new(0);
static HEALTH_LEN: AtomicU32 = AtomicU32::new(0);
static HEALTH_BUF: HealthBuf = HealthBuf {
    bytes: UnsafeCell::new([0; HEALTH_CAP]),
};

struct HealthBuf {
    bytes: UnsafeCell<[u8; HEALTH_CAP]>,
}

unsafe impl Sync for HealthBuf {}
static TOP_BUF: TopBuf = TopBuf {
    bytes: UnsafeCell::new([0; TOP_CAP]),
};

struct TopBuf {
    bytes: UnsafeCell<[u8; TOP_CAP]>,
}

unsafe impl Sync for TopBuf {}

pub fn init(config: &DnsConfig) {
    log::info!("DNS server initialized");
    log::info!("Local records: {}", config.records.len());
    log::info!("Upstream resolvers: {:?}", config.upstreams);
    if !config.blocklist.is_empty() {
        log::info!("Blocklist entries: {}", config.blocklist.len());
    }
    CACHE_ENTRIES.store(0, Ordering::Relaxed);
}

pub fn set_cache_entries(count: usize) {
    CACHE_ENTRIES.store(count, Ordering::Relaxed);
}

pub fn cache_entries() -> usize {
    CACHE_ENTRIES.load(Ordering::Relaxed)
}

/// One-shot request from `/debug/cache-plant`. `ttl` is 1..=5 seconds.
pub fn plant_request(ttl: u32) {
    PLANT.store(ttl, Ordering::Release);
}

pub fn plant_take() -> u32 {
    PLANT.swap(0, Ordering::AcqRel)
}

/// `words` is slots, used, free, hits, misses, evictions, reaped, bytes.
pub fn set_cache_stats(words: &[u64; 8]) {
    let mut index = 0;
    while index < 8 {
        CACHE_STATS[index].store(words[index], Ordering::Relaxed);
        index += 1;
    }
    CACHE_ENTRIES.store(words[1] as usize, Ordering::Relaxed);
}

pub fn write_cache_stats(out: &mut [u8]) -> usize {
    let mut cursor = 0;
    cursor = push(out, cursor, b"cache slots=");
    cursor = push_dec(out, cursor, CACHE_STATS[0].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" used=");
    cursor = push_dec(out, cursor, CACHE_STATS[1].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" free=");
    cursor = push_dec(out, cursor, CACHE_STATS[2].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" hits=");
    cursor = push_dec(out, cursor, CACHE_STATS[3].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" misses=");
    cursor = push_dec(out, cursor, CACHE_STATS[4].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" evictions=");
    cursor = push_dec(out, cursor, CACHE_STATS[5].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" reaped=");
    cursor = push_dec(out, cursor, CACHE_STATS[6].load(Ordering::Relaxed));
    cursor = push(out, cursor, b" bytes=");
    cursor = push_dec(out, cursor, CACHE_STATS[7].load(Ordering::Relaxed));
    cursor = push(out, cursor, b"\n");
    cursor
}

/// Pending forwards, upstream timeouts, times the forward table was full,
/// and client replies that could not be queued. From the DNS task.
pub fn set_load(inflight: u64, timeouts: u64, forward_full: u64, reply_drops: u64) {
    DNS_INFLIGHT.store(inflight, Ordering::Relaxed);
    DNS_TIMEOUTS.store(timeouts, Ordering::Relaxed);
    DNS_FORWARD_FULL.store(forward_full, Ordering::Relaxed);
    DNS_REPLY_DROPS.store(reply_drops, Ordering::Relaxed);
}

pub fn load() -> (u64, u64, u64, u64) {
    (
        DNS_INFLIGHT.load(Ordering::Relaxed),
        DNS_TIMEOUTS.load(Ordering::Relaxed),
        DNS_FORWARD_FULL.load(Ordering::Relaxed),
        DNS_REPLY_DROPS.load(Ordering::Relaxed),
    )
}

/// Replace the packed upstream health blob. `bytes` is `count` then
/// `count` records of 4 address bytes and one up flag.
pub fn set_upstream_health(bytes: &[u8]) {
    if bytes.is_empty() || bytes.len() > HEALTH_CAP {
        return;
    }
    let count = bytes[0] as usize;
    if count > 8 || bytes.len() < 1 + count * 5 {
        return;
    }
    let n = 1 + count * 5;
    let gen = HEALTH_GEN.load(Ordering::Relaxed) & !1;
    HEALTH_GEN.store(gen.wrapping_add(1), Ordering::Release);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), HEALTH_BUF.bytes.get().cast::<u8>(), n);
    }
    HEALTH_LEN.store(n as u32, Ordering::Release);
    HEALTH_GEN.store(gen.wrapping_add(2), Ordering::Release);
}

/// Copy the packed health blob. Empty until the DNS task publishes it.
pub fn copy_upstream_health(out: &mut [u8]) -> usize {
    let mut spins = 0;
    while spins < 4 {
        let gen = HEALTH_GEN.load(Ordering::Acquire);
        if gen & 1 != 0 {
            spins += 1;
            continue;
        }
        let n = HEALTH_LEN.load(Ordering::Acquire) as usize;
        if n == 0 || n > out.len() || n > HEALTH_CAP {
            return 0;
        }
        unsafe {
            core::ptr::copy_nonoverlapping(
                HEALTH_BUF.bytes.get().cast::<u8>(),
                out.as_mut_ptr(),
                n,
            );
        }
        if HEALTH_GEN.load(Ordering::Acquire) == gen {
            return n;
        }
        spins += 1;
    }
    0
}

/// Store the version-1 top-N blob from dns-el0. The writer is the DNS task.
/// Readers retry if a publish is in progress. No lock, so this stays off the
/// scheduler and the FD lock order.
pub fn set_top(bytes: &[u8]) {
    let n = if bytes.len() < TOP_CAP {
        bytes.len()
    } else {
        TOP_CAP
    };
    let gen = TOP_GEN.load(Ordering::Relaxed) & !1;
    TOP_GEN.store(gen.wrapping_add(1), Ordering::Release);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), TOP_BUF.bytes.get().cast::<u8>(), n);
    }
    TOP_LEN.store(n as u32, Ordering::Release);
    TOP_GEN.store(gen.wrapping_add(2), Ordering::Release);
}

/// `queries`, `hitrate`, and the three `top-` lines. Empty until the first publish.
pub fn write_top(out: &mut [u8]) -> usize {
    let mut raw = [0u8; TOP_CAP];
    let n = copy_top(&mut raw);
    let Some(view) = parse_top(&raw[..n]) else {
        return push(out, 0, b"queries=0 rate=0\nhitrate=0\n");
    };
    let hits = CACHE_STATS[3].load(Ordering::Relaxed);
    let misses = CACHE_STATS[4].load(Ordering::Relaxed);
    let hitrate = if hits + misses == 0 {
        0
    } else {
        hits.saturating_mul(100) / (hits + misses)
    };
    let mut cursor = 0;
    cursor = push(out, cursor, b"queries=");
    cursor = push_dec(out, cursor, view.queries);
    cursor = push(out, cursor, b" rate=");
    cursor = push_dec(out, cursor, view.rate);
    cursor = push(out, cursor, b"\nhitrate=");
    cursor = push_dec(out, cursor, hitrate);
    cursor = push(out, cursor, b"\ntop-name");
    let mut i = 0;
    while i < view.name_n {
        cursor = push(out, cursor, b" ");
        cursor = push(out, cursor, &view.names[i].0[..view.names[i].1]);
        cursor = push(out, cursor, b"=");
        cursor = push_dec(out, cursor, view.names[i].2 as u64);
        i += 1;
    }
    cursor = push(out, cursor, b"\ntop-type");
    i = 0;
    while i < view.type_n {
        cursor = push(out, cursor, b" ");
        cursor = push_type(out, cursor, view.types[i].0);
        cursor = push(out, cursor, b"=");
        cursor = push_dec(out, cursor, view.types[i].1 as u64);
        i += 1;
    }
    cursor = push(out, cursor, b"\ntop-src");
    i = 0;
    while i < view.src_n {
        cursor = push(out, cursor, b" ");
        cursor = push_ip(out, cursor, view.srcs[i].0);
        cursor = push(out, cursor, b"=");
        cursor = push_dec(out, cursor, view.srcs[i].1 as u64);
        i += 1;
    }
    push(out, cursor, b"\n")
}

fn copy_top(out: &mut [u8]) -> usize {
    let mut spins = 0;
    loop {
        let gen = TOP_GEN.load(Ordering::Acquire);
        if gen & 1 != 0 {
            spins += 1;
            if spins > 8 {
                return 0;
            }
            core::hint::spin_loop();
            continue;
        }
        let n = TOP_LEN.load(Ordering::Acquire) as usize;
        let n = if n < out.len() { n } else { out.len() };
        let n = if n < TOP_CAP { n } else { TOP_CAP };
        unsafe {
            core::ptr::copy_nonoverlapping(TOP_BUF.bytes.get().cast::<u8>(), out.as_mut_ptr(), n);
        }
        if TOP_GEN.load(Ordering::Acquire) == gen {
            return n;
        }
    }
}

struct TopView {
    queries: u64,
    rate: u64,
    names: [([u8; 48], usize, u32); 8],
    name_n: usize,
    types: [(u16, u32); 8],
    type_n: usize,
    srcs: [([u8; 4], u32); 8],
    src_n: usize,
}

fn parse_top(raw: &[u8]) -> Option<TopView> {
    if raw.len() < 21 || u32::from_le_bytes(raw[0..4].try_into().ok()?) != 1 {
        return None;
    }
    let mut view = TopView {
        queries: u64::from_le_bytes(raw[4..12].try_into().ok()?),
        rate: u64::from_le_bytes(raw[12..20].try_into().ok()?),
        names: [([0; 48], 0, 0); 8],
        name_n: 0,
        types: [(0, 0); 8],
        type_n: 0,
        srcs: [([0; 4], 0); 8],
        src_n: 0,
    };
    let mut cursor = 20;
    let name_n = *raw.get(cursor)? as usize;
    cursor += 1;
    if name_n > 8 {
        return None;
    }
    let mut i = 0;
    while i < name_n {
        let len = *raw.get(cursor)? as usize;
        cursor += 1;
        if len == 0 || len > 48 || cursor + len + 4 > raw.len() {
            return None;
        }
        let mut name = [0u8; 48];
        name[..len].copy_from_slice(&raw[cursor..cursor + len]);
        if name[..len]
            .iter()
            .any(|byte| *byte <= 0x20 || *byte >= 0x7f || *byte == b'=')
        {
            return None;
        }
        cursor += len;
        let count = u32::from_le_bytes(raw[cursor..cursor + 4].try_into().ok()?);
        cursor += 4;
        view.names[i] = (name, len, count);
        i += 1;
    }
    view.name_n = name_n;
    let type_n = *raw.get(cursor)? as usize;
    cursor += 1;
    if type_n > 8 {
        return None;
    }
    i = 0;
    while i < type_n {
        if cursor + 6 > raw.len() {
            return None;
        }
        let qtype = u16::from_le_bytes(raw[cursor..cursor + 2].try_into().ok()?);
        cursor += 2;
        let count = u32::from_le_bytes(raw[cursor..cursor + 4].try_into().ok()?);
        cursor += 4;
        view.types[i] = (qtype, count);
        i += 1;
    }
    view.type_n = type_n;
    let src_n = *raw.get(cursor)? as usize;
    cursor += 1;
    if src_n > 8 {
        return None;
    }
    i = 0;
    while i < src_n {
        if cursor + 8 > raw.len() {
            return None;
        }
        let mut ip = [0u8; 4];
        ip.copy_from_slice(&raw[cursor..cursor + 4]);
        cursor += 4;
        let count = u32::from_le_bytes(raw[cursor..cursor + 4].try_into().ok()?);
        cursor += 4;
        view.srcs[i] = (ip, count);
        i += 1;
    }
    view.src_n = src_n;
    let _ = cursor;
    Some(view)
}

fn push_type(out: &mut [u8], cursor: usize, qtype: u16) -> usize {
    let name = match qtype {
        1 => "A",
        2 => "NS",
        5 => "CNAME",
        6 => "SOA",
        12 => "PTR",
        15 => "MX",
        16 => "TXT",
        28 => "AAAA",
        _ => "",
    };
    if !name.is_empty() {
        push(out, cursor, name.as_bytes())
    } else {
        push_dec(out, cursor, qtype as u64)
    }
}

fn push_ip(out: &mut [u8], mut cursor: usize, ip: [u8; 4]) -> usize {
    let mut i = 0;
    while i < 4 {
        if i > 0 {
            cursor = push(out, cursor, b".");
        }
        cursor = push_dec(out, cursor, ip[i] as u64);
        i += 1;
    }
    cursor
}

fn push(out: &mut [u8], mut cursor: usize, text: &[u8]) -> usize {
    for byte in text {
        if cursor < out.len() {
            out[cursor] = *byte;
            cursor += 1;
        }
    }
    cursor
}

fn push_dec(out: &mut [u8], cursor: usize, mut value: u64) -> usize {
    let mut digits = [0u8; 20];
    let mut n = 0;
    if value == 0 {
        return push(out, cursor, b"0");
    }
    while value > 0 && n < digits.len() {
        digits[n] = b'0' + (value % 10) as u8;
        value /= 10;
        n += 1;
    }
    let mut cursor = cursor;
    while n > 0 {
        n -= 1;
        cursor = push(out, cursor, &digits[n..n + 1]);
    }
    cursor
}
