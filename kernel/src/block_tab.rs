//! Domain table in the dedicated block region, plus the blocker's syscalls.
//!
//! A lookup tries the lock once. If the blocker is rebuilding, not running,
//! or the generation is unpublished, the answer is "not blocked" and DNS
//! keeps going.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use blocklist::{self, URL_MAX};
use spin::Mutex;

use crate::block_mem;

const ALLOW_LEN: usize = 253;
const URL_KEEP: usize = 160;
const ERR_KEEP: usize = 48;

static LOCK: Mutex<()> = Mutex::new(());
static BLOCKED: AtomicU64 = AtomicU64::new(0);
static STATE: AtomicU8 = AtomicU8::new(0);
static ALLOW_N: AtomicU8 = AtomicU8::new(0);
static ALLOW: Mutex<[[u8; ALLOW_LEN]; 32]> = Mutex::new([[0; ALLOW_LEN]; 32]);
static ALLOW_L: Mutex<[u8; 32]> = Mutex::new([0; 32]);

struct UrlRow {
    name: [u8; 12],
    url: [u8; URL_KEEP],
    url_len: u8,
    err: [u8; ERR_KEEP],
    err_len: u8,
    entries: u32,
    mtime: i64,
    next: i64,
}

impl UrlRow {
    const fn empty() -> Self {
        Self {
            name: [0; 12],
            url: [0; URL_KEEP],
            url_len: 0,
            err: [0; ERR_KEEP],
            err_len: 0,
            entries: 0,
            mtime: 0,
            next: 0,
        }
    }
}

static ROWS: Mutex<[UrlRow; URL_MAX]> = Mutex::new([const { UrlRow::empty() }; URL_MAX]);

pub fn set_allow(names: &[alloc::string::String]) {
    let mut store = ALLOW.lock();
    let mut lens = ALLOW_L.lock();
    let mut n = 0u8;
    for name in names {
        if n as usize >= 32 {
            break;
        }
        let bytes = name.as_bytes();
        let len = bytes.len().min(ALLOW_LEN);
        let mut i = 0;
        while i < len {
            store[n as usize][i] = bytes[i].to_ascii_lowercase();
            i += 1;
        }
        lens[n as usize] = len as u8;
        n += 1;
    }
    ALLOW_N.store(n, Ordering::Release);
}

fn region() -> Option<&'static mut [u8]> {
    let start = block_mem::start();
    let size = block_mem::size();
    if start.is_null() || size < 64 || (start as usize) % 8 != 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts_mut(start, size) })
}

fn running() -> bool {
    crate::supervisor::runtime(crate::task::BLOCK_SLOT) == 1
}

/// `0` not blocked, `1` blocked, `2` allow-listed. Anything else fails open.
pub fn lookup(name: &str) -> u8 {
    if !running() {
        return 0;
    }
    if allowed(name) {
        return 2;
    }
    let Some(_guard) = LOCK.try_lock() else {
        return 0;
    };
    let Some(mem) = region() else {
        return 0;
    };
    if blocklist::lookup(mem, name) {
        BLOCKED.fetch_add(1, Ordering::Relaxed);
        1
    } else {
        0
    }
}

fn allowed(name: &str) -> bool {
    let n = ALLOW_N.load(Ordering::Acquire) as usize;
    if n == 0 {
        return false;
    }
    let store = ALLOW.lock();
    let lens = ALLOW_L.lock();
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < n {
        let len = lens[i] as usize;
        if len > 0 && suffix(bytes, &store[i][..len]) {
            return true;
        }
        i += 1;
    }
    false
}

fn suffix(name: &[u8], allow: &[u8]) -> bool {
    if name.len() == allow.len() {
        return name.eq_ignore_ascii_case(allow);
    }
    if name.len() > allow.len() + 1
        && name[name.len() - allow.len() - 1] == b'.'
        && name[name.len() - allow.len()..].eq_ignore_ascii_case(allow)
    {
        return true;
    }
    false
}

