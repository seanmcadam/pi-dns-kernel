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
pub const LIST_MAX_BYTES: usize = 4 * 1024 * 1024;
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

/// Bytes kept when a line is split across reads. A hosts name is at most
/// 253 bytes, and `0.0.0.0 ` in front of it still fits. A longer line is
/// skipped through its newline so the next line is not lost with it.
pub const LINE_HOLD: usize = 512;

pub struct LineSplit {
    hold: [u8; LINE_HOLD],
    n: usize,
    skip: bool,
}

impl LineSplit {
    pub const fn new() -> Self {
        Self {
            hold: [0; LINE_HOLD],
            n: 0,
            skip: false,
        }
    }

    /// Emit every complete line in `chunk`. A trailing fragment stays
    /// buffered until [`Self::finish`] or the next chunk.
    pub fn push(&mut self, chunk: &[u8], mut emit: impl FnMut(&[u8])) {
        let mut i = 0;
        while i < chunk.len() {
            if self.skip {
                match chunk[i..].iter().position(|byte| *byte == b'\n') {
                    Some(rel) => {
                        self.skip = false;
                        i += rel + 1;
                    }
                    None => return,
                }
                continue;
            }
            match chunk[i..].iter().position(|byte| *byte == b'\n') {
                Some(rel) => {
                    let end = i + rel;
                    self.emit_line(&chunk[i..end], &mut emit);
                    i = end + 1;
                }
                None => {
                    self.keep(&chunk[i..]);
                    return;
                }
            }
        }
    }

    pub fn finish(&mut self, mut emit: impl FnMut(&[u8])) {
        if !self.skip && self.n > 0 {
            let n = self.n;
            self.n = 0;
            emit(&self.hold[..n]);
        }
        self.n = 0;
        self.skip = false;
    }

    fn emit_line(&mut self, tail: &[u8], emit: &mut impl FnMut(&[u8])) {
        if self.n == 0 {
            emit(tail);
            return;
        }
        if self.n + tail.len() <= self.hold.len() {
            let n = self.n;
            self.hold[n..n + tail.len()].copy_from_slice(tail);
            self.n = 0;
            emit(&self.hold[..n + tail.len()]);
        } else {
            self.n = 0;
        }
    }

    fn keep(&mut self, tail: &[u8]) {
        if self.n + tail.len() <= self.hold.len() {
            self.hold[self.n..self.n + tail.len()].copy_from_slice(tail);
            self.n += tail.len();
        } else {
            self.n = 0;
            self.skip = true;
        }
    }
}

/// How a download read ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadEnd {
    /// TCP EOF or a TLS close_notify.
    Clean,
    /// Timeout or a broken stream. The bytes so far are not a whole body.
    Fail,
}

/// `true` when `got` is a complete HTTP body.
///
/// A short read against Content-Length is incomplete even on a clean close.
/// A failure is incomplete even when the server never sent a length. Callers
/// that already have every Content-Length byte can stop before this.
pub fn body_complete(content_length: Option<usize>, got: usize, end: ReadEnd) -> bool {
    match end {
        ReadEnd::Fail => false,
        ReadEnd::Clean => match content_length {
            Some(n) => got >= n,
            None => true,
        },
    }
}

