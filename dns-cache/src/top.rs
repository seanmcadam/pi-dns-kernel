//! Fixed-size heavy hitters.
//!
//! Each table holds `N` keys. A repeated key increments its counter. A new
//! key fills an empty slot or, when the table is full, replaces the smallest
//! counter and keeps `min + 1` (the space-saving rule). `N` is a constant, so
//! an update scans a fixed number of slots. Nothing in here allocates.

pub const NAMES: usize = 8;
pub const NAME_LEN: usize = 48;
pub const KINDS: usize = 8;
pub const SOURCES: usize = 8;

pub struct Table<const N: usize, const K: usize> {
    key: [[u8; K]; N],
    len: [u8; N],
    count: [u32; N],
}

impl<const N: usize, const K: usize> Table<N, K> {
    pub const fn new() -> Self {
        Self {
            key: [[0; K]; N],
            len: [0; N],
            count: [0; N],
        }
    }

    pub fn observe(&mut self, key: &[u8]) {
        if key.is_empty() || N == 0 || K == 0 {
            return;
        }
        let key = if key.len() > K { &key[..K] } else { key };
        let mut found = None;
        let mut empty = None;
        let mut min_at = 0;
        let mut min_count = u32::MAX;
        let mut index = 0;
        while index < N {
            if self.count[index] == 0 {
                if empty.is_none() {
                    empty = Some(index);
                }
            } else if self.len[index] as usize == key.len()
                && &self.key[index][..key.len()] == key
            {
                found = Some(index);
                break;
            }
            if self.count[index] < min_count {
                min_count = self.count[index];
                min_at = index;
            }
            index += 1;
        }
        if let Some(index) = found {
            self.count[index] = self.count[index].saturating_add(1);
            return;
        }
        if let Some(index) = empty {
            self.store(index, key, 1);
            return;
        }
        self.store(min_at, key, min_count.saturating_add(1));
    }

    /// Highest count first. Empty slots are left out.
    pub fn rank<'a>(&'a self, out: &mut [Item<'a>]) -> usize {
        let mut order = [0usize; N];
        let mut n = 0;
        while n < N {
            order[n] = n;
            n += 1;
        }
        let mut i = 1;
        while i < N {
            let mut j = i;
            while j > 0 && self.count[order[j - 1]] < self.count[order[j]] {
                order.swap(j - 1, j);
                j -= 1;
            }
            i += 1;
        }
        let mut written = 0;
        let mut i = 0;
        while i < N && written < out.len() {
            let index = order[i];
            if self.count[index] == 0 {
                break;
            }
            let len = self.len[index] as usize;
            out[written] = Item {
                key: &self.key[index][..len],
                count: self.count[index],
            };
            written += 1;
            i += 1;
        }
        written
    }

    fn store(&mut self, index: usize, key: &[u8], count: u32) {
        let mut slot = [0u8; K];
        slot[..key.len()].copy_from_slice(key);
        self.key[index] = slot;
        self.len[index] = key.len() as u8;
        self.count[index] = count;
    }
}

#[derive(Clone, Copy)]
pub struct Item<'a> {
    pub key: &'a [u8],
    pub count: u32,
}

/// Query total, a one-second rate, and the three top-N tables.
///
/// `note` only touches the counters. The rate is computed in `write`, which
/// the publisher calls off the query path.
pub struct Traffic {
    queries: u64,
    mark_queries: u64,
    mark_ms: u64,
    names: Table<NAMES, NAME_LEN>,
    types: Table<KINDS, 2>,
    sources: Table<SOURCES, 4>,
}

impl Traffic {
    pub const fn new() -> Self {
        Self {
            queries: 0,
            mark_queries: 0,
            mark_ms: 0,
            names: Table::new(),
            types: Table::new(),
            sources: Table::new(),
        }
    }

    pub fn note(&mut self, name: &[u8], qtype: u16, ip: &[u8; 4]) {
        self.queries = self.queries.saturating_add(1);
        self.names.observe(name);
        self.types.observe(&qtype.to_be_bytes());
        self.sources.observe(ip);
    }

    pub fn queries(&self) -> u64 {
        self.queries
    }

