#![no_std]
#![no_main]

extern crate alloc;

core::arch::global_asm!(include_str!("../../user-common/start.s"));

mod fetch;

use alloc::vec::Vec;
use core::panic::PanicInfo;

mod syscall {
    #![allow(dead_code)]
    include!("../../user-common/syscall.rs");
}

mod heap {
    include!("../../user-common/alloc.rs");
}

use syscall::{sys, BLOCK, EXIT, LOG, NET_READY, TIME_MS, UDP_BIND, YIELD};

const INDEX: &str = "BLOCK.IDX";
const TEMP: &str = "BLOCK.TMP";

pub(crate) const HEAP_VA: usize = 0x1_000A_0000;
pub(crate) const ALLOC_SIZE: usize = 1536 * 1024;

#[global_allocator]
static HEAP: heap::Heap = heap::Heap;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    log("block panic\n");
    sys(EXIT, 1, 0, 0, 0, 0, 0);
    loop {}
}

fn log(text: &str) {
    let _ = sys(LOG, text.as_ptr() as u64, text.len() as u64, 0, 0, 0, 0);
}

fn time_ms() -> u64 {
    sys(TIME_MS, 0, 0, 0, 0, 0, 0) as u64
}

fn sleep_ms(ms: u64) {
    let end = time_ms().saturating_add(ms);
    while time_ms() < end {
        let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
    }
}

fn block(op: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    sys(BLOCK, op, a1, a2, a3, a4, a5)
}

fn file_read(name: &str, offset: u64, buf: &mut [u8]) -> i64 {
    block(
        7,
        name.as_ptr() as u64,
        name.len() as u64,
        offset,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
    )
}

fn file_write(name: &str, data: &[u8], truncate: bool) -> bool {
    block(
        8,
        name.as_ptr() as u64,
        name.len() as u64,
        data.as_ptr() as u64,
        data.len() as u64,
        if truncate { 1 } else { 0 },
    ) >= 0
}

fn file_mtime(name: &str) -> i64 {
    block(10, name.as_ptr() as u64, name.len() as u64, 0, 0, 0)
}

fn file_len(name: &str) -> i64 {
    block(15, name.as_ptr() as u64, name.len() as u64, 0, 0, 0)
}

fn unix_now() -> i64 {
    block(11, 0, 0, 0, 0, 0)
}

struct Cfg {
    enabled: bool,
    epoch: u64,
    refresh: i64,
    urls: Vec<alloc::string::String>,
    allow: Vec<alloc::string::String>,
}

fn load_cfg() -> Cfg {
    let mut buf = [0u8; 2048];
    let n = block(5, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0);
    let text = if n > 0 {
        core::str::from_utf8(&buf[..n as usize]).unwrap_or("")
    } else {
        ""
    };
    let mut cfg = Cfg {
        enabled: false,
        epoch: 0,
        refresh: 86400,
        urls: Vec::new(),
        allow: Vec::new(),
    };
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("enabled ") {
            cfg.enabled = rest.trim() == "1";
        } else if let Some(rest) = line.strip_prefix("epoch ") {
            cfg.epoch = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("refresh ") {
            cfg.refresh = rest.trim().parse().unwrap_or(86400);
        } else if let Some(rest) = line.strip_prefix("url ") {
            let url = rest.trim();
            if !url.is_empty() {
                cfg.urls.push(alloc::string::String::from(url));
            }
        } else if let Some(rest) = line.strip_prefix("allow ") {
            let name = rest.trim();
            if !name.is_empty() {
                cfg.allow.push(alloc::string::String::from(name));
            }
        }
    }
    cfg
}

fn note(index: u8, name: &str, url: &str, err: &str, entries: u32, mtime: i64, next: i64) {
    let mut buf = [0u8; 256];
    if name.len() > 12 || url.len() > 160 || err.len() > 48 {
        return;
    }
    buf[0] = index;
    buf[1..1 + name.len()].copy_from_slice(name.as_bytes());
    buf[13] = url.len() as u8;
    buf[14] = err.len() as u8;
    buf[16..20].copy_from_slice(&entries.to_le_bytes());
    buf[20..28].copy_from_slice(&mtime.to_le_bytes());
    buf[28..36].copy_from_slice(&next.to_le_bytes());
    let mut n = 36;
    buf[n..n + url.len()].copy_from_slice(url.as_bytes());
    n += url.len();
    buf[n..n + err.len()].copy_from_slice(err.as_bytes());
    n += err.len();
    let _ = block(6, buf.as_ptr() as u64, n as u64, 0, 0, 0);
}

fn resolvers(out: &mut [[u8; 4]]) -> usize {
    let mut buf = [0u8; 320];
    let n = sys(14, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0, 0);
    if n <= 0 {
        return 0;
    }
    let end = (n as usize).min(buf.len());
    ntp::parse_resolvers(&buf[..end], out)
}