pub fn call(op: u64, args: [u64; 5]) -> i64 {
    match op {
        0 => begin(),
        1 => insert(args[0]),
        2 => commit(),
        3 => lookup_user(args[0], args[1]),
        4 => write_stats(args[0], args[1]),
        5 => write_config(args[0], args[1]),
        6 => note_url(args[0], args[1]),
        7 => read_file(args[0], args[1], args[2], args[3], args[4]),
        8 => write_file(args[0], args[1], args[2], args[3], args[4]),
        9 => swing(args[0], args[1], args[2], args[3]),
        10 => mtime(args[0], args[1]),
        11 => unix_now(),
        12 => read_ca(args[0], args[1]),
        13 => {
            let state = args[0] as u8;
            if state <= 2 {
                STATE.store(state, Ordering::Relaxed);
            }
            0
        }
        14 => clear_rows(args[0]),
        _ => -22,
    }
}

/// Drop a published list set. Lookups fail open until the process loads the
/// lists that the new config still names.
pub fn drop_lists() {
    let _ = clear_rows(0);
    let mut try_n = 0;
    while try_n < 8 {
        if begin() == 0 {
            let _ = commit();
            return;
        }
        try_n += 1;
    }
}

fn clear_rows(from: u64) -> i64 {
    let start = from as usize;
    if start > URL_MAX {
        return -22;
    }
    let mut rows = ROWS.lock();
    let mut i = start;
    while i < URL_MAX {
        rows[i] = UrlRow::empty();
        i += 1;
    }
    0
}

fn begin() -> i64 {
    let Some(_guard) = LOCK.try_lock() else {
        return -11;
    };
    let Some(mem) = region() else {
        return -5;
    };
    if blocklist::begin(mem) { 0 } else { -5 }
}

fn insert(hash: u64) -> i64 {
    let Some(_guard) = LOCK.try_lock() else {
        return -11;
    };
    let Some(mem) = region() else {
        return -5;
    };
    if blocklist::insert(mem, hash) { 1 } else { 0 }
}

fn commit() -> i64 {
    let Some(_guard) = LOCK.try_lock() else {
        return -11;
    };
    let Some(mem) = region() else {
        return -5;
    };
    blocklist::commit(mem);
    0
}

fn lookup_user(ptr: u64, len: u64) -> i64 {
    let mut buf = [0u8; 253];
    let n = copy_in(ptr, len, &mut buf);
    if n == 0 {
        return 0;
    }
    let Ok(name) = core::str::from_utf8(&buf[..n]) else {
        return 0;
    };
    let mut norm = [0u8; 253];
    let Some(len) = blocklist::normalize_into(name, &mut norm) else {
        return 0;
    };
    let Ok(name) = core::str::from_utf8(&norm[..len]) else {
        return 0;
    };
    i64::from(lookup(name))
}

fn write_stats(ptr: u64, cap: u64) -> i64 {
    let mut buf = [0u8; 2048];
    let n = fill_stats(&mut buf);
    copy_out(ptr, cap, &buf[..n])
}

