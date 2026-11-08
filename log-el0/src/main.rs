//! Log task. Formats queued records, prints them, and appends the ones the
//! kernel marked for the FAT log. Debug lines are dropped. A full queue is
//! the kernel's problem: this task only drains what was kept.

#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("start.s"));

const DRAIN: u64 = 29;
const EMIT: u64 = 30;
const FILE_READ: u64 = 32;
const FILE_WRITE: u64 = 33;
const YIELD: u64 = 1;
const EXIT: u64 = 3;

const PERSIST: u8 = 1;

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    run();
}

fn run() -> ! {
    let mut raw = [0u8; 96];
    loop {
        let n = sys(DRAIN, raw.as_mut_ptr() as u64, raw.len() as u64, 0, 0, 0, 0);
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
        let prefix: &[u8] = match level {
            3 => b"[ERROR] ",
            2 => b"[WARN ] ",
            _ => b"[INFO ] ",
        };
        let room = line.len() - prefix.len();
        let take = msg.len().min(room);
        line[..prefix.len()].copy_from_slice(prefix);
        line[prefix.len()..prefix.len() + take].copy_from_slice(&msg[..take]);
        let end = prefix.len() + take;
        let _ = sys(EMIT, line.as_ptr() as u64, end as u64, 0, 0, 0, 0);
        if flags & PERSIST != 0 {
            persist(msg);
        }
        yield_now();
    }
}

fn persist(msg: &[u8]) {
    let mut file = [0u8; 600];
    let n = file_read(b"LOG.TXT", &mut file);
    if n >= 512 {
        let _ = file_write(b"LOG.OLD", &file[..n], false);
        let _ = file_write(b"LOG.TXT", b"", false);
    }
    let mut line = [b' '; 73];
    let take = msg.len().min(72);
    line[..take].copy_from_slice(&msg[..take]);
    line[72] = b'\n';
    let _ = file_write(b"LOG.TXT", &line, true);
}

fn file_read(name: &[u8], buf: &mut [u8]) -> usize {
    let n = sys(
        FILE_READ,
        name.as_ptr() as u64,
        name.len() as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
        0,
        0,
    );
    if n < 0 {
        0
    } else {
        n as usize
    }
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
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
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
    let _ = sys(EXIT, 1, 0, 0, 0, 0, 0);
    loop {}
}
