//! Fixed DNS response cache.
//!
//! Every slot is reserved when the cache is created. Insert, lookup, expiry,
//! and eviction never allocate. A response larger than [`RESPONSE_MAX`] bytes,
//! or a name longer than [`NAME_MAX`] bytes, is not stored.
//!
//! The key is the lowercased query name, the query type, and the query class.
//! Buckets are a fixed array. Colliding keys chain through slot indices.
//!
//! Eviction is least-recently-used. A hit or a refresh moves that slot to the
//! head. When the free list is empty, the tail (the entry that has gone the
//! longest without a hit or a refresh) is reused. Expired slots are not
//! evicted that way: [`DnsCache::reap`] walks the live list and returns them
//! to the free list. An insert that finds a free list empty reaps once before
//! it evicts, so a dead entry is not kept in front of a live one.
//!
//! `expiry` is an absolute hardware-counter value. The caller adds
//! `ttl_seconds * counter_frequency` to the current counter. A lookup at
//! `now` hits only while `now < expiry`.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod top;

use alloc::vec::Vec;

/// Name bytes stored in a slot, not counting a trailing dot.
pub const NAME_MAX: usize = 255;
/// Raw response bytes stored in a slot. Larger replies are not cached.
pub const RESPONSE_MAX: usize = 512;

const NONE: u32 = u32::MAX;

/// One preallocated record. The layout is fixed so the reserved size is
/// `slots * 800 + buckets * 4` bytes.
#[repr(C)]
struct Slot {
    expiry: u64,
    hash_next: u32,
    lru_prev: u32,
    lru_next: u32,
    free_next: u32,
    response_len: u16,
    qtype: u16,
    qclass: u16,
    name_len: u8,
    live: u8,
    name: [u8; NAME_MAX],
    _pad: u8,
    response: [u8; RESPONSE_MAX],
}

/// Counters and the reserved size. `bytes` is the pool plus the bucket array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheStats {
    pub slots: u64,
    pub used: u64,
    pub free: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub reaped: u64,
    pub bytes: u64,
}

pub struct DnsCache {
    slots: Vec<Slot>,
    buckets: Vec<u32>,
    free_head: u32,
    lru_head: u32,
    lru_tail: u32,
    used: u32,
    hits: u64,
    misses: u64,
    evictions: u64,
    reaped: u64,
}