/// `Content-Length` from a header block, if the field is present and numeric.
pub fn content_length(headers: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < headers.len() {
        let end = headers[i..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|rel| i + rel)
            .unwrap_or(headers.len());
        let mut line = &headers[i..end];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let key = b"content-length:";
        if line.len() >= key.len() && line[..key.len()].eq_ignore_ascii_case(key) {
            let text = core::str::from_utf8(line[key.len()..].trim_ascii()).ok()?;
            return text.parse().ok();
        }
        if end == headers.len() {
            break;
        }
        i = end + 1;
    }
    None
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

/// Mark a rebuild in progress without zeroing the slots.
///
/// The generation goes odd, so lookups fail open until [`commit`]. The
/// caller zeros the slots with [`scrub`], in pieces, so a 32 MiB table
/// does not keep the CPU inside one syscall.
pub fn open_rebuild(region: &mut [u8]) -> bool {
    if region.len() < HEADER + 8 || region.as_ptr() as usize % 8 != 0 {
        return false;
    }
    let next = generation(region).wrapping_add(1) | 1;
    write_u64(region, 0, MAGIC);
    write_u64(region, 8, next);
    write_u64(region, 16, 0);
    let Some(len) = slot_len(region) else {
        return false;
    };
    write_u64(region, 24, len as u64);
    true
}

fn slot_len(region: &mut [u8]) -> Option<usize> {
    Some(slots_mut(region)?.len())
}

/// Zero `count` slots starting at `from`. Returns `(next, len)`.
/// `next == len` means the table is clear.
pub fn scrub(region: &mut [u8], from: usize, count: usize) -> Option<(usize, usize)> {
    let table = slots_mut(region)?;
    let len = table.len();
    let start = from.min(len);
    let end = start.saturating_add(count).min(len);
    let ptr = table.as_mut_ptr();
    let mut i = start;
    while i < end {
        unsafe {
            *ptr.add(i) = 0;
        }
        i += 1;
    }
    Some((end, len))
}

/// Start a rebuild and zero every slot. Lookups fail open until [`commit`].
pub fn begin(region: &mut [u8]) -> bool {
    if !open_rebuild(region) {
        return false;
    }
    scrub(region, 0, usize::MAX).is_some()
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

fn read_tlv(input: &[u8]) -> Option<(u8, usize, usize)> {
    if input.len() < 2 {
        return None;
    }
    let tag = input[0];
    let (len, header) = if input[1] & 0x80 == 0 {
        (input[1] as usize, 2usize)
    } else {
        let nbytes = (input[1] & 0x7f) as usize;
        if nbytes == 0 || nbytes > 3 || input.len() < 2 + nbytes {
            return None;
        }
        let mut len = 0usize;
        let mut i = 0;
        while i < nbytes {
            len = (len << 8) | input[2 + i] as usize;
            i += 1;
        }
        (len, 2 + nbytes)
    };
    if input.len() < header + len {
        return None;
    }
    Some((tag, header, len))
}

fn tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (tag, header, len) = read_tlv(input)?;
    Some((tag, &input[header..header + len], &input[header + len..]))
}

/// Issuer (`subject` false) or subject Name, including its tag and length.
pub fn cert_name(cert: &[u8], subject: bool) -> Option<&[u8]> {
    let (tag, value, _) = tlv(cert)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = tlv(value)?;
    if tag != 0x30 {
        return None;
    }
    let mut rest = tbs;
    let (tag, _, next) = tlv(rest)?;
    if tag == 0xA0 {
        rest = next;
    }
    let (tag, _, next) = tlv(rest)?;
    if tag != 0x02 {
        return None;
    }
    rest = next;
    let (tag, _, next) = tlv(rest)?;
    if tag != 0x30 {
        return None;
    }
    rest = next;
    let (tag, _, next) = tlv(rest)?;
    if tag != 0x30 {
        return None;
    }
    let issuer = &rest[..rest.len() - next.len()];
    if !subject {
        return Some(issuer);
    }
    rest = next;
    let (tag, _, next) = tlv(rest)?;
    if tag != 0x30 {
        return None;
    }
    rest = next;
    let (tag, _, next) = tlv(rest)?;
    if tag != 0x30 {
        return None;
    }
    Some(&rest[..rest.len() - next.len()])
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
    fn hosts_split_across_chunk_boundaries_match_one_feed() {
        let mut body = String::new();
        let mut i = 0;
        while i < 20_000 {
            if i % 17 == 0 {
                body.push_str("# a comment that is not a name\r\n");
            } else if i % 19 == 0 {
                body.push('\n');
            } else if i % 23 == 0 {
                body.push_str("0.0.0.0 one.example two.example\n");
            } else if i % 29 == 0 {
                body.push_str("plain.example\n");
            } else {
                body.push_str(&format!("127.0.0.1 host{i}.example\n"));
            }
            i += 1;
        }
        body.push_str(&"x".repeat(600));
        body.push_str("\n0.0.0.0 after-long.example\n");
        body.push_str("0.0.0.0 tail.example");
        let reference = feed(&body, |_| {});
        assert!(reference > 18_000, "reference {reference}");
        let sizes = [
            1usize, 2, 3, 7, 17, 63, 64, 127, 128, 255, 256, 299, 300, 301, 511, 512, 513, 1024,
            1400, 16384,
        ];
        let bytes = body.as_bytes();
        for size in sizes {
            let mut split = LineSplit::new();
            let mut got = 0usize;
            let mut off = 0;
            while off < bytes.len() {
                let end = (off + size).min(bytes.len());
                split.push(&bytes[off..end], |line| {
                    let Ok(text) = core::str::from_utf8(line) else {
                        return;
                    };
                    parse_line(text, |_| got += 1);
                });
                off = end;
            }
            split.finish(|line| {
                let Ok(text) = core::str::from_utf8(line) else {
                    return;
                };
                parse_line(text, |_| got += 1);
            });
            assert_eq!(got, reference, "chunk {size}");
        }
    }

    #[test]
    fn a_short_body_is_not_a_complete_download() {
        assert_eq!(
            content_length(b"HTTP/1.1 200 OK\r\nContent-Length: 2505650\r\n\r\n"),
            Some(2_505_650)
        );
        assert_eq!(
            content_length(b"HTTP/1.0 200 OK\r\ncontent-length: 12\r\n"),
            Some(12)
        );
        assert_eq!(content_length(b"HTTP/1.0 200 OK\r\n\r\n"), None);
        assert!(!body_complete(Some(100), 80, ReadEnd::Clean));
        assert!(body_complete(Some(100), 100, ReadEnd::Clean));
        assert!(!body_complete(Some(100), 50, ReadEnd::Fail));
        assert!(body_complete(None, 10, ReadEnd::Clean));
        assert!(!body_complete(None, 10, ReadEnd::Fail));
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
        assert!(LIST_MAX_BYTES >= 4 * 1024 * 1024);
        assert!(LIST_MAX_BYTES <= 8 * 1024 * 1024);
        assert!(LIST_MAX_ENTRIES <= 400_000);
    }

    #[test]
    fn isrg_root_yr_is_a_self_signed_trust_anchor() {
        let yr = include_bytes!("../../block-el0/roots/isrgyr.der");
        let subject = cert_name(yr, true).expect("subject");
        let issuer = cert_name(yr, false).expect("issuer");
        assert_eq!(subject, issuer);
        assert!(subject.windows(7).any(|window| window == b"Root YR"));
    }

    #[test]
    fn scrub_in_slices_matches_a_full_begin() {
        let (mut buf, offset) = aligned(HEADER + 64);
        let region = &mut buf[offset..offset + HEADER + 64];
        region.fill(0x5a);
        assert!(open_rebuild(region));
        assert_eq!(generation(region) % 2, 1);
        let mut at = 0;
        let mut steps = 0;
        loop {
            let (next, len) = scrub(region, at, 3).unwrap();
            steps += 1;
            if next >= len {
                break;
            }
            at = next;
            assert!(steps < 8);
        }
        let slots =
            unsafe { core::slice::from_raw_parts(region[HEADER..].as_ptr() as *const u64, 8) };
        assert!(slots.iter().all(|slot| *slot == 0));
        assert!(insert(region, name_hash("ads.example")));
        assert!(!lookup(region, "ads.example"));
        commit(region);
        assert!(lookup(region, "ads.example"));
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

    #[test]
    fn root_subject_matches_its_issuer() {
        for der in [
            include_bytes!("../../block-el0/roots/isrgx1.der").as_slice(),
            include_bytes!("../../block-el0/roots/isrgx2.der").as_slice(),
            include_bytes!("../../block-el0/roots/digicertg2.der").as_slice(),
        ] {
            let subject = cert_name(der, true).unwrap();
            let issuer = cert_name(der, false).unwrap();
            assert_eq!(subject, issuer);
            assert!(subject.len() > 8);
        }
    }
}
