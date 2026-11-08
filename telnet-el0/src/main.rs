//! Telnet log. One thread per connection, fed from the log task's fan-out.
//! A slow client blocks only its own send. The publisher never waits.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};

core::arch::global_asm!(include_str!("start.s"));

mod thread {
    include!("../../user-common/thread.rs");
}

const TCP_LISTEN: u64 = 8;
const TCP_ACCEPT: u64 = 9;
const TCP_RECV: u64 = 10;
const TCP_SEND: u64 = 11;
const TCP_CLOSE: u64 = 12;
const LOG_FOLLOW: u64 = 40;
const LOG_WAKE: u64 = 41;
const YIELD: u64 = 1;
const EXIT: u64 = 3;

const EAGAIN: i64 = -11;
const MAX_CLIENTS: usize = 16;
const HEAP_VA: u64 = 0x1_000A_0000;
const HEAP_SIZE: u64 = 256 * 1024;
const STACK_BYTES: u64 = 8 * 1024;
const PORT: u64 = 23;

static USED: [AtomicU32; MAX_CLIENTS] = [const { AtomicU32::new(0) }; MAX_CLIENTS];
static DONE: [AtomicU32; MAX_CLIENTS] = [const { AtomicU32::new(0) }; MAX_CLIENTS];
static CLOSED: [AtomicU32; MAX_CLIENTS] = [const { AtomicU32::new(0) }; MAX_CLIENTS];
static FD_LIVE: [AtomicU32; MAX_CLIENTS] = [const { AtomicU32::new(0) }; MAX_CLIENTS];
static FD_NUM: [AtomicU32; MAX_CLIENTS] = [const { AtomicU32::new(0) }; MAX_CLIENTS];
static TID: [AtomicI64; MAX_CLIENTS] = [const { AtomicI64::new(0) }; MAX_CLIENTS];

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    run();
}

fn run() -> ! {
    let listen = sys(TCP_LISTEN, PORT, 0, 0, 0, 0, 0);
    if listen < 0 {
        exit_now(1);
    }
    let _ = thread::spawn(reaper, HEAP_VA + STACK_BYTES, 0);
    loop {
        let conn = sys(TCP_ACCEPT, listen as u64, 0, 0, 0, 0, 0);
        if conn < 0 {
            yield_now();
            continue;
        }
        reap();
        match free_slot() {
            Some(index) => {
                CLOSED[index].store(0, Ordering::Release);
                DONE[index].store(0, Ordering::Release);
                FD_NUM[index].store(conn as u32, Ordering::Release);
                FD_LIVE[index].store(1, Ordering::Release);
                USED[index].store(1, Ordering::Release);
                let stack = HEAP_VA + HEAP_SIZE - (index as u64) * STACK_BYTES;
                let arg = ((index as u64) << 32) | (conn as u64);
                let tid = thread::spawn(client, stack, arg);
                if tid < 0 {
                    FD_LIVE[index].store(0, Ordering::Release);
                    USED[index].store(0, Ordering::Release);
                    let _ = sys(TCP_CLOSE, conn as u64, 0, 0, 0, 0, 0);
                } else {
                    TID[index].store(tid, Ordering::Release);
                    let _ = sys(LOG_WAKE, 0, 0, 0, 0, 0, 0);
                }
            }
            None => refuse(conn),
        }
    }
}