fn fill_stats(out: &mut [u8]) -> usize {
    if out.len() < 48 {
        return 0;
    }
    let state = if running() { STATE.load(Ordering::Relaxed) } else { 0 };
    out[0] = state;
    let (domains, size) = match LOCK.try_lock() {
        None => (0u64, block_mem::size() as u64),
        Some(_guard) => {
            if let Some(mem) = region() {
                (blocklist::count(mem), mem.len() as u64)
            } else {
                (0, block_mem::size() as u64)
            }
        }
    };
    out[8..16].copy_from_slice(&domains.to_le_bytes());
    out[16..24].copy_from_slice(&BLOCKED.load(Ordering::Relaxed).to_le_bytes());
    let used = domains.saturating_mul(8);
    out[24..32].copy_from_slice(&used.to_le_bytes());
    out[32..40].copy_from_slice(&size.to_le_bytes());
    let rows = ROWS.lock();
    let mut n = 48;
    let mut count = 0u8;
    for row in rows.iter() {
        if row.url_len == 0 {
            continue;
        }
        if n + 36 > out.len() {
            break;
        }
        out[n..n + 12].copy_from_slice(&row.name);
        out[n + 12] = row.url_len;
        out[n + 13] = row.err_len;
        out[n + 16..n + 20].copy_from_slice(&row.entries.to_le_bytes());
        out[n + 20..n + 28].copy_from_slice(&row.mtime.to_le_bytes());
        out[n + 28..n + 36].copy_from_slice(&row.next.to_le_bytes());
        n += 36;
        let ul = row.url_len as usize;
        if n + ul + row.err_len as usize > out.len() {
            break;
        }
        out[n..n + ul].copy_from_slice(&row.url[..ul]);
        n += ul;
        let el = row.err_len as usize;
        out[n..n + el].copy_from_slice(&row.err[..el]);
        n += el;
        count += 1;
    }
    out[40] = count;
    n
}

fn write_config(ptr: u64, cap: u64) -> i64 {
    let text = match crate::config_manager::block_text() {
        Ok(text) => text,
        Err(_) => return -5,
    };
    copy_out(ptr, cap, text.as_bytes())
}

fn note_url(ptr: u64, len: u64) -> i64 {
    let mut buf = [0u8; 256];
    let n = copy_in(ptr, len, &mut buf);
    if n < 36 {
        return -22;
    }
    let index = buf[0] as usize;
    if index >= URL_MAX {
        return -22;
    }
    let mut row = UrlRow::empty();
    row.name.copy_from_slice(&buf[1..13]);
    row.url_len = buf[13].min(URL_KEEP as u8);
    row.err_len = buf[14].min(ERR_KEEP as u8);
    let mut ent = [0u8; 4];
    ent.copy_from_slice(&buf[16..20]);
    row.entries = u32::from_le_bytes(ent);
    let mut wide = [0u8; 8];
    wide.copy_from_slice(&buf[20..28]);
    row.mtime = i64::from_le_bytes(wide);
    wide.copy_from_slice(&buf[28..36]);
    row.next = i64::from_le_bytes(wide);
    let mut at = 36;
    let ul = row.url_len as usize;
    let el = row.err_len as usize;
    if n < at + ul + el {
        return -22;
    }
    row.url[..ul].copy_from_slice(&buf[at..at + ul]);
    at += ul;
    row.err[..el].copy_from_slice(&buf[at..at + el]);
    ROWS.lock()[index] = row;
    0
}

fn file_name(ptr: u64, len: u64) -> Option<[u8; 16]> {
    let mut raw = [0u8; 16];
    let n = copy_in(ptr, len, &mut raw);
    if n == 0 || n > 12 {
        return None;
    }
    let Ok(name) = core::str::from_utf8(&raw[..n]) else {
        return None;
    };
    if !safe_name(name) {
        return None;
    }
    Some(raw)
}

fn safe_name(name: &str) -> bool {
    if name == "BLOCK.IDX" || name == "BLOCK.TMP" || name == "TESTCA.DER" {
        return true;
    }
    let bytes = name.as_bytes();
    bytes.len() == 12
        && bytes[8] == b'.'
        && &bytes[9..] == b"LST"
        && bytes[..8]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(byte))
}

fn read_file(name_ptr: u64, name_len: u64, offset: u64, buf: u64, cap: u64) -> i64 {
    let Some(name) = file_name(name_ptr, name_len) else {
        return -22;
    };
    let Ok(name) = core::str::from_utf8(&name) else {
        return -22;
    };
    let name = name.trim_end_matches('\0');
    let mut tmp = [0u8; 1024];
    let cap = (cap as usize).min(tmp.len());
    if cap == 0 || !crate::mmu::user_write_ok(buf, cap as u64) {
        return -14;
    }
    match crate::fs::read_at(name, offset as u32, &mut tmp[..cap]) {
        Ok(n) => copy_out(buf, cap as u64, &tmp[..n]),
        Err(_) => -2,
    }
}

