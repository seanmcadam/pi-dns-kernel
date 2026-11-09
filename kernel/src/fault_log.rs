//! Fault line layout and the decision of where a fault record goes.
//!
//! The buffer is the caller's. Nothing here allocates, takes a lock, or
//! prints. The logger applies `action` after it has already tried the
//! queue without waiting.

use core::sync::atomic::{AtomicU8, Ordering};

include!("../../user-common/panic_line.rs");

static DEPTH: AtomicU8 = AtomicU8::new(0);

/// First caller owns the fault-log section. A nested caller must not touch
/// the queue or format another line.
pub fn enter() -> bool {
    DEPTH.fetch_add(1, Ordering::Relaxed) == 0
}

pub fn leave() {
    DEPTH.fetch_sub(1, Ordering::Relaxed);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultAction {
    /// Log task is up, the queue lock was free, and a slot was free.
    Queue,
    /// UART now. The queue was down, busy, or full.
    Direct,
    /// Already inside a fault report. UART a fixed nested line only.
    Nested,
}

pub fn action(first: bool, log_live: bool, queue_free: bool, queue_full: bool) -> FaultAction {
    if !first {
        return FaultAction::Nested;
    }
    if log_live && queue_free && !queue_full {
        FaultAction::Queue
    } else {
        FaultAction::Direct
    }
}

pub fn task_name(slot: usize) -> &'static str {
    match slot {
        0 => "dns",
        1 => "http",
        2 => "spin",
        3 => "log",
        4 => "telnet",
        5 => "vnc",
        6 => "hdmi",
        _ => "task",
    }
}

/// `kfault v=<vector> ec=<ec> elr=<16 hex> far=<16 hex> <access> <status>`
pub fn write_kfault(out: &mut [u8], kind: u64, ec: u64, iss: u64, elr: u64, far: u64) -> usize {
    let mut n = 0;
    n = push(out, n, b"kfault v=");
    n = push_u32(out, n, kind as u32);
    n = push_ec(out, n, ec);
    n = push_where(out, n, elr, far);
    n = push_detail(out, n, ec, iss);
    n
}

/// `elfault <name> t=<thread> ec=<ec> elr= far= <access> <status>`
pub fn write_elfault(
    out: &mut [u8],
    name: &str,
    thread: u32,
    ec: u64,
    iss: u64,
    elr: u64,
    far: u64,
) -> usize {
    let mut n = 0;
    n = push(out, n, b"elfault ");
    n = push(out, n, name.as_bytes());
    n = push(out, n, b" t=");
    n = push_u32(out, n, thread);
    n = push_ec(out, n, ec);
    n = push_where(out, n, elr, far);
    n = push_detail(out, n, ec, iss);
    n
}

fn push_ec(out: &mut [u8], n: usize, ec: u64) -> usize {
    let n = push(out, n, b" ec=");
    push_hex2(out, n, (ec & 0x3f) as u8)
}

fn push_where(out: &mut [u8], n: usize, elr: u64, far: u64) -> usize {
    let n = push(out, n, b" elr=");
    let n = push_hex16(out, n, elr);
    let n = push(out, n, b" far=");
    push_hex16(out, n, far)
}

fn push_detail(out: &mut [u8], n: usize, ec: u64, iss: u64) -> usize {
    let (access, status) = detail(ec, iss);
    if access.is_empty() && status.is_empty() {
        return n;
    }
    let n = push(out, n, b" ");
    let n = push(out, n, access.as_bytes());
    if status.is_empty() {
        return n;
    }
    let n = push(out, n, b" ");
    push(out, n, status.as_bytes())
}

fn detail(ec: u64, iss: u64) -> (&'static str, &'static str) {
    match ec {
        0x20 | 0x21 => ("fetch", status(iss & 0x3f)),
        0x24 | 0x25 => {
            let access = if iss & (1 << 6) != 0 { "write" } else { "read" };
            (access, status(iss & 0x3f))
        }
        0x22 => ("pc", "align"),
        0x26 => ("sp", "align"),
        _ => ("", "other"),
    }
}

fn status(dfsc: u64) -> &'static str {
    match dfsc {
        0b000100 | 0b000101 | 0b000110 | 0b000111 => "trans",
        0b001001 | 0b001010 | 0b001011 => "flag",
        0b001101 | 0b001110 | 0b001111 => "perm",
        0b000000 | 0b000001 | 0b000010 | 0b000011 => "size",
        0b100001 => "align",
        0b010000 | 0b010100 => "ext",
        _ => "other",
    }
}

fn push_hex2(out: &mut [u8], n: usize, byte: u8) -> usize {
    let digits = [hex((byte >> 4) & 0xf), hex(byte & 0xf)];
    push(out, n, &digits)
}

fn push_hex16(out: &mut [u8], mut n: usize, mut value: u64) -> usize {
    let mut buf = [0u8; 16];
    let mut i = 16;
    while i > 0 {
        i -= 1;
        buf[i] = hex((value & 0xf) as u8);
        value >>= 4;
    }
    n = push(out, n, &buf);
    n
}

fn hex(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_matrix() {
        assert_eq!(action(false, true, true, false), FaultAction::Nested);
        assert_eq!(action(true, false, true, false), FaultAction::Direct);
        assert_eq!(action(true, true, false, false), FaultAction::Direct);
        assert_eq!(action(true, true, true, true), FaultAction::Direct);
        assert_eq!(action(true, true, true, false), FaultAction::Queue);
    }

    #[test]
    fn depth_is_one_owner() {
        DEPTH.store(0, Ordering::SeqCst);
        let first = super::enter();
        let second = super::enter();
        super::leave();
        super::leave();
        assert!(first, "first call did not own the section");
        assert!(!second, "second call was not nested");
    }

    #[test]
    fn kfault_fits_and_names_the_access() {
        let mut buf = [0u8; 80];
        let n = write_kfault(&mut buf, 4, 0x25, (1 << 6) | 0b001101, 0xffff_0000_0008_1234, 0);
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(n <= 80, "{line}");
        assert!(line.starts_with("kfault v=4 ec=25 "));
        assert!(line.contains("elr=ffff000000081234"));
        assert!(line.contains("far=0000000000000000"));
        assert!(line.ends_with("write perm"));
    }

    #[test]
    fn elfault_worst_case_fits_the_log_slot() {
        let mut buf = [0u8; 96];
        let n = write_elfault(
            &mut buf,
            "telnet",
            32,
            0x24,
            0b000100,
            0x1_0000_0000,
            0,
        );
        assert!(n <= 80, "{}", n);
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            line,
            "elfault telnet t=32 ec=24 elr=0000000100000000 far=0000000000000000 read trans"
        );
        assert_eq!(task_name(0), "dns");
        assert_eq!(task_name(4), "telnet");
        assert_eq!(task_name(6), "hdmi");
        assert_eq!(task_name(9), "task");
    }

    #[test]
    fn panic_line_redacts_credentials_and_keeps_the_location() {
        let mut buf = [0u8; 80];
        let msg = b"password=hunter2";
        let n = format_panic_line(&mut buf, b"http", b"/src/server.rs", 151, msg);
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(line, "panic http server.rs:151 redacted");
        assert!(!line.contains("hunter2"));
        assert!(!secret(b"bypass the cache"));
        let n = format_panic_line(&mut buf, b"dns", b"main.rs", 12, b"oom");
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(line, "panic dns main.rs:12 oom");
    }
}