fn free_slot() -> Option<usize> {
    let mut index = 0;
    while index < MAX_CLIENTS {
        if USED[index].load(Ordering::Acquire) == 0 {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn reap() {
    let mut index = 0;
    while index < MAX_CLIENTS {
        if USED[index].load(Ordering::Acquire) == 1 {
            let tid = TID[index].load(Ordering::Acquire);
            if tid > 0 {
                let code = thread::try_join(tid);
                if code != -11 {
                    if FD_LIVE[index].swap(0, Ordering::AcqRel) == 1 {
                        let fd = FD_NUM[index].load(Ordering::Acquire);
                        let _ = sys(TCP_CLOSE, fd as u64, 0, 0, 0, 0, 0);
                    }
                    TID[index].store(0, Ordering::Release);
                    DONE[index].store(0, Ordering::Release);
                    USED[index].store(0, Ordering::Release);
                }
            }
        }
        index += 1;
    }
}

fn refuse(fd: i64) {
    let _ = send_all(fd, b"busy, try later\r\n");
    let _ = sys(TCP_CLOSE, fd as u64, 0, 0, 0, 0, 0);
}

extern "C" fn reaper(_arg: u64) {
    let mut cursor = 0u64;
    let mut drops = 0u64;
    let mut line = [0u8; 96];
    loop {
        reap();
        let mut live = false;
        let mut index = 0;
        while index < MAX_CLIENTS {
            if USED[index].load(Ordering::Acquire) == 1 && DONE[index].load(Ordering::Acquire) == 0 {
                live = true;
                if CLOSED[index].load(Ordering::Acquire) == 1 {
                    index += 1;
                    continue;
                }
                if FD_LIVE[index].load(Ordering::Acquire) == 1 {
                    let fd = FD_NUM[index].load(Ordering::Acquire) as i64;
                    if !peer_open(fd) {
                        CLOSED[index].store(1, Ordering::Release);
                        let _ = sys(LOG_WAKE, 0, 0, 0, 0, 0, 0);
                    }
                }
            }
            index += 1;
        }
        if live {
            yield_now();
        } else {
            let _ = follow(&mut cursor, &mut line, &mut drops, 0);
        }
    }
}

extern "C" fn client(arg: u64) {
    let index = (arg >> 32) as usize;
    let fd = (arg & 0xffff_ffff) as i64;
    if index >= MAX_CLIENTS {
        thread::exit(1);
    }
    if !send_all(fd, b"telnet-log\r\n") {
        finish(index, fd);
    }
    let mut cursor = 0u64;
    let mut drops = 0u64;
    let mut line = [0u8; 96];
    loop {
        if CLOSED[index].load(Ordering::Acquire) == 1 {
            break;
        }
        let n = follow(&mut cursor, &mut line, &mut drops, 0);
        if n < 0 {
            break;
        }
        if CLOSED[index].load(Ordering::Acquire) == 1 {
            break;
        }
        if n == 0 {
            continue;
        }
        if drops > 0 {
            let mut marker = [0u8; 48];
            let len = write_drops(drops, &mut marker);
            if !send_all(fd, &marker[..len]) {
                break;
            }
        }
        if !send_line(fd, &line[..n as usize]) {
            break;
        }
    }
    finish(index, fd);
}

fn finish(index: usize, fd: i64) -> ! {
    FD_LIVE[index].store(0, Ordering::Release);
    let _ = sys(TCP_CLOSE, fd as u64, 0, 0, 0, 0, 0);
    DONE[index].store(1, Ordering::Release);
    thread::exit(0);
}

fn follow(cursor: &mut u64, buf: &mut [u8], drops: &mut u64, flags: u64) -> i64 {
    sys(
        LOG_FOLLOW,
        cursor as *mut u64 as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        drops as *mut u64 as u64,
        flags,
        0,
    )
}

fn peer_open(fd: i64) -> bool {
    let mut buf = [0u8; 128];
    let mut spins = 0;
    while spins < 4 {
        let n = sys(
            TCP_RECV,
            fd as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            1,
            0,
            0,
        );
        if n == 0 || (n < 0 && n != EAGAIN) {
            return false;
        }
        if n == EAGAIN {
            return true;
        }
        let _ = strip_iac(&buf[..n as usize]);
        spins += 1;
    }
    true
}

fn strip_iac(bytes: &[u8]) -> usize {
    let mut kept = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0xff {
            kept += 1;
            i += 1;
            continue;
        }
        if i + 1 >= bytes.len() {
            break;
        }
        let cmd = bytes[i + 1];
        if cmd == 0xff {
            kept += 1;
            i += 2;
            continue;
        }
        if cmd == 0xfa {
            i += 2;
            while i + 1 < bytes.len() {
                if bytes[i] == 0xff && bytes[i + 1] == 0xf0 {
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        i += 2;
        if cmd == 0xfb || cmd == 0xfc || cmd == 0xfd || cmd == 0xfe {
            if i < bytes.len() {
                i += 1;
            }
        }
    }
    kept
}

fn send_line(fd: i64, line: &[u8]) -> bool {
    let mut out = [0u8; 98];
    let n = line.len().min(96);
    out[..n].copy_from_slice(&line[..n]);
    out[n] = b'\r';
    out[n + 1] = b'\n';
    send_all(fd, &out[..n + 2])
}

fn send_all(fd: i64, data: &[u8]) -> bool {
    let mut off = 0;
    while off < data.len() {
        let n = sys(
            TCP_SEND,
            fd as u64,
            data[off..].as_ptr() as u64,
            (data.len() - off) as u64,
            0,
            0,
            0,
        );
        if n > 0 {
            off += n as usize;
        } else if n == EAGAIN {
            yield_now();
        } else {
            return false;
        }
    }
    true
}

fn write_drops(n: u64, out: &mut [u8]) -> usize {
    let mut digits = [0u8; 20];
    let mut value = n;
    let mut count = 0;
    if value == 0 {
        digits[0] = b'0';
        count = 1;
    } else {
        while value > 0 && count < digits.len() {
            digits[count] = b'0' + (value % 10) as u8;
            value /= 10;
            count += 1;
        }
    }
    let mut i = 0;
    out[i] = b'[';
    i += 1;
    while count > 0 {
        count -= 1;
        out[i] = digits[count];
        i += 1;
    }
    for byte in b" lines dropped]\r\n" {
        out[i] = *byte;
        i += 1;
    }
    i
}

fn yield_now() {
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
}

fn exit_now(code: u64) -> ! {
    let _ = sys(EXIT, code, 0, 0, 0, 0, 0);
    loop {}
}

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit_now(1);
}
