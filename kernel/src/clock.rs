//! Wall clock for the rest of the kernel.
//!
//! [`synced`] is false until SNTP accepts a reply. Callers that check a
//! certificate, or that write a timestamp they will compare later, refuse
//! the work until then. Between syncs the generic timer advances the same
//! instant. Nothing here waits or takes the network lock.

use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};

use ntp::Civil;

pub const STATUS_LEN: usize = 100;

static SYNCED: AtomicBool = AtomicBool::new(false);
static UNIX_MS: AtomicI64 = AtomicI64::new(0);
static TIMER_AT: AtomicU64 = AtomicU64::new(0);
static OFFSET_MS: AtomicI64 = AtomicI64::new(0);
static LAST_UNIX: AtomicI64 = AtomicI64::new(0);
static STRATUM: AtomicU8 = AtomicU8::new(0);
static SERVER_LEN: AtomicU8 = AtomicU8::new(0);
static SERVER: spin::Mutex<[u8; 48]> = spin::Mutex::new([0; 48]);

pub fn synced() -> bool {
    SYNCED.load(Ordering::Acquire)
}

/// Unix seconds when time is valid. `None` until the first accepted sync.
pub fn unix_secs() -> Option<i64> {
    unix_ms().map(|ms| ms.div_euclid(1000))
}

pub fn unix_ms() -> Option<i64> {
    if !synced() {
        return None;
    }
    let base = UNIX_MS.load(Ordering::Acquire);
    let at = TIMER_AT.load(Ordering::Acquire);
    let now = crate::timer::millis();
    Some(base.saturating_add(now.saturating_sub(at) as i64))
}

pub fn stratum() -> u8 {
    STRATUM.load(Ordering::Relaxed)
}

pub fn offset_ms() -> i64 {
    OFFSET_MS.load(Ordering::Relaxed)
}

/// Install one accepted sample. `unix_ms` is the time at `timer_ms`.
pub fn sync(unix_ms: i64, offset_ms: i64, stratum: u8, server: &[u8], timer_ms: u64) {
    let mut name = SERVER.lock();
    let n = server.len().min(name.len());
    name.fill(0);
    name[..n].copy_from_slice(&server[..n]);
    drop(name);
    SERVER_LEN.store(n as u8, Ordering::Relaxed);
    UNIX_MS.store(unix_ms, Ordering::Release);
    TIMER_AT.store(timer_ms, Ordering::Release);
    OFFSET_MS.store(offset_ms, Ordering::Relaxed);
    LAST_UNIX.store(unix_ms.div_euclid(1000), Ordering::Relaxed);
    STRATUM.store(stratum, Ordering::Relaxed);
    SYNCED.store(true, Ordering::Release);
}

/// Calendar fields for a directory entry. Unsynced writes keep the
/// historical fallback, 2026-11-05 00:00:00 UTC.
pub fn fat_parts() -> (u16, u8, u8, u8, u8, u8) {
    if let Some(secs) = unix_secs() {
        if let Some(civil) = ntp::civil_from_unix(secs) {
            if (1980..=2107).contains(&civil.year) {
                return parts(civil);
            }
        }
    }
    (2026, 11, 5, 0, 0, 0)
}

fn parts(civil: Civil) -> (u16, u8, u8, u8, u8, u8) {
    (
        civil.year,
        civil.month,
        civil.day,
        civil.hour,
        civil.minute,
        civil.second,
    )
}

/// `synced`, `stratum`, `offset_ms`, 20-byte UTC, 20-byte last sync, server.
pub fn status(out: &mut [u8]) -> usize {
    if out.len() < STATUS_LEN {
        return 0;
    }
    out[..STATUS_LEN].fill(0);
    out[0] = u8::from(synced());
    out[1] = stratum();
    out[4..12].copy_from_slice(&offset_ms().to_le_bytes());
    if let Some(secs) = unix_secs() {
        let _ = ntp::format_utc(secs, &mut out[12..32]);
    }
    let last = LAST_UNIX.load(Ordering::Relaxed);
    if synced() && last > 0 {
        let _ = ntp::format_utc(last, &mut out[32..52]);
    }
    let name = SERVER.lock();
    let n = SERVER_LEN.load(Ordering::Relaxed) as usize;
    let n = n.min(48);
    out[52..52 + n].copy_from_slice(&name[..n]);
    STATUS_LEN
}

pub fn prefix(out: &mut [u8]) -> usize {
    let Some(secs) = unix_secs() else {
        return 0;
    };
    ntp::format_prefix(secs, crate::timer::uptime_secs(), out)
}
