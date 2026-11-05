#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
extern crate std;

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

#[derive(Debug, Clone, PartialEq)]
pub struct CacheEntry {
    pub name: String,
    pub record_type: u16,
    pub data: Vec<u8>,
    pub ttl: u32,
    pub timestamp: u64,
}

pub struct DnsCache {
    entries: Vec<CacheEntry>,
    max_size: usize,
}

impl DnsCache {
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: Vec::new(),
            max_size,
        }
    }

    pub fn get(&self, name: &str, record_type: u16, current_time: u64) -> Option<&CacheEntry> {
        self.entries.iter().find(|entry| {
            entry.name == name
                && entry.record_type == record_type
                && !Self::is_expired(entry, current_time)
        })
    }

    pub fn insert(&mut self, entry: CacheEntry) {
        // Remove expired entries first
        self.entries
            .retain(|e| !Self::is_expired(e, entry.timestamp));

        // Remove existing entry for this name/type
        self.entries
            .retain(|e| !(e.name == entry.name && e.record_type == entry.record_type));

        // Add new entry
        self.entries.push(entry);

        // Evict oldest if over limit
        if self.entries.len() > self.max_size {
            self.entries.remove(0);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_expired(entry: &CacheEntry, current_time: u64) -> bool {
        let elapsed = current_time.saturating_sub(entry.timestamp);
        elapsed > entry.ttl as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_insert_and_get() {
        let mut cache = DnsCache::new(10);

        let entry = CacheEntry {
            name: "example.com".into(),
            record_type: 1, // A record
            data: vec![93, 184, 216, 34],
            ttl: 300,
            timestamp: 1000,
        };

        cache.insert(entry.clone());

        let retrieved = cache.get("example.com", 1, 1100);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().data, vec![93, 184, 216, 34]);
    }

    #[test]
    fn test_cache_expiry() {
        let mut cache = DnsCache::new(10);

        let entry = CacheEntry {
            name: "example.com".into(),
            record_type: 1,
            data: vec![93, 184, 216, 34],
            ttl: 300,
            timestamp: 1000,
        };

        cache.insert(entry);

        // Within TTL
        assert!(cache.get("example.com", 1, 1200).is_some());

        // After TTL
        assert!(cache.get("example.com", 1, 1400).is_none());
    }

    #[test]
    fn test_cache_eviction() {
        let mut cache = DnsCache::new(2);

        cache.insert(CacheEntry {
            name: "example1.com".into(),
            record_type: 1,
            data: vec![1, 2, 3, 4],
            ttl: 300,
            timestamp: 1000,
        });

        cache.insert(CacheEntry {
            name: "example2.com".into(),
            record_type: 1,
            data: vec![5, 6, 7, 8],
            ttl: 300,
            timestamp: 1000,
        });

        cache.insert(CacheEntry {
            name: "example3.com".into(),
            record_type: 1,
            data: vec![9, 10, 11, 12],
            ttl: 300,
            timestamp: 1000,
        });

        // First entry should be evicted
        assert!(cache.get("example1.com", 1, 1100).is_none());
        assert!(cache.get("example2.com", 1, 1100).is_some());
        assert!(cache.get("example3.com", 1, 1100).is_some());
    }
}
