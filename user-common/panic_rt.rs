//! Panic line for the EL0 programs.
//!
//! This is its own crate so the formatter is not part of the HTTP (or DNS)
//! codegen unit. Putting `core::fmt` in those programs changes how the
//! worker functions are outlined and drops one of four simultaneous
//! connections. The panic handler in each program calls `report_panic`
//! and then exits or spins exactly as it did before.
//!
//! Syscall 30 writes the UART and the recent buffer. Syscall 28 queues an
//! info record and returns immediately when the queue is full. Neither
//! call is repeated if the handler runs again.

#![no_std]

include!("panic_line.rs");

use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

static REPORTED: AtomicU8 = AtomicU8::new(0);

pub fn report_panic(who: &[u8], info: &core::panic::PanicInfo) {
    if REPORTED.fetch_add(1, Ordering::Relaxed) != 0 {
        return;
    }
    let (file, line) = match info.location() {
        Some(loc) => (loc.file().as_bytes(), loc.line()),
        None => (b"".as_slice(), 0),
    };
    let mut msg = [0u8; 48];
    let msg_len = {
        let mut cap = Cap {
            buf: &mut msg,
            n: 0,
        };
        let _ = write!(cap, "{}", info.message());
        cap.n
    };
    let mut line_buf = [0u8; 80];
    let n = format_panic_line(&mut line_buf, who, file, line, &msg[..msg_len]);
    note_text(&line_buf[..n]);
}

pub fn note_text(text: &[u8]) {
    let n = text.len().min(80);
    if n == 0 {
        return;
    }
    let ptr = text.as_ptr() as u64;
    let len = n as u64;
    let _ = panic_svc(30, ptr, len);
    let _ = panic_svc(28, ptr, len);
}

struct Cap<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl Write for Cap<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() && self.n < self.buf.len() {
            self.buf[self.n] = bytes[i];
            self.n += 1;
            i += 1;
        }
        Ok(())
    }
}

fn panic_svc(num: u64, ptr: u64, len: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") ptr => ret,
            in("x1") len,
            in("x2") 0u64,
            in("x3") 0u64,
            in("x4") 0u64,
            in("x5") 0u64,
        );
    }
    ret as i64
}
