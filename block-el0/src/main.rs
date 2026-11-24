#![no_std]
#![no_main]

extern crate alloc;

core::arch::global_asm!(include_str!("start.s"));

mod fetch;

use alloc::vec::Vec;
use core::alloc::{GlobalAlloc, Layout};
use core::panic::PanicInfo;
use core::ptr;

const LOG: u64 = 0;
const YIELD: u64 = 1;
const EXIT: u64 = 3;
const TIME_MS: u64 = 4;
const UDP_BIND: u64 = 5;
const NET_READY: u64 = 63;
const BLOCK: u64 = 80;

const INDEX: &str = "BLOCK.IDX";
const TEMP: &str = "BLOCK.TMP";

const HEAP_BASE: usize = 0x1_000A_0000;
const HEAP_SIZE: usize = 1536 * 1024;
const HEADER: usize = 16;

struct Node {
    size: usize,
    next: *mut Node,
}

static mut FREE: *mut Node = ptr::null_mut();

struct Heap;

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        alloc_block(layout)
    }

    unsafe fn dealloc(&self, ptr_in: *mut u8, layout: Layout) {
        free_block(ptr_in, alloc_size(layout));
    }
}

fn alloc_size(layout: Layout) -> usize {
    align(layout.size().max(1), 8).max(HEADER)
}

fn align(value: usize, to: usize) -> usize {
    (value + to - 1) & !(to - 1)
}

unsafe fn alloc_block(layout: Layout) -> *mut u8 {
    if FREE.is_null() {
        let node = HEAP_BASE as *mut Node;
        (*node).size = HEAP_SIZE;
        (*node).next = ptr::null_mut();
        FREE = node;
    }
    let align_to = layout.align().max(HEADER);
    let size = alloc_size(layout);
    let mut prev: *mut Node = ptr::null_mut();
    let mut node = FREE;
    while !node.is_null() {
        let start = node as usize;
        let region = (*node).size;
        let aligned = align(start + HEADER, align_to);
        let padding = aligned.saturating_sub(start);
        if padding >= HEADER && padding + size <= region {
            let next = (*node).next;
            if prev.is_null() {
                FREE = next;
            } else {
                (*prev).next = next;
            }
            if padding >= HEADER {
                let pad = start as *mut Node;
                (*pad).size = padding;
                insert_free(pad);
            }
            let remain_at = aligned + size;
            let remain = region - (remain_at - start);
            if remain >= HEADER {
                let rest = remain_at as *mut Node;
                (*rest).size = remain;
                insert_free(rest);
            }
            return aligned as *mut u8;
        }
        prev = node;
        node = (*node).next;
    }
    ptr::null_mut()
}

unsafe fn free_block(user: *mut u8, size: usize) {
    let node = user as *mut Node;
    (*node).size = size;
    insert_free(node);
}

unsafe fn insert_free(node: *mut Node) {
    let addr = node as usize;
    let mut prev: *mut Node = ptr::null_mut();
    let mut cur = FREE;
    while !cur.is_null() && (cur as usize) < addr {
        prev = cur;
        cur = (*cur).next;
    }
    if !cur.is_null() && addr + (*node).size == cur as usize {
        (*node).size += (*cur).size;
        (*node).next = (*cur).next;
    } else {
        (*node).next = cur;
    }
    if !prev.is_null() && (prev as usize) + (*prev).size == node as usize {
        (*prev).size += (*node).size;
        (*prev).next = (*node).next;
    } else if prev.is_null() {
        FREE = node;
    } else {
        (*prev).next = node;
    }
}

#[global_allocator]
static HEAP: Heap = Heap;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    log("block panic\n");
    sys(EXIT, 1, 0, 0, 0, 0, 0);
    loop {}
}

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let rc: i64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => rc,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
            options(nostack),
        );
    }
    rc
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

fn unix_now() -> i64 {
    block(11, 0, 0, 0, 0, 0)
}

