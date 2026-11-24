//! Block-list names, list parsing, and the open-addressed domain table.
//!
//! The table lives in a caller-provided region. A generation of 0 means
//! nothing has been published (the blocker is down or still loading). An
//! odd generation means a rebuild is in progress. A lookup then fails open:
//! it does not report a hit. A published generation is even and non-zero,
//! and a lookup still fails open when that generation changes mid-probe.

#![cfg_attr(not(test), no_std)]

pub const URL_MAX: usize = 8;
pub const URL_LEN_MAX: usize = 160;
/// One downloaded list. Larger bodies are refused and the old file is kept.
pub const LIST_MAX_BYTES: usize = 1024 * 1024;
/// Names taken from one list. The table's own load cap still applies.
pub const LIST_MAX_ENTRIES: usize = 400_000;
pub const ALLOW_MAX: usize = 32;
pub const REFRESH_DEFAULT: u64 = 86_400;
pub const REFRESH_MIN: u64 = 3_600;
pub const REFRESH_MAX: u64 = 604_800;
pub const PROBE_MAX: usize = 32;
pub const INDEX_NAME: &str = "BLOCK.IDX";
pub const TEMP_NAME: &str = "BLOCK.TMP";
pub const CA_NAME: &str = "TESTCA.DER";

const HEADER: usize = 32;
const MAGIC: u64 = 0x3148_5442_4B4C_42;

