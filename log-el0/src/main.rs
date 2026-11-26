//! Log task. Formats queued records, prints them, and appends the ones the
//! kernel marked for the FAT log. Debug lines are dropped. A full queue is
//! the kernel's problem: this task only drains what was kept.

#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("../../user-common/start.s"));

mod syscall {
    #![allow(dead_code)]
    include!("../../user-common/syscall.rs");
}

use syscall::{sys, CLOCK_PREFIX, EXIT, FILE_WRITE, LOG_DRAIN, LOG_EMIT, LOG_PUBLISH, LOG_RELOAD};

const PERSIST: u8 = 1;

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    run();
}

fn run() -> ! {
    let _ = sys(LOG_RELOAD, 0, 0, 0, 0, 0, 0);
    let mut raw = [0u8; 96];
    let mut persisted = 0u32;
    loop {
        let n = sys(LOG_DRAIN, raw.as_mut_ptr() as u64, raw.len() as u64, 0, 0, 0, 0);
        if n < 2 {
            yield_now();
            continue;
        }
        let level = raw[0];
        let flags = raw[1];
        let msg = &raw[2..n as usize];
        if level == 0 || level >= 4 || msg.is_empty() {
            continue;
        }
        let mut line = [0u8; 96];
        let mut stamp = [0u8; 40];
        let stamp_n = sys(
            CLOCK_PREFIX,
            stamp.as_mut_ptr() as u64,
            stamp.len() as u64,
            0,
            0,
            0,
            0,
        );
        let stamp_n = if stamp_n > 0 {
            stamp_n as usize
        } else {
            0
        };
        let prefix: &[u8] = match level {
            3 => b"[ERROR] ",
            2 => b"[WARN ] ",
            _ => b"[INFO ] ",
        };
        let head = stamp_n.min(40);
        line[..head].copy_from_slice(&stamp[..head]);
        let mut at = head;
        let tag = prefix.len().min(line.len() - at);
        line[at..at + tag].copy_from_slice(&prefix[..tag]);
        at += tag;
        let take = msg.len().min(line.len() - at);
        line[at..at + take].copy_from_slice(&msg[..take]);
        let end = at + take;
        let _ = sys(LOG_EMIT, line.as_ptr() as u64, end as u64, 0, 0, 0, 0);
        let _ = sys(LOG_PUBLISH, line.as_ptr() as u64, end as u64, 0, 0, 0, 0);
        if flags & PERSIST != 0 {
            if persisted % 16 == 0 {
                let _ = sys(LOG_RELOAD, 0, 0, 0, 0, 0, 0);
            }
            persisted = persisted.wrapping_add(1);
            persist(msg);
        }
        yield_now();
    }
}

fn persist(msg: &[u8]) {
    let mut line = [b' '; 73];
    let mut stamp = [0u8; 40];
    let stamp_n = sys(
        CLOCK_PREFIX,
        stamp.as_mut_ptr() as u64,
        stamp.len() as u64,
        0,
        0,
        0,
        0,
    );
    let stamp_n = if stamp_n > 0 { (stamp_n as usize).min(40) } else { 0 };
    line[..stamp_n].copy_from_slice(&stamp[..stamp_n]);
    let take = msg.len().min(72 - stamp_n);
    line[stamp_n..stamp_n + take].copy_from_slice(&msg[..take]);
    line[72] = b'\n';
    let _ = file_write(b"LOG.TXT", &line, true);
}

fn file_write(name: &[u8], data: &[u8], append: bool) -> i64 {
    sys(
        FILE_WRITE,
        name.as_ptr() as u64,
        name.len() as u64,
        data.as_ptr() as u64,
        data.len() as u64,
        if append { 1 } else { 0 },
        0,
    )
}

fn yield_now() {
    syscall::yield_now();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    panic_rt::report_panic(b"log", info);
    let _ = sys(EXIT, 1, 0, 0, 0, 0, 0);
    loop {}
}
