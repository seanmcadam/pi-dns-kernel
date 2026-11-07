//! DNS state the kernel still owns.
//!
//! Packet parsing, the cache, upstream retries, and the blocklist check run
//! in the EL0 program. This module keeps the counters and the boot log. The
//! user program publishes the cache size through a syscall so `/status` can
//! keep reporting it.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::config_manager::DnsConfig;

static CACHE_ENTRIES: AtomicUsize = AtomicUsize::new(0);

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