pub fn fnv1a32(data: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in data {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

pub fn fnv1a64(data: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in data {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub fn name_hash(name: &str) -> u64 {
    let hash = fnv1a64(name.as_bytes());
    if hash == 0 { 1 } else { hash }
}

/// `XXXXXXXX.LST`, eight uppercase hex digits from FNV-1a of the URL.
pub fn list_file(url: &str) -> [u8; 12] {
    let hash = fnv1a32(url.as_bytes());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = [0u8; 12];
    let mut i = 0;
    while i < 8 {
        let shift = (7 - i) * 4;
        out[i] = HEX[((hash >> shift) & 0xf) as usize];
        i += 1;
    }
    out[8] = b'.';
    out[9] = b'L';
    out[10] = b'S';
    out[11] = b'T';
    out
}

/// Lowercase, strip one trailing dot, and keep a DNS label sequence.
pub fn normalize_into(name: &str, out: &mut [u8]) -> Option<usize> {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() || name.len() > out.len() || name.len() > 253 {
        return None;
    }
    if name.bytes().all(|byte| byte.is_ascii_digit() || byte == b'.') {
        return None;
    }
    let mut labels = 0;
    for label in name.split('.') {
        labels += 1;
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return None;
        }
        if bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
            return None;
        }
        if !bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        {
            return None;
        }
    }
    if labels == 0 {
        return None;
    }
    for (slot, byte) in out.iter_mut().zip(name.bytes()) {
        *slot = byte.to_ascii_lowercase();
    }
    Some(name.len())
}

fn is_sink(token: &str) -> bool {
    matches!(
        token,
        "0.0.0.0" | "127.0.0.1" | "255.255.255.255" | "::" | "::0" | "::1" | "0"
    )
}

/// Hosts-file lines and plain domain lines. `#` comments are ignored.
pub fn parse_line(line: &str, mut emit: impl FnMut(&str)) {
    let line = line.split('#').next().unwrap_or("").trim();
    if line.is_empty() {
        return;
    }
    let mut parts = line.split_whitespace();
    let Some(first) = parts.next() else {
        return;
    };
    let mut buf = [0u8; 253];
    if is_sink(first) {
        for name in parts {
            if let Some(len) = normalize_into(name, &mut buf) {
                if let Ok(text) = core::str::from_utf8(&buf[..len]) {
                    emit(text);
                }
            }
        }
        return;
    }
    if parts.next().is_some() {
        return;
    }
    if let Some(len) = normalize_into(first, &mut buf) {
        if let Ok(text) = core::str::from_utf8(&buf[..len]) {
            emit(text);
        }
    }
}

pub fn feed(text: &str, mut emit: impl FnMut(&str)) -> usize {
    let mut count = 0;
    for line in text.split('\n') {
        parse_line(line, |name| {
            emit(name);
            count += 1;
        });
    }
    count
}

/// `true` only for a stable published hit. Down, loading, and a generation
/// that moves during the probe all return false so DNS answers normally.
pub fn fail_open(gen_before: u64, gen_after: u64, hit: bool) -> bool {
    gen_before != 0 && gen_before % 2 == 0 && gen_before == gen_after && hit
}

fn slots(region: &[u8]) -> Option<&[u64]> {
    if region.len() < HEADER + 8 || region.as_ptr() as usize % 8 != 0 {
        return None;
    }
    let rest = &region[HEADER..];
    let n = rest.len() / 8;
    Some(unsafe { core::slice::from_raw_parts(rest.as_ptr() as *const u64, n) })
}

fn slots_mut(region: &mut [u8]) -> Option<&mut [u64]> {
    if region.len() < HEADER + 8 || region.as_ptr() as usize % 8 != 0 {
        return None;
    }
    let n = (region.len() - HEADER) / 8;
    Some(unsafe { core::slice::from_raw_parts_mut(region.as_mut_ptr().add(HEADER) as *mut u64, n) })
}

fn read_gen(region: &[u8]) -> u64 {
    if region.len() < HEADER {
        return 0;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&region[8..16]);
    u64::from_le_bytes(buf)
}

fn write_u64(region: &mut [u8], offset: usize, value: u64) {
    region[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

pub fn generation(region: &[u8]) -> u64 {
    let magic = if region.len() >= 8 {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&region[..8]);
        u64::from_le_bytes(buf)
    } else {
        0
    };
    if magic != MAGIC {
        return 0;
    }
    read_gen(region)
}

pub fn count(region: &[u8]) -> u64 {
    if generation(region) == 0 || region.len() < 24 {
        return 0;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&region[16..24]);
    u64::from_le_bytes(buf)
}

/// Start a rebuild. The generation goes odd, so lookups fail open until
/// [`commit`].
pub fn begin(region: &mut [u8]) -> bool {
    if region.len() < HEADER + 8 || region.as_ptr() as usize % 8 != 0 {
        return false;
    }
    let next = generation(region).wrapping_add(1) | 1;
    write_u64(region, 0, MAGIC);
    write_u64(region, 8, next);
    write_u64(region, 16, 0);
    let (ptr, len) = {
        let Some(table) = slots_mut(region) else {
            return false;
        };
        (table.as_mut_ptr(), table.len())
    };
    let mut i = 0;
    while i < len {
        unsafe { *ptr.add(i) = 0; }
        i += 1;
    }
    write_u64(region, 24, len as u64);
    true
}

pub fn insert(region: &mut [u8], hash: u64) -> bool {
    let hash = if hash == 0 { 1 } else { hash };
    let gen = read_gen(region);
    if gen == 0 || gen % 2 == 0 {
        return false;
    }
    let count = {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&region[16..24]);
        u64::from_le_bytes(buf) as usize
    };
    let (ptr, len) = {
        let Some(table) = slots_mut(region) else {
            return false;
        };
        (table.as_mut_ptr(), table.len())
    };
    if count >= len / 2 {
        return false;
    }
    let mut index = (hash as usize) % len;
    let mut step = 0;
    while step < PROBE_MAX && step < len {
        let slot = unsafe { *ptr.add(index) };
        if slot == 0 {
            unsafe { *ptr.add(index) = hash; }
            write_u64(region, 16, (count + 1) as u64);
            return true;
        }
        if slot == hash {
            return true;
        }
        index = (index + 1) % len;
        step += 1;
    }
    false
}

pub fn commit(region: &mut [u8]) {
    let gen = read_gen(region);
    if gen == 0 {
        return;
    }
    let published = gen + (gen % 2);
    write_u64(region, 8, published);
}

pub fn lookup(region: &[u8], name: &str) -> bool {
    let before = generation(region);
    if before == 0 || before % 2 != 0 {
        return false;
    }
    let Some(table) = slots(region) else {
        return false;
    };
    let hash = name_hash(name);
    let mut index = (hash as usize) % table.len();
    let mut step = 0;
    let mut hit = false;
    while step < PROBE_MAX && step < table.len() {
        let slot = table[index];
        if slot == 0 {
            break;
        }
        if slot == hash {
            hit = true;
            break;
        }
        index = (index + 1) % table.len();
        step += 1;
    }
    fail_open(before, generation(region), hit)
}

pub fn used_bytes(region: &[u8]) -> usize {
    count(region) as usize * 8
}

pub fn region_size(region: &[u8]) -> usize {
    region.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aligned(bytes: usize) -> (Vec<u8>, usize) {
        let mut buf = std::vec![0u8; bytes + 16];
        let addr = buf.as_mut_ptr() as usize;
        let aligned = (addr + 7) & !7;
        let offset = aligned - addr;
        (buf, offset)
    }

    #[test]
    fn parses_hosts_and_plain_domains() {
        let text = "\
# comment
0.0.0.0 Ads.Example.com extra.test
127.0.0.1 localhost
::1 bad.example.
plain.example.com
not a domain line
1.2.3.4
";
        let mut names = std::vec::Vec::new();
        let n = feed(text, |name| names.push(name.to_string()));
        assert_eq!(n, 5);
        assert_eq!(
            names,
            [
                "ads.example.com",
                "extra.test",
                "localhost",
                "bad.example",
                "plain.example.com"
            ]
        );
    }

    #[test]
    fn url_file_names_are_8_3_and_stable() {
        let a = list_file("https://example.com/hosts");
        let b = list_file("https://example.com/hosts");
        let c = list_file("http://example.com/hosts");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(&a[8..], b".LST");
        assert!(a[..8].iter().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(a.len(), 12);
        assert!(URL_MAX <= 8);
        assert!(LIST_MAX_BYTES <= 1024 * 1024);
        assert!(LIST_MAX_ENTRIES <= 400_000);
    }

    #[test]
    fn lookup_fails_open_until_a_stable_publish() {
        let (mut buf, offset) = aligned(4096 + HEADER);
        let region = &mut buf[offset..offset + 4096 + HEADER];
        assert!(!lookup(region, "ads.example"));
        assert!(begin(region));
        assert!(insert(region, name_hash("ads.example")));
        assert!(!lookup(region, "ads.example"), "odd generation is loading");
        assert!(!fail_open(0, 0, true), "unpublished");
        assert!(!fail_open(1, 1, true), "writing");
        assert!(!fail_open(2, 4, true), "generation moved");
        commit(region);
        assert!(lookup(region, "ads.example"));
        assert!(!lookup(region, "other.example"));
        assert_eq!(count(region), 1);
    }

    #[test]
    fn insert_stops_at_half_load() {
        let (mut buf, offset) = aligned(HEADER + 64);
        let region = &mut buf[offset..offset + HEADER + 64];
        assert!(begin(region));
        let mut added = 0;
        let mut n = 0;
        while n < 20 {
            let name = format!("n{n}.example");
            if insert(region, name_hash(&name)) {
                added += 1;
            }
            n += 1;
        }
        assert_eq!(added, 4);
        assert_eq!(count(region), 4);
    }
}