struct Cfg {
    enabled: bool,
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
        refresh: 86400,
        urls: Vec::new(),
        allow: Vec::new(),
    };
    for line in text.split('\n') {
        if let Some(rest) = line.strip_prefix("enabled ") {
            cfg.enabled = rest.trim() == "1";
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

fn load_file(name: &str) -> u32 {
    let mut off = 0u64;
    let mut chunk = [0u8; 1024];
    let mut hold = [0u8; 300];
    let mut hold_n = 0usize;
    let mut count = 0u32;
    loop {
        let n = file_read(name, off, &mut chunk);
        if n < 0 {
            break;
        }
        if n == 0 || count >= blocklist::LIST_MAX_ENTRIES as u32 {
            break;
        }
        off += n as u64;
        let bytes = &chunk[..n as usize];
        let mut start = 0;
        for (i, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            if hold_n > 0 {
                let room = hold.len() - hold_n;
                let take = i.min(room);
                hold[hold_n..hold_n + take].copy_from_slice(&bytes[..take]);
                count += feed_line(&hold[..hold_n + take]);
                hold_n = 0;
            } else {
                count += feed_line(&bytes[start..i]);
            }
            start = i + 1;
        }
        let tail = &bytes[start..];
        if hold_n + tail.len() <= hold.len() {
            hold[hold_n..hold_n + tail.len()].copy_from_slice(tail);
            hold_n += tail.len();
        } else {
            hold_n = 0;
        }
        if off > blocklist::LIST_MAX_BYTES as u64 {
            break;
        }
    }
    if hold_n > 0 {
        count += feed_line(&hold[..hold_n]);
    }
    count
}

fn list_name(url: &str) -> alloc::string::String {
    let raw = blocklist::list_file(url);
    alloc::string::String::from(core::str::from_utf8(&raw).unwrap_or("BAD.LST"))
}

fn feed_line(line: &[u8]) -> u32 {
    let Ok(text) = core::str::from_utf8(line) else {
        return 0;
    };
    let mut added = 0u32;
    blocklist::parse_line(text, |name| {
        if block(1, blocklist::name_hash(name), 0, 0, 0, 0) == 1 {
            added += 1;
        }
    });
    added
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

fn publish(cfg: &Cfg, previous: [u32; 8]) -> [u32; 8] {
    let latest = load_cfg();
    let cfg = if latest.enabled != cfg.enabled || latest.urls != cfg.urls {
        &latest
    } else {
        cfg
    };
    if block(0, 0, 0, 0, 0, 0) < 0 {
        return previous;
    }
    let mut counts = [0u32; 8];
    if cfg.enabled && !cfg.urls.is_empty() {
        let cached = index_lines();
        for (i, url) in cfg.urls.iter().enumerate().take(8) {
            let name = list_name(url);
            if cached.iter().any(|(file, _)| file == &name) {
                counts[i] = load_file(&name);
            }
        }
    }
    let _ = block(2, 0, 0, 0, 0, 0);
    let tail = if cfg.enabled { cfg.urls.len() as u64 } else { 0 };
    let _ = block(14, tail, 0, 0, 0, 0);
    counts
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
    let mut hold = [0u8; 300];
    let mut hold_n = 0usize;
    let mut count = 0u32;
    loop {
        let n = file_read(TEMP, off, &mut chunk);
        if n < 0 {
            return Err("disk");
        }
        if n == 0 {
            break;
        }
        off += n as u64;
        let bytes = &chunk[..n as usize];
        let mut start = 0usize;
        for (i, byte) in bytes.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }
            let line = if hold_n > 0 {
                let room = hold.len() - hold_n;
                let take = i.min(room);
                hold[hold_n..hold_n + take].copy_from_slice(&bytes[..take]);
                let n = hold_n + take;
                hold_n = 0;
                &hold[..n]
            } else {
                &bytes[start..i]
            };
            count = count.saturating_add(names_in_line(line));
            if count > blocklist::LIST_MAX_ENTRIES as u32 {
                return Ok(count);
            }
            start = i + 1;
        }
        let tail = &bytes[start..];
        if hold_n + tail.len() <= hold.len() {
            hold[hold_n..hold_n + tail.len()].copy_from_slice(tail);
            hold_n += tail.len();
        } else {
            hold_n = 0;
        }
    }
    if hold_n > 0 {
        count = count.saturating_add(names_in_line(&hold[..hold_n]));
    }
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
        let n = file_read(TEMP, got, &mut buf);
        if n <= 0 {
            return false;
        }
        let take = (n as u64).min(len - got) as usize;
        check = fnv(&buf[..take], check);
        got += take as u64;
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
    if names_in_temp()? > blocklist::LIST_MAX_ENTRIES as u32 {
        return Err("full");
    }
    let name = list_name(url);
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
    Ok(())
}

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    log("block start\n");
    fetch::set_dns(sys(UDP_BIND, 5353, 0, 0, 0, 0, 0));
    let _ = block(13, 1, 0, 0, 0, 0);
    let mut cfg = load_cfg();
    let mut counts = publish(&cfg, [0; 8]);
    let _ = block(13, 2, 0, 0, 0, 0);
    let mut errors = [""; 8];
    let mut next = [0i64; 8];
    let mut hold = [0u64; 8];
    loop {
        let now = time_ms();
        let unix = unix_now();
        let loaded = load_cfg();
        if loaded.enabled != cfg.enabled
            || loaded.refresh != cfg.refresh
            || loaded.urls != cfg.urls
            || loaded.allow != cfg.allow
        {
            cfg = loaded;
            let _ = block(13, 1, 0, 0, 0, 0);
            counts = publish(&cfg, counts);
            let _ = block(13, 2, 0, 0, 0, 0);
            errors = [""; 8];
            next = [0; 8];
        }
        if cfg.enabled && sys(NET_READY, 0, 0, 0, 0, 0, 0) == 1 {
            let mut servers = [[0u8; 4]; 16];
            let servers_n = resolvers(&mut servers);
            let mut changed = false;
            for (i, url) in cfg.urls.iter().enumerate().take(8) {
                let name = list_name(url);
                let mtime = file_mtime(&name);
                let due = if url.starts_with("https://") {
                    unix > 0 && (mtime <= 0 || unix >= mtime.saturating_add(cfg.refresh))
                } else {
                    mtime <= 0 || (unix > 0 && unix >= mtime.saturating_add(cfg.refresh))
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
                let _ = block(13, 1, 0, 0, 0, 0);
                counts = publish(&cfg, counts);
                let _ = block(13, 2, 0, 0, 0, 0);
            }
        }
        if cfg.enabled {
            let cached = index_lines();
            for (i, url) in cfg.urls.iter().enumerate().take(8) {
                let name = list_name(url);
                let mtime = if cached.iter().any(|(file, _)| file == &name) {
                    file_mtime(&name)
                } else {
                    0
                };
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
