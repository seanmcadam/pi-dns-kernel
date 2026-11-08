//! DNS state the kernel still owns.
//!
//! Packet parsing, the cache, upstream retries, and the blocklist check run
//! in the EL0 program. This module keeps the counters and the boot log. The
//! user program publishes the cache size through a syscall so `/status` can
//! keep reporting it.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::config_manager::DnsConfig;

static CACHE_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static CACHE_STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static PLANT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

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