fn read_soft(name: &str, off: u64, buf: &mut [u8]) -> i64 {
    let mut tries = 0;
    loop {
        let n = file_read(name, off, buf);
        if n >= 0 || tries >= 4 {
            return n;
        }
        tries += 1;
        let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
    }
}

fn load_file(name: &str) -> Result<u32, &'static str> {
    let mut off = 0u64;
    let mut chunk = [0u8; 1024];
    let mut split = blocklist::LineSplit::new();
    let mut count = 0u32;
    loop {
        let n = read_soft(name, off, &mut chunk);
        if n < 0 {
            return Err("disk");
        }
        if n == 0 || count >= blocklist::LIST_MAX_ENTRIES as u32 {
            break;
        }
        off += n as u64;
        split.push(&chunk[..n as usize], |line| {
            count = count.saturating_add(feed_line(line));
        });
        if off > blocklist::LIST_MAX_BYTES as u64 {
            break;
        }
        // Back to the owner so a query is not stuck behind the rest of the file.
        let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
    }
    split.finish(|line| {
        count = count.saturating_add(feed_line(line));
    });
    Ok(count)
}

fn list_name(url: &str) -> alloc::string::String {
    let raw = blocklist::list_file(url);
    alloc::string::String::from(core::str::from_utf8(&raw).unwrap_or("BAD.LST"))
}

fn meta_name(url: &str) -> alloc::string::String {
    let raw = blocklist::meta_file(url);
    alloc::string::String::from(core::str::from_utf8(&raw).unwrap_or("BAD.NFO"))
}

/// The list file matches a version-1 sidecar. A short file from an older
/// build has no sidecar and is not loaded.
fn cache_complete(url: &str) -> bool {
    let list = list_name(url);
    let len = file_len(&list);
    if len <= 0 {
        return false;
    }
    let meta = meta_name(url);
    let mut buf = [0u8; 64];
    let n = file_read(&meta, 0, &mut buf);
    if n <= 0 {
        return false;
    }
    blocklist::cache_is_complete(
        blocklist::decode_cache_meta(&buf[..n as usize]),
        len as u32,
    )
}

fn write_meta(url: &str, version: u32, bytes: u32, entries: u32) {
    let name = meta_name(url);
    let mut buf = [0u8; 64];
    let Some(n) = blocklist::encode_cache_meta(
        &blocklist::CacheMeta {
            version,
            bytes,
            entries,
        },
        &mut buf,
    ) else {
        return;
    };
    let _ = file_write(&name, &buf[..n], true);
}

fn feed_line(line: &[u8]) -> u32 {
    let Ok(text) = core::str::from_utf8(line) else {
        return 0;
    };
    let mut added = 0u32;
    blocklist::parse_line(text, |name| {
        if insert_name(name) {
            added += 1;
        }
    });
    added
}

fn insert_name(name: &str) -> bool {
    let hash = blocklist::name_hash(name);
    let mut tries = 0;
    loop {
        match block(1, hash, 0, 0, 0, 0) {
            1 => return true,
            // The lookup path holds the table lock for one probe. Retry
            // instead of dropping the name.
            -11 if tries < 8 => {
                tries += 1;
                let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
            }
            _ => return false,
        }
    }
}

fn index_lines() -> Vec<(alloc::string::String, alloc::string::String)> {
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    let mut off = 0u64;
    let mut hold = alloc::string::String::new();
    loop {
        let n = file_read(INDEX, off, &mut buf);
        if n <= 0 {
            break;
        }
        off += n as u64;
        hold.push_str(core::str::from_utf8(&buf[..n as usize]).unwrap_or(""));
    }
    for line in hold.split('\n') {
        let mut parts = line.splitn(2, ' ');
        let name = parts.next().unwrap_or("").trim();
        let url = parts.next().unwrap_or("").trim();
        if name.len() == 12 && !url.is_empty() {
            out.push((
                alloc::string::String::from(name),
                alloc::string::String::from(url),
            ));
        }
    }
    out
}

