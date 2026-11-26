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
        7 => "block",
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

/// `STACK OVERFLOW <name> t=<thread> ec=<ec> elr= far=`
///
/// Same fields as an elfault line, without the access words, so the longest
/// task name still fits the 80-byte log slot.
pub fn write_stack_overflow(
    out: &mut [u8],
    name: &str,
    thread: u32,
    ec: u64,
    elr: u64,
    far: u64,
) -> usize {
    let mut n = 0;
    n = push(out, n, b"STACK OVERFLOW ");
    n = push(out, n, name.as_bytes());
    n = push(out, n, b" t=");
    n = push_u32(out, n, thread);
    n = push_ec(out, n, ec);
    push_where(out, n, elr, far)
}

/// Three pinned rows. Newlines are the row breaks.
pub fn write_stack_overflow_rows(
    out: &mut [u8],
    name: &str,
    thread: u32,
    ec: u64,
    elr: u64,
    far: u64,
) -> usize {
    let mut n = 0;
    n = push(out, n, b"STACK OVERFLOW ");
    n = push(out, n, name.as_bytes());
    n = push(out, n, b"\nt=");
    n = push_u32(out, n, thread);
    n = push_ec(out, n, ec);
    n = push(out, n, b" elr=");
    n = push_hex16(out, n, elr);
    n = push(out, n, b"\nfar=");
    push_hex16(out, n, far)
}

/// EL0 window shared by every task. Stacks grow down: the guard page under
/// a stack is the overflow side, and the page at the initial SP is the
/// guard above. Those pages are left unmapped. The physical frames stay
/// reserved in the task image; nothing returns them to an allocator.
pub const PAGE: u64 = 4096;
pub const USER_HEAP_VA: u64 = 0x1_000A_0000;
pub const USER_HEAP_BYTES: u64 = 1536 * 1024;
pub const USER_STACK_VA: u64 = 0x1_0022_0000;
pub const USER_STACK_BYTES: u64 = 64 * 1024;
pub const WORKER_STACK_BYTES: u64 = 64 * 1024;
pub const DNS_WORKERS: usize = 5;
pub const HTTP_WORKERS: usize = 4;
pub const TELNET_HEAP_BYTES: u64 = 256 * 1024;
pub const TELNET_CLIENTS: usize = 16;
pub const TELNET_STACK_BYTES: u64 = 8 * 1024;
pub const SPIN_STACK_BYTES: u64 = 4096;

const _: () = assert!(heap_alloc_bytes(HTTP_WORKERS) == 0x13_b000);
const _: () = assert!(heap_alloc_bytes(DNS_WORKERS) == 0x12_a000);
const _: () = assert!(worker_sp(0) == main_guard_below());
const _: () = assert!(worker_sp(0) == 0x1_0021_f000);
const _: () = assert!(telnet_reaper_guard_below() == USER_HEAP_VA);

pub const fn main_stack_bytes(slot: usize) -> u64 {
    if slot == 2 {
        SPIN_STACK_BYTES
    } else {
        USER_STACK_BYTES
    }
}

/// Page immediately under every main stack.
pub const fn main_guard_below() -> u64 {
    USER_STACK_VA - PAGE
}

/// Page at the initial SP, immediately above the main stack.
pub const fn main_guard_above(slot: usize) -> u64 {
    USER_STACK_VA + main_stack_bytes(slot)
}

/// Initial SP of DNS/HTTP worker `index`. Worker 0 is the highest.
pub const fn worker_sp(index: usize) -> u64 {
    let heap_end = USER_HEAP_VA + USER_HEAP_BYTES;
    heap_end - PAGE - (index as u64) * (WORKER_STACK_BYTES + PAGE)
}

/// Unmapped page under a DNS/HTTP worker. Overflow stores land here.
pub const fn worker_guard_below(index: usize) -> u64 {
    worker_sp(index) - WORKER_STACK_BYTES - PAGE
}

/// Bytes of the DNS/HTTP heap the bump allocator may own.
/// One page under the main stack, then each worker stack and the page under it.
pub const fn heap_alloc_bytes(workers: usize) -> u64 {
    USER_HEAP_BYTES - PAGE - (workers as u64) * (WORKER_STACK_BYTES + PAGE)
}

pub const fn telnet_client_sp(index: usize) -> u64 {
    let heap_end = USER_HEAP_VA + TELNET_HEAP_BYTES;
    heap_end - (index as u64) * (TELNET_STACK_BYTES + PAGE)
}

pub const fn telnet_client_guard_below(index: usize) -> u64 {
    telnet_client_sp(index) - TELNET_STACK_BYTES - PAGE
}

pub const fn telnet_reaper_sp() -> u64 {
    USER_HEAP_VA + PAGE + TELNET_STACK_BYTES
}

