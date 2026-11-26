// Syscall numbers and the `svc #0` wrapper shared by the EL0 programs.
//
// Include from the crate root:
// `mod syscall { include!("../../user-common/syscall.rs"); }`
// `thread.rs` calls `crate::syscall::sys`. The kernel linker scripts,
// SDHCI, consoles, and PSCI stay in the kernel.

pub const LOG: u64 = 0;
pub const YIELD: u64 = 1;
pub const UPTIME_TICKS: u64 = 2;
pub const EXIT: u64 = 3;
pub const TIME_MS: u64 = 4;
pub const UDP_BIND: u64 = 5;
pub const UDP_RECV: u64 = 6;
pub const UDP_SEND: u64 = 7;
pub const TCP_LISTEN: u64 = 8;
pub const TCP_ACCEPT: u64 = 9;
pub const TCP_RECV: u64 = 10;
pub const TCP_SEND: u64 = 11;
pub const TCP_CLOSE: u64 = 12;
pub const CONFIG_RECORDS: u64 = 13;
pub const CONFIG_UPSTREAMS: u64 = 14;
pub const CONFIG_BLOCKLIST: u64 = 15;
pub const CONFIG_CACHE_SIZE: u64 = 16;
pub const SET_CACHE_ENTRIES: u64 = 17;
pub const AUTH: u64 = 18;
pub const CONFIG_ADD: u64 = 19;
pub const CONFIG_REMOVE: u64 = 20;
pub const UPTIME_SECS: u64 = 21;
pub const CACHE_ENTRIES: u64 = 22;
pub const FAULT_GUARD: u64 = 23;
pub const FAULT_TEXT: u64 = 24;
pub const HTTP_INFO: u64 = 25;
pub const TICK_HZ: u64 = 26;
pub const DEBUG_SPIN: u64 = 27;
pub const LOG_POST: u64 = 28;
pub const LOG_DRAIN: u64 = 29;
pub const LOG_EMIT: u64 = 30;
pub const LOG_RECENT: u64 = 31;
pub const FILE_READ: u64 = 32;
pub const FILE_WRITE: u64 = 33;
pub const THREAD_CREATE: u64 = 34;
pub const THREAD_EXIT: u64 = 35;
pub const THREAD_JOIN: u64 = 36;
pub const FUTEX_WAIT: u64 = 37;
pub const FUTEX_WAKE: u64 = 38;
pub const LOG_PUBLISH: u64 = 39;
pub const LOG_FOLLOW: u64 = 40;
pub const LOG_WAKE: u64 = 41;
pub const DEBUG_STATUS: u64 = 42;
pub const DEBUG_CRASH: u64 = 43;
pub const THREAD_TRY_JOIN: u64 = 44;
pub const LOG_RELOAD: u64 = 45;
pub const CONFIG_READ: u64 = 46;
pub const CONFIG_WRITE: u64 = 47;
pub const LOG_FILL: u64 = 48;
pub const CONFIG_SET_CACHE: u64 = 49;
pub const CONFIG_CACHE_REAP: u64 = 50;
pub const TIMER_TICKS: u64 = 51;
pub const TIMER_FREQ: u64 = 52;
pub const SET_CACHE_STATS: u64 = 53;
pub const CACHE_PLANT: u64 = 54;
pub const DISPLAY_IO: u64 = 55;
pub const DISPLAY_SET: u64 = 56;
pub const SET_DNS_TOP: u64 = 57;
pub const POWER: u64 = 58;
pub const VNC_CTRL: u64 = 59;
pub const SET_UPSTREAM_HEALTH: u64 = 60;
pub const UPSTREAM_HEALTH: u64 = 61;
pub const CONFIG_PROBE_SECS: u64 = 62;
pub const NET_READY: u64 = 63;
pub const NET_COUNTERS: u64 = 64;
pub const SET_DNS_LOAD: u64 = 65;
pub const NET_EXTRA: u64 = 66;
pub const FW: u64 = 67;
pub const SD_STATUS: u64 = 68;
pub const HDMI_CONSOLE: u64 = 69;
pub const VCLOG_READ: u64 = 70;
pub const CRASH: u64 = 71;
pub const BOOT: u64 = 72;
pub const BOARD: u64 = 73;
pub const CSRF: u64 = 74;
pub const NET_ARM: u64 = 75;
pub const CLOCK: u64 = 76;
pub const CLOCK_PREFIX: u64 = 77;
pub const HEAP: u64 = 78;
pub const TCP_CONNECT: u64 = 79;
pub const BLOCK: u64 = 80;
pub const LOG_FILE: u64 = 81;
pub const HTTP_HOLD: u64 = 82;

/// Job ring full. `TCP_CLOSE` restores the previous descriptor kind.
pub const EAGAIN: i64 = -11;

pub fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
        );
    }
    ret as i64
}

pub fn yield_now() {
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
}

/// Retry `TCP_CLOSE` while the job ring is full. Dropping `EAGAIN` leaves
/// the descriptor as a live connection, which recycle does not reap.
pub fn close_tcp(fd: i64) {
    loop {
        let rc = sys(TCP_CLOSE, fd as u64, 0, 0, 0, 0, 0);
        if rc != EAGAIN {
            return;
        }
        yield_now();
    }
}