fn publish(cfg: &Cfg) -> Option<[u32; 8]> {
    let latest = load_cfg();
    let cfg = if latest.epoch != cfg.epoch || latest.enabled != cfg.enabled || latest.urls != cfg.urls
    {
        &latest
    } else {
        cfg
    };
    // Each call zeros a slice and returns. Yielding between slices lets the
    // owner poll the NIC, so DNS keeps answering while the table is rebuilt.
    let mut misses = 0;
    loop {
        match block(0, 0, 0, 0, 0, 0) {
            0 => break,
            1 => {
                let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
            }
            _ => {
                misses += 1;
                if misses >= 8 {
                    // The inactive half is unfinished. Leave the published
                    // half in place and let the caller try again.
                    return None;
                }
                let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
            }
        }
    }
    let mut counts = [0u32; 8];
    if cfg.enabled && !cfg.urls.is_empty() {
        let cached = index_lines();
        for (i, url) in cfg.urls.iter().enumerate().take(8) {
            let name = list_name(url);
            // The index only records that a download was attempted. A file
            // without a matching sidecar is a truncated or pre-0.1.90 cache.
            if cached.iter().any(|(file, _)| file == &name) && cache_complete(url) {
                match load_file(&name) {
                    Ok(n) => counts[i] = n,
                    // A short read must not be reported as a full list. The
                    // generation is still committed below so lookups are not
                    // left fail-open until the next refresh.
                    Err(_) => counts[i] = 0,
                }
            }
        }
    }
    if block(2, 0, 0, 0, 0, 0) < 0 {
        return None;
    }
    let tail = if cfg.enabled { cfg.urls.len() as u64 } else { 0 };
    let _ = block(14, tail, 0, 0, 0, 0);
    Some(counts)
}

fn write_index(cfg: &Cfg) {
    let mut body = alloc::string::String::new();
    for url in &cfg.urls {
        body.push_str(&list_name(url));
        body.push(' ');
        body.push_str(url);
        body.push('\n');
    }
    let bytes = body.as_bytes();
    if bytes.is_empty() {
        let _ = file_write(INDEX, b"\n", true);
        return;
    }
    let mut off = 0;
    while off < bytes.len() {
        let end = (off + 1024).min(bytes.len());
        if !file_write(INDEX, &bytes[off..end], off == 0) {
            return;
        }
        off = end;
    }
}