impl DnsCache {
    /// Reserve `slots` records and one bucket per slot, rounded up to a power
    /// of two. This is the only allocation.
    pub fn new(slots: usize) -> Self {
        let slots = slots.max(1);
        let mut entries = Vec::with_capacity(slots);
        let mut index = 0u32;
        while (index as usize) < slots {
            let mut slot = blank_slot();
            slot.free_next = if (index as usize) + 1 == slots {
                NONE
            } else {
                index + 1
            };
            entries.push(slot);
            index += 1;
        }
        let bucket_count = buckets_for(slots);
        let mut buckets = Vec::with_capacity(bucket_count);
        let mut bucket = 0;
        while bucket < bucket_count {
            buckets.push(NONE);
            bucket += 1;
        }
        Self {
            slots: entries,
            buckets,
            free_head: 0,
            lru_head: NONE,
            lru_tail: NONE,
            used: 0,
            hits: 0,
            misses: 0,
            evictions: 0,
            reaped: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.used as usize
    }

    pub fn stats(&self) -> CacheStats {
        let reserved = self.slots.len() * core::mem::size_of::<Slot>()
            + self.buckets.len() * core::mem::size_of::<u32>();
        CacheStats {
            slots: self.slots.len() as u64,
            used: self.used as u64,
            free: (self.slots.len() as u32 - self.used) as u64,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
            reaped: self.reaped,
            bytes: reserved as u64,
        }
    }

    /// Copy a live response into `out`. Expired and unknown keys miss.
    pub fn get(
        &mut self,
        name: &str,
        qtype: u16,
        qclass: u16,
        now: u64,
        out: &mut [u8],
    ) -> Option<usize> {
        let mut key = [0u8; NAME_MAX];
        let Some(len) = normalize(name, &mut key) else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        let found = self.find(&key[..len], qtype, qclass);
        let Some(index) = found else {
            self.misses = self.misses.saturating_add(1);
            return None;
        };
        if self.slots[index as usize].expiry <= now {
            self.misses = self.misses.saturating_add(1);
            return None;
        }
        let n = self.slots[index as usize].response_len as usize;
        if n > out.len() {
            self.misses = self.misses.saturating_add(1);
            return None;
        }
        out[..n].copy_from_slice(&self.slots[index as usize].response[..n]);
        self.touch(index);
        self.hits = self.hits.saturating_add(1);
        Some(n)
    }

    /// Store `data` until `expiry`. Returns false when the name or the
    /// response does not fit, or when `expiry` is already due. A repeated key
    /// refreshes the same slot.
    pub fn insert(
        &mut self,
        name: &str,
        qtype: u16,
        qclass: u16,
        data: &[u8],
        expiry: u64,
        now: u64,
    ) -> bool {
        if data.is_empty() || data.len() > RESPONSE_MAX || expiry <= now {
            return false;
        }
        let mut key = [0u8; NAME_MAX];
        let Some(len) = normalize(name, &mut key) else {
            return false;
        };
        if let Some(index) = self.find(&key[..len], qtype, qclass) {
            self.write_slot(index, &key[..len], qtype, qclass, data, expiry);
            self.touch(index);
            return true;
        }
        if self.free_head == NONE {
            self.reap(now);
        }
        if self.free_head == NONE && !self.evict_lru() {
            return false;
        }
        let Some(index) = self.pop_free() else {
            return false;
        };
        self.write_slot(index, &key[..len], qtype, qclass, data, expiry);
        self.link_hash(index, &key[..len], qtype, qclass);
        self.push_mru(index);
        self.used = self.used.saturating_add(1);
        true
    }

    /// Return expired slots to the free list. The count is added to `reaped`.
    pub fn reap(&mut self, now: u64) -> u32 {
        let mut freed = 0u32;
        let mut cursor = self.lru_head;
        while cursor != NONE {
            let next = self.slots[cursor as usize].lru_next;
            if self.slots[cursor as usize].expiry <= now {
                self.unlink_hash(cursor);
                self.unlink_lru(cursor);
                self.push_free(cursor);
                self.used = self.used.saturating_sub(1);
                self.reaped = self.reaped.saturating_add(1);
                freed = freed.saturating_add(1);
            }
            cursor = next;
        }
        freed
    }

    fn find(&self, name: &[u8], qtype: u16, qclass: u16) -> Option<u32> {
        let mut cursor = self.buckets[self.bucket(name, qtype, qclass)];
        while cursor != NONE {
            let slot = &self.slots[cursor as usize];
            if slot.live == 1
                && slot.qtype == qtype
                && slot.qclass == qclass
                && slot.name_len as usize == name.len()
                && slot.name[..name.len()] == *name
            {
                return Some(cursor);
            }
            cursor = slot.hash_next;
        }
        None
    }

    fn write_slot(
        &mut self,
        index: u32,
        name: &[u8],
        qtype: u16,
        qclass: u16,
        data: &[u8],
        expiry: u64,
    ) {
        let slot = &mut self.slots[index as usize];
        slot.expiry = expiry;
        slot.qtype = qtype;
        slot.qclass = qclass;
        slot.name_len = name.len() as u8;
        slot.live = 1;
        slot.name[..name.len()].copy_from_slice(name);
        slot.response_len = data.len() as u16;
        slot.response[..data.len()].copy_from_slice(data);
    }

    fn evict_lru(&mut self) -> bool {
        let index = self.lru_tail;
        if index == NONE {
            return false;
        }
        self.unlink_hash(index);
        self.unlink_lru(index);
        self.push_free(index);
        self.used = self.used.saturating_sub(1);
        self.evictions = self.evictions.saturating_add(1);
        true
    }

    fn pop_free(&mut self) -> Option<u32> {
        let index = self.free_head;
        if index == NONE {
            return None;
        }
        self.free_head = self.slots[index as usize].free_next;
        self.slots[index as usize].free_next = NONE;
        self.slots[index as usize].hash_next = NONE;
        Some(index)
    }

    fn push_free(&mut self, index: u32) {
        let slot = &mut self.slots[index as usize];
        slot.live = 0;
        slot.name_len = 0;
        slot.response_len = 0;
        slot.hash_next = NONE;
        slot.lru_prev = NONE;
        slot.lru_next = NONE;
        slot.free_next = self.free_head;
        self.free_head = index;
    }

    fn link_hash(&mut self, index: u32, name: &[u8], qtype: u16, qclass: u16) {
        let bucket = self.bucket(name, qtype, qclass);
        self.slots[index as usize].hash_next = self.buckets[bucket];
        self.buckets[bucket] = index;
    }

    fn unlink_hash(&mut self, index: u32) {
        let slot = &self.slots[index as usize];
        let qtype = slot.qtype;
        let qclass = slot.qclass;
        let len = slot.name_len as usize;
        let mut name = [0u8; NAME_MAX];
        name[..len].copy_from_slice(&slot.name[..len]);
        let bucket = self.bucket(&name[..len], qtype, qclass);
        let mut prev = NONE;
        let mut cursor = self.buckets[bucket];
        while cursor != NONE {
            if cursor == index {
                let next = self.slots[cursor as usize].hash_next;
                if prev == NONE {
                    self.buckets[bucket] = next;
                } else {
                    self.slots[prev as usize].hash_next = next;
                }
                self.slots[index as usize].hash_next = NONE;
                return;
            }
            prev = cursor;
            cursor = self.slots[cursor as usize].hash_next;
        }
    }

    fn touch(&mut self, index: u32) {
        if self.lru_head == index {
            return;
        }
        self.unlink_lru(index);
        self.push_mru(index);
    }

    fn push_mru(&mut self, index: u32) {
        self.slots[index as usize].lru_prev = NONE;
        self.slots[index as usize].lru_next = self.lru_head;
        if self.lru_head != NONE {
            self.slots[self.lru_head as usize].lru_prev = index;
        } else {
            self.lru_tail = index;
        }
        self.lru_head = index;
    }

    fn unlink_lru(&mut self, index: u32) {
        let prev = self.slots[index as usize].lru_prev;
        let next = self.slots[index as usize].lru_next;
        if prev == NONE {
            self.lru_head = next;
        } else {
            self.slots[prev as usize].lru_next = next;
        }
        if next == NONE {
            self.lru_tail = prev;
        } else {
            self.slots[next as usize].lru_prev = prev;
        }
        self.slots[index as usize].lru_prev = NONE;
        self.slots[index as usize].lru_next = NONE;
    }

    fn bucket(&self, name: &[u8], qtype: u16, qclass: u16) -> usize {
        let mask = (self.buckets.len() - 1) as u32;
        (hash_key(name, qtype, qclass) & mask) as usize
    }
}

fn buckets_for(slots: usize) -> usize {
    let mut n = 1usize;
    while n < slots {
        n <<= 1;
    }
    n
}

fn hash_key(name: &[u8], qtype: u16, qclass: u16) -> u32 {
    let mut hash = 2166136261u32;
    for byte in name {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash ^= qtype as u32;
    hash = hash.wrapping_mul(16777619);
    hash ^= (qclass as u32) << 16;
    hash.wrapping_mul(16777619)
}

fn normalize(name: &str, out: &mut [u8; NAME_MAX]) -> Option<usize> {
    let bytes = name.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b'.' {
        end -= 1;
    }
    if end == 0 || end > NAME_MAX {
        return None;
    }
    let mut index = 0;
    while index < end {
        let byte = bytes[index];
        out[index] = if byte.is_ascii_uppercase() {
            byte + 32
        } else {
            byte
        };
        index += 1;
    }
    Some(end)
}

fn blank_slot() -> Slot {
    Slot {
        expiry: 0,
        hash_next: NONE,
        lru_prev: NONE,
        lru_next: NONE,
        free_next: NONE,
        response_len: 0,
        qtype: 0,
        qclass: 0,
        name_len: 0,
        live: 0,
        name: [0; NAME_MAX],
        _pad: 0,
        response: [0; RESPONSE_MAX],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn take(cache: &mut DnsCache, name: &str, now: u64) -> Option<Vec<u8>> {
        let mut buf = [0u8; RESPONSE_MAX];
        cache
            .get(name, 1, 1, now, &mut buf)
            .map(|n| buf[..n].to_vec())
    }

    #[test]
    fn slot_is_eight_hundred_bytes() {
        assert_eq!(core::mem::size_of::<Slot>(), 800);
    }

    #[test]
    fn hit_miss_and_refresh() {
        let mut cache = DnsCache::new(4);
        assert!(cache.insert("Example.COM.", 1, 1, &[9, 9, 9, 9], 500, 100));
        assert_eq!(take(&mut cache, "example.com", 200).unwrap(), vec![9, 9, 9, 9]);
        assert!(take(&mut cache, "example.com", 500).is_none());
        assert!(cache.insert("example.com", 1, 1, &[1, 2], 800, 500));
        assert_eq!(cache.len(), 1);
        assert_eq!(take(&mut cache, "example.com", 700).unwrap(), vec![1, 2]);
        let stats = cache.stats();
        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!(stats.bytes >= 800);
    }

    #[test]
    fn hash_collisions_keep_both_keys() {
        let mut cache = DnsCache::new(8);
        let mut first = String::new();
        let mut second = String::new();
        let mut bucket = 0usize;
        let mut n = 0u16;
        while n < 400 && second.is_empty() {
            let name = alloc::format!("n{n}.test");
            let mut key = [0u8; NAME_MAX];
            let len = normalize(&name, &mut key).unwrap();
            let at = cache.bucket(&key[..len], 1, 1);
            if first.is_empty() {
                first = name;
                bucket = at;
            } else if at == bucket {
                second = name;
            }
            n += 1;
        }
        assert!(!second.is_empty(), "no colliding pair");
        assert!(cache.insert(&first, 1, 1, &[1], 1000, 0));
        assert!(cache.insert(&second, 1, 1, &[2], 1000, 0));
        assert_eq!(take(&mut cache, &first, 10).unwrap(), vec![1]);
        assert_eq!(take(&mut cache, &second, 10).unwrap(), vec![2]);
        assert!(cache.insert(&first, 1, 2, &[3], 1000, 0));
        assert_eq!(cache.len(), 3);
    }

    #[test]
    fn free_list_reuses_reaped_slots() {
        let mut cache = DnsCache::new(2);
        assert!(cache.insert("a.test", 1, 1, &[1], 50, 0));
        assert!(cache.insert("b.test", 1, 1, &[2], 50, 0));
        assert_eq!(cache.reap(50), 2);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.stats().free, 2);
        assert!(cache.insert("c.test", 1, 1, &[3], 200, 50));
        assert_eq!(cache.stats().evictions, 0);
        assert_eq!(cache.len(), 1);
        assert!(take(&mut cache, "c.test", 60).is_some());
        assert!(take(&mut cache, "a.test", 60).is_none());
    }

    #[test]
    fn expiry_misses_until_the_reaper_runs() {
        let mut cache = DnsCache::new(2);
        assert!(cache.insert("gone.test", 1, 1, &[4, 5], 100, 0));
        assert!(take(&mut cache, "gone.test", 99).is_some());
        assert!(take(&mut cache, "gone.test", 100).is_none());
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.reap(100), 1);
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.stats().reaped, 1);
    }

    #[test]
    fn lru_evicts_the_cold_entry() {
        let mut cache = DnsCache::new(2);
        assert!(cache.insert("old.test", 1, 1, &[1], 900, 0));
        assert!(cache.insert("warm.test", 1, 1, &[2], 900, 0));
        assert!(take(&mut cache, "old.test", 10).is_some());
        assert!(cache.insert("new.test", 1, 1, &[3], 900, 10));
        assert!(take(&mut cache, "warm.test", 20).is_none());
        assert!(take(&mut cache, "old.test", 20).is_some());
        assert!(take(&mut cache, "new.test", 20).is_some());
        assert_eq!(cache.stats().evictions, 1);
    }

    #[test]
    fn oversized_name_and_response_are_not_stored() {
        let mut cache = DnsCache::new(2);
        let big = [7u8; RESPONSE_MAX + 1];
        assert!(!cache.insert("fit.test", 1, 1, &big, 100, 0));
        let long = "a".repeat(NAME_MAX + 1);
        assert!(!cache.insert(&long, 1, 1, &[1], 100, 0));
        assert!(!cache.insert("late.test", 1, 1, &[1], 5, 5));
        assert_eq!(cache.len(), 0);
    }
}