    /// Version 1 blob. Layout is documented on `write`. Returns 0 if `out` is short.
    pub fn write(&mut self, now_ms: u64, out: &mut [u8]) -> usize {
        let rate = self.rate(now_ms);
        let mut names = [Item { key: b"".as_slice(), count: 0 }; NAMES];
        let mut types = [Item { key: b"".as_slice(), count: 0 }; KINDS];
        let mut sources = [Item { key: b"".as_slice(), count: 0 }; SOURCES];
        let name_n = self.names.rank(&mut names);
        let type_n = self.types.rank(&mut types);
        let src_n = self.sources.rank(&mut sources);
        let need = 4 + 8 + 8 + 1
            + names[..name_n].iter().map(|item| 1 + item.key.len() + 4).sum::<usize>()
            + 1
            + type_n * 6
            + 1
            + src_n * 8;
        if out.len() < need {
            return 0;
        }
        let mut n = 0;
        n = put_u32(out, n, 1);
        n = put_u64(out, n, self.queries);
        n = put_u64(out, n, rate);
        out[n] = name_n as u8;
        n += 1;
        let mut i = 0;
        while i < name_n {
            out[n] = names[i].key.len() as u8;
            n += 1;
            out[n..n + names[i].key.len()].copy_from_slice(names[i].key);
            n += names[i].key.len();
            n = put_u32(out, n, names[i].count);
            i += 1;
        }
        out[n] = type_n as u8;
        n += 1;
        i = 0;
        while i < type_n {
            let key = types[i].key;
            let qtype = if key.len() == 2 {
                u16::from_be_bytes([key[0], key[1]])
            } else {
                0
            };
            n = put_u16(out, n, qtype);
            n = put_u32(out, n, types[i].count);
            i += 1;
        }
        out[n] = src_n as u8;
        n += 1;
        i = 0;
        while i < src_n {
            let key = sources[i].key;
            let mut ip = [0u8; 4];
            let copy = if key.len() < 4 { key.len() } else { 4 };
            ip[..copy].copy_from_slice(&key[..copy]);
            out[n..n + 4].copy_from_slice(&ip);
            n += 4;
            n = put_u32(out, n, sources[i].count);
            i += 1;
        }
        n
    }

    fn rate(&mut self, now_ms: u64) -> u64 {
        if self.mark_ms == 0 {
            self.mark_ms = now_ms;
            self.mark_queries = self.queries;
            return 0;
        }
        let elapsed = now_ms.saturating_sub(self.mark_ms);
        if elapsed < 1000 {
            return 0;
        }
        let delta = self.queries.saturating_sub(self.mark_queries);
        let rate = delta.saturating_mul(1000) / elapsed;
        self.mark_ms = now_ms;
        self.mark_queries = self.queries;
        rate
    }
}

fn put_u16(out: &mut [u8], n: usize, value: u16) -> usize {
    out[n..n + 2].copy_from_slice(&value.to_le_bytes());
    n + 2
}

fn put_u32(out: &mut [u8], n: usize, value: u32) -> usize {
    out[n..n + 4].copy_from_slice(&value.to_le_bytes());
    n + 4
}

fn put_u64(out: &mut [u8], n: usize, value: u64) -> usize {
    out[n..n + 8].copy_from_slice(&value.to_le_bytes());
    n + 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_counts_while_the_table_has_room() {
        let mut table = Table::<4, 8>::new();
        table.observe(b"a.test");
        table.observe(b"a.test");
        table.observe(b"b.test");
        let mut ranked = [Item { key: b"".as_slice(), count: 0 }; 4];
        let n = table.rank(&mut ranked);
        assert_eq!(n, 2);
        assert_eq!(ranked[0].key, b"a.test");
        assert_eq!(ranked[0].count, 2);
        assert_eq!(ranked[1].key, b"b.test");
        assert_eq!(ranked[1].count, 1);
    }

    #[test]
    fn a_new_key_replaces_the_smallest_counter() {
        let mut table = Table::<4, 8>::new();
        table.observe(b"a");
        table.observe(b"b");
        table.observe(b"c");
        table.observe(b"d");
        table.observe(b"a");
        table.observe(b"a");
        table.observe(b"a");
        table.observe(b"a");
        table.observe(b"e");
        let mut ranked = [Item { key: b"".as_slice(), count: 0 }; 4];
        let n = table.rank(&mut ranked);
        assert_eq!(n, 4);
        assert_eq!(ranked[0].key, b"a");
        assert_eq!(ranked[0].count, 5);
        assert_eq!(ranked[1].key, b"e");
        assert_eq!(ranked[1].count, 2);
        assert!(ranked.iter().take(n).all(|item| item.key != b"b"));
    }

    #[test]
    fn long_names_share_the_truncated_key() {
        let mut table = Table::<2, 4>::new();
        table.observe(b"abcdef");
        table.observe(b"abcdZZ");
        let mut ranked = [Item { key: b"".as_slice(), count: 0 }; 2];
        let n = table.rank(&mut ranked);
        assert_eq!(n, 1);
        assert_eq!(ranked[0].key, b"abcd");
        assert_eq!(ranked[0].count, 2);
    }

    #[test]
    fn blob_lists_queries_names_types_and_sources() {
        let mut traffic = Traffic::new();
        traffic.note(b"alpha.console.local", 1, &[10, 0, 2, 2]);
        traffic.note(b"alpha.console.local", 1, &[10, 0, 2, 2]);
        traffic.note(b"beta.console.local", 28, &[10, 0, 2, 15]);
        let mut out = [0u8; 576];
        let n = traffic.write(0, &mut out);
        assert!(n > 20);
        assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(out[4..12].try_into().unwrap()), 3);
        let name_n = out[20] as usize;
        assert_eq!(name_n, 2);
        let mut cursor = 21;
        let len = out[cursor] as usize;
        cursor += 1;
        assert_eq!(&out[cursor..cursor + len], b"alpha.console.local");
        cursor += len;
        assert_eq!(u32::from_le_bytes(out[cursor..cursor + 4].try_into().unwrap()), 2);
    }
}