fn write_file(name_ptr: u64, name_len: u64, buf: u64, len: u64, flags: u64) -> i64 {
    let Some(name) = file_name(name_ptr, name_len) else {
        return -22;
    };
    let Ok(name) = core::str::from_utf8(&name) else {
        return -22;
    };
    let name = name.trim_end_matches('\0');
    if name != "BLOCK.TMP" && name != "BLOCK.IDX" {
        return -22;
    }
    if len > 1024 || !crate::mmu::user_slice_ok(buf, len) {
        return -14;
    }
    let mut tmp = [0u8; 1024];
    let n = copy_in(buf, len, &mut tmp);
    let result = if flags & 1 != 0 {
        crate::fs::write_new(name, &tmp[..n])
    } else {
        crate::fs::append_bytes(name, &tmp[..n])
    };
    if result.is_ok() { n as i64 } else { -5 }
}

fn swing(from_ptr: u64, from_len: u64, to_ptr: u64, to_len: u64) -> i64 {
    let Some(from) = file_name(from_ptr, from_len) else {
        return -22;
    };
    let Some(to) = file_name(to_ptr, to_len) else {
        return -22;
    };
    let Ok(from) = core::str::from_utf8(&from) else {
        return -22;
    };
    let Ok(to) = core::str::from_utf8(&to) else {
        return -22;
    };
    let from = from.trim_end_matches('\0');
    let to = to.trim_end_matches('\0');
    if from != "BLOCK.TMP" {
        return -22;
    }
    if crate::fs::swing_file(from, to).is_ok() { 0 } else { -5 }
}

fn mtime(ptr: u64, len: u64) -> i64 {
    let Some(name) = file_name(ptr, len) else {
        return -22;
    };
    let Ok(name) = core::str::from_utf8(&name) else {
        return -22;
    };
    let name = name.trim_end_matches('\0');
    match crate::fs::file_time(name) {
        Ok(stamp) => fat_unix(stamp),
        Err(_) => 0,
    }
}

fn fat_unix(stamp: embedded_sdmmc::Timestamp) -> i64 {
    let year = 1970 + stamp.year_since_1970 as i64;
    let month = stamp.zero_indexed_month as i64 + 1;
    let day = stamp.zero_indexed_day as i64 + 1;
    let mut y = year;
    let mut m = month;
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    days * 86400 + stamp.hours as i64 * 3600 + stamp.minutes as i64 * 60 + stamp.seconds as i64
}

fn unix_now() -> i64 {
    crate::clock::unix_secs().unwrap_or(0)
}

fn read_ca(ptr: u64, cap: u64) -> i64 {
    let mut tmp = [0u8; 2048];
    let n = match crate::fs::read_at(blocklist::CA_NAME, 0, &mut tmp) {
        Ok(n) => n,
        Err(_) => return 0,
    };
    copy_out(ptr, cap, &tmp[..n])
}

fn copy_in(ptr: u64, len: u64, out: &mut [u8]) -> usize {
    let n = (len as usize).min(out.len());
    if n == 0 || !crate::mmu::user_slice_ok(ptr, n as u64) {
        return 0;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, out.as_mut_ptr(), n);
    }
    crate::mmu::set_pan(true);
    n
}

fn copy_out(ptr: u64, cap: u64, data: &[u8]) -> i64 {
    let n = data.len().min(cap as usize);
    if n == 0 {
        return 0;
    }
    if !crate::mmu::user_write_ok(ptr, n as u64) {
        return -14;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), ptr as *mut u8, n);
    }
    crate::mmu::set_pan(true);
    n as i64
}