pub const fn telnet_reaper_guard_below() -> u64 {
    USER_HEAP_VA
}

pub const fn telnet_reaper_guard_above() -> u64 {
    telnet_reaper_sp()
}

/// Guard page `index` for this task, low address of the page.
/// Slot 0 is DNS (and the boot self-test), 1 is HTTP, 2 is the spinner,
/// 4 is telnet. Other slots only have the main-stack guards.
pub fn user_guard_page(slot: usize, index: usize) -> Option<u64> {
    if index == 0 {
        return Some(main_guard_below());
    }
    if index == 1 {
        return Some(main_guard_above(slot));
    }
    let n = index - 2;
    match slot {
        0 => {
            if n < DNS_WORKERS {
                Some(worker_guard_below(n))
            } else {
                None
            }
        }
        1 => {
            if n < HTTP_WORKERS {
                Some(worker_guard_below(n))
            } else {
                None
            }
        }
        4 => telnet_guard(n),
        _ => None,
    }
}

fn telnet_guard(n: usize) -> Option<u64> {
    if n < TELNET_CLIENTS {
        return Some(telnet_client_guard_below(n));
    }
    match n - TELNET_CLIENTS {
        0 => Some(telnet_client_sp(0)),
        1 => Some(telnet_reaper_guard_below()),
        2 => Some(telnet_reaper_guard_above()),
        _ => None,
    }
}

pub fn is_user_stack_guard(slot: usize, far: u64) -> bool {
    let page = far & !(PAGE - 1);
    user_range_hits_guard(slot, page, page + 1)
}

/// True when `[start, end)` covers any byte of a guard page.
pub fn user_range_hits_guard(slot: usize, start: u64, end: u64) -> bool {
    let mut index = 0;
    while let Some(guard) = user_guard_page(slot, index) {
        let guard_end = guard + PAGE;
        if start < guard_end && end > guard {
            return true;
        }
        index += 1;
    }
    false
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
        0b000100..=0b000111 => "trans",
        0b001001..=0b001011 => "flag",
        0b001101..=0b001111 => "perm",
        0b000000..=0b000011 => "size",
        0b100001 => "align",
        0b010000 | 0b010100 => "ext",
        _ => "other",
    }
}

fn push_hex2(out: &mut [u8], n: usize, byte: u8) -> usize {
    minfmt::push_hex(out, n, byte as u64, 2)
}