fn fnv(data: &[u8], hash: u64) -> u64 {
    let mut h = hash;
    for byte in data {
        h ^= *byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn names_in_temp() -> Result<u32, &'static str> {
    let mut off = 0u64;
    let mut chunk = [0u8; 1024];
    let mut split = blocklist::LineSplit::new();
    let mut count = 0u32;
    let mut failed = false;
    loop {
        let n = read_soft(TEMP, off, &mut chunk);
        if n < 0 {
            return Err("disk");
        }
        if n == 0 {
            break;
        }
        off += n as u64;
        split.push(&chunk[..n as usize], |line| {
            count = count.saturating_add(names_in_line(line));
            if count > blocklist::LIST_MAX_ENTRIES as u32 {
                failed = true;
            }
        });
        if failed {
            return Ok(count);
        }
        let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
    }
    split.finish(|line| {
        count = count.saturating_add(names_in_line(line));
    });
    Ok(count)
}

fn names_in_line(line: &[u8]) -> u32 {
    let Ok(text) = core::str::from_utf8(line) else {
        return 0;
    };
    let mut n = 0u32;
    blocklist::parse_line(text, |_| n = n.saturating_add(1));
    n
}

fn read_back(expect: u64, len: u64) -> bool {
    let mut check = 0xcbf29ce484222325u64;
    let mut got = 0u64;
    let mut buf = [0u8; 1024];
    while got < len {
        let n = read_soft(TEMP, got, &mut buf);
        if n <= 0 {
            return false;
        }
        let take = (n as u64).min(len - got) as usize;
        check = fnv(&buf[..take], check);
        got += take as u64;
        let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
    }
    check == expect
}

fn refresh_one(url: &str, dns: &[[u8; 4]], unix: i64) -> Result<(), &'static str> {
    let mut total = 0usize;
    let mut hash = 0xcbf29ce484222325u64;
    let mut started = false;
    fetch::download(url, unix, dns, |chunk| {
        if chunk.is_empty() {
            return Ok(());
        }
        if total.saturating_add(chunk.len()) > blocklist::LIST_MAX_BYTES {
            return Err("size");
        }
        if !file_write(TEMP, chunk, !started) {
            return Err("disk");
        }
        started = true;
        hash = fnv(chunk, hash);
        total += chunk.len();
        Ok(())
    })?;
    if total == 0 || !read_back(hash, total as u64) {
        return Err(if total == 0 { "empty" } else { "disk" });
    }
    let names = names_in_temp()?;
    if names > blocklist::LIST_MAX_ENTRIES as u32 {
        return Err("full");
    }
    let name = list_name(url);
    // Drop any previous marker before the file moves, so a crash cannot
    // pair the new bytes with an old length.
    write_meta(url, 0, 0, 0);
    if block(
        9,
        TEMP.as_ptr() as u64,
        TEMP.len() as u64,
        name.as_ptr() as u64,
        name.len() as u64,
        0,
    ) < 0
    {
        return Err("disk");
    }
    write_meta(url, blocklist::CACHE_META_VERSION, total as u32, names);
    Ok(())
}

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    log("block start\n");
    fetch::set_dns(sys(UDP_BIND, 5353, 0, 0, 0, 0, 0));
    let _ = block(13, 1, 0, 0, 0, 0);
    let mut cfg = load_cfg();
    let mut applied: Option<u64> = None;
    let mut counts = [0u32; 8];
    if let Some(next) = publish(&cfg) {
        counts = next;
        applied = Some(cfg.epoch);
        let _ = block(13, 2, 0, 0, 0, 0);
    }
    let mut errors = [""; 8];
    let mut next = [0i64; 8];
    let mut hold = [0u64; 8];
    // Directory time and the sidecar are read once. The steady loop then
    // sleeps, so a DNS query is not queued behind a FAT stat.
    let mut seen = [false; 8];
    let mut fresh = [false; 8];
    let mut stamp = [0i64; 8];
    // A download that could not commit the table has to be published on a
    // later pass. The config epoch does not change for that case.
    let mut pending = false;
    loop {
        let now = time_ms();
        let unix = unix_now();
        let loaded = load_cfg();
        if loaded.refresh != cfg.refresh || loaded.allow != cfg.allow {
            // The kernel already installed the allow list. Rebuilding the
            // domain table here would unpublish names that did not change.
            cfg.refresh = loaded.refresh;
            cfg.allow = loaded.allow.clone();
        }
        if blocklist::needs_table_reload(
            applied,
            loaded.epoch,
            loaded.enabled != cfg.enabled,
            loaded.urls != cfg.urls,
        ) {
            let _ = block(13, 1, 0, 0, 0, 0);
            if let Some(published) = publish(&loaded) {
                if loaded.urls != cfg.urls || loaded.enabled != cfg.enabled {
                    write_index(&loaded);
                }
                cfg = loaded;
                applied = Some(cfg.epoch);
                counts = published;
                let _ = block(13, 2, 0, 0, 0, 0);
                errors = [""; 8];
                next = [0; 8];
                seen = [false; 8];
                fresh = [false; 8];
                stamp = [0; 8];
                // The 60s hold belongs to the previous URL. A new list should
                // download on this pass, not after that timer.
                hold = [0; 8];
                pending = false;
            }
        }
        if cfg.enabled && sys(NET_READY, 0, 0, 0, 0, 0, 0) == 1 {
            let mut servers = [[0u8; 4]; 16];
            let servers_n = resolvers(&mut servers);
            let mut changed = false;
            for (i, url) in cfg.urls.iter().enumerate().take(8) {
                let name = list_name(url);
                if !seen[i] {
                    stamp[i] = file_mtime(&name);
                    fresh[i] = cache_complete(url);
                    seen[i] = true;
                    if stamp[i] > 0 && !fresh[i] && errors[i].is_empty() {
                        errors[i] = "stale";
                    }
                }
                let aged =
                    stamp[i] <= 0 || (unix > 0 && unix >= stamp[i].saturating_add(cfg.refresh));
                // A truncated cache is due even when its directory time is
                // still inside the refresh window. HTTPS waits for a clock
                // so the certificate can be checked.
                let due = if url.starts_with("https://") {
                    unix > 0 && (!fresh[i] || aged)
                } else {
                    !fresh[i] || aged
                };
                if !due || (next[i] != 0 && (unix <= 0 || unix < next[i])) || now < hold[i] {
                    continue;
                }
                match refresh_one(url, &servers[..servers_n], unix) {
                    Ok(()) => {
                        errors[i] = "";
                        changed = true;
                        next[i] = 0;
                        hold[i] = now.saturating_add(60_000);
                        stamp[i] = file_mtime(&name);
                        fresh[i] = true;
                        seen[i] = true;
                        log("block fetched\n");
                    }
                    Err(err) => {
                        errors[i] = err;
                        if unix > 0 {
                            next[i] = unix.saturating_add(60);
                        }
                        hold[i] = now.saturating_add(60_000);
                        log("block fetch failed\n");
                    }
                }
            }
            if changed {
                cfg = load_cfg();
                write_index(&cfg);
                pending = true;
            }
        }
        if pending {
            let _ = block(13, 1, 0, 0, 0, 0);
            if let Some(next_counts) = publish(&cfg) {
                counts = next_counts;
                applied = Some(cfg.epoch);
                pending = false;
                let _ = block(13, 2, 0, 0, 0, 0);
            }
        }
        if cfg.enabled {
            for (i, url) in cfg.urls.iter().enumerate().take(8) {
                let name = list_name(url);
                let mtime = stamp[i];
                let due = if mtime > 0 {
                    mtime.saturating_add(cfg.refresh)
                } else if next[i] != 0 {
                    next[i]
                } else {
                    0
                };
                note(i as u8, &name, url, errors[i], counts[i], mtime, due);
            }
            let _ = block(14, cfg.urls.len() as u64, 0, 0, 0, 0);
        } else {
            let _ = block(14, 0, 0, 0, 0, 0);
        }
        let _ = now;
        sleep_ms(1000);
    }
}