fn push_hex16(out: &mut [u8], n: usize, value: u64) -> usize {
    minfmt::push_hex(out, n, value, 16)
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
        let n = write_kfault(
            &mut buf,
            4,
            0x25,
            (1 << 6) | 0b001101,
            0xffff_0000_0008_1234,
            0,
        );
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
        let n = write_elfault(&mut buf, "telnet", 32, 0x24, 0b000100, 0x1_0000_0000, 0);
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

    #[test]
    fn stack_overflow_line_names_the_task_and_fits() {
        let mut buf = [0u8; 80];
        let n = write_stack_overflow(&mut buf, "telnet", 32, 0x24, 0x1_0000_0000, 0x1_0021_f000);
        assert!(n <= 80, "{}", n);
        let line = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            line,
            "STACK OVERFLOW telnet t=32 ec=24 elr=0000000100000000 far=000000010021f000"
        );
        let n =
            write_stack_overflow_rows(&mut buf, "telnet", 32, 0x24, 0x1_0000_0000, 0x1_0021_f000);
        assert!(n <= 80, "{}", n);
        let rows = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            rows,
            "STACK OVERFLOW telnet\nt=32 ec=24 elr=0000000100000000\nfar=000000010021f000"
        );
        assert!(is_user_stack_guard(0, 0x1_0021_f000));
        assert!(is_user_stack_guard(0, main_guard_below() + 0xfff));
        assert!(!is_user_stack_guard(0, USER_STACK_VA));
        assert!(!is_user_stack_guard(0, USER_HEAP_VA));
        assert!(is_user_stack_guard(4, USER_HEAP_VA));
    }

    #[test]
    fn guards_sit_under_every_user_stack_and_outside_the_allocator() {
        assert_eq!(worker_sp(0), 0x1_0021_f000);
        assert_eq!(worker_sp(0), main_guard_below());
        assert_eq!(main_guard_above(0), 0x1_0023_0000);
        assert_eq!(main_guard_above(2), 0x1_0022_1000);
        assert_eq!(worker_guard_below(3), 0x1_001d_b000);
        assert_eq!(heap_alloc_bytes(HTTP_WORKERS), 0x13_b000);
        assert_eq!(worker_guard_below(4), 0x1_001c_a000);
        assert_eq!(heap_alloc_bytes(DNS_WORKERS), 0x12_a000);
        assert_eq!(telnet_client_sp(0), 0x1_000e_0000);
        assert_eq!(telnet_client_sp(15), 0x1_000b_3000);
        assert_eq!(telnet_client_guard_below(15), 0x1_000b_0000);
        assert_eq!(telnet_reaper_sp(), 0x1_000a_3000);
        assert_eq!(telnet_reaper_guard_below(), USER_HEAP_VA);

        check_workers(0, DNS_WORKERS);
        check_workers(1, HTTP_WORKERS);
        check_telnet();
        assert!(guard_pages_unique(0));
        assert!(guard_pages_unique(1));
        assert!(guard_pages_unique(2));
        assert!(guard_pages_unique(3));
        assert!(guard_pages_unique(4));
        assert!(guard_pages_unique(5));
        assert!(guard_pages_unique(6));
    }

    fn overlaps(a0: u64, a1: u64, b0: u64, b1: u64) -> bool {
        a0 < b1 && b0 < a1
    }

    fn check_workers(slot: usize, workers: usize) {
        let alloc0 = USER_HEAP_VA;
        let alloc1 = USER_HEAP_VA + heap_alloc_bytes(workers);
        let main0 = USER_STACK_VA;
        let main1 = USER_STACK_VA + USER_STACK_BYTES;
        assert!(!overlaps(alloc0, alloc1, main0, main1));
        assert_eq!(alloc1, worker_guard_below(workers - 1));
        let mut index = 0;
        while index < workers {
            let sp = worker_sp(index);
            let low = sp - WORKER_STACK_BYTES;
            let below = worker_guard_below(index);
            assert_eq!(below + PAGE, low, "guard is the page under the stack");
            assert_eq!(sp & 15, 0);
            assert!(!overlaps(alloc0, alloc1, low, sp));
            assert!(!overlaps(alloc0, alloc1, below, below + PAGE));
            assert!(!overlaps(low, sp, main0, main1));
            assert!(is_user_stack_guard(slot, below));
            assert!(is_user_stack_guard(slot, below + PAGE - 1));
            assert!(!is_user_stack_guard(slot, low));
            assert!(!is_user_stack_guard(slot, sp - 1));
            let mut other = 0;
            while other < workers {
                if other != index {
                    let osp = worker_sp(other);
                    assert!(!overlaps(low, sp, osp - WORKER_STACK_BYTES, osp));
                }
                other += 1;
            }
            index += 1;
        }
        // The page at worker 0's SP is the main stack's overflow guard.
        assert!(is_user_stack_guard(slot, worker_sp(0)));
        assert!(!is_user_stack_guard(slot, main0));
    }

    fn check_telnet() {
        let main0 = USER_STACK_VA;
        let main1 = USER_STACK_VA + USER_STACK_BYTES;
        let reaper0 = telnet_reaper_guard_below() + PAGE;
        let reaper1 = telnet_reaper_sp();
        assert_eq!(reaper1 - reaper0, TELNET_STACK_BYTES);
        assert!(is_user_stack_guard(4, telnet_reaper_guard_below()));
        assert!(is_user_stack_guard(4, telnet_reaper_guard_above()));
        assert!(!overlaps(reaper0, reaper1, main0, main1));
        let mut index = 0;
        while index < TELNET_CLIENTS {
            let sp = telnet_client_sp(index);
            let low = sp - TELNET_STACK_BYTES;
            let below = telnet_client_guard_below(index);
            assert_eq!(below + PAGE, low);
            assert!(is_user_stack_guard(4, below));
            assert!(!is_user_stack_guard(4, low));
            assert!(!overlaps(low, sp, reaper0, reaper1));
            assert!(!overlaps(low, sp, main0, main1));
            assert!(!overlaps(
                low,
                sp,
                telnet_reaper_guard_below(),
                telnet_reaper_guard_above() + PAGE
            ));
            let mut other = 0;
            while other < TELNET_CLIENTS {
                if other != index {
                    let osp = telnet_client_sp(other);
                    assert!(!overlaps(low, sp, osp - TELNET_STACK_BYTES, osp));
                }
                other += 1;
            }
            index += 1;
        }
        assert!(
            telnet_client_guard_below(TELNET_CLIENTS - 1) >= telnet_reaper_guard_above() + PAGE
        );
    }

    fn guard_pages_unique(slot: usize) -> bool {
        let mut seen = [0u64; 32];
        let mut n = 0;
        let mut index = 0;
        while let Some(page) = user_guard_page(slot, index) {
            let mut prev = 0;
            while prev < n {
                if seen[prev] == page {
                    return false;
                }
                prev += 1;
            }
            seen[n] = page;
            n += 1;
            index += 1;
        }
        n >= 2
    }
}
