use core::fmt::Write;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU8, Ordering};

struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::console::write_str(s);
        Ok(())
    }
}

static PANIC_ONCE: AtomicU8 = AtomicU8::new(0);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if PANIC_ONCE.fetch_add(1, Ordering::Relaxed) != 0 {
        crate::console::write_str("\r\nnested kernel panic\r\n");
        #[cfg(feature = "board-pi4")]
        crate::act::halt_with("nested kernel panic");
        #[cfg(not(feature = "board-pi4"))]
        halt();
    }
    let _ = writeln!(UartWriter, "\r\n!!! KERNEL PANIC !!!\r\n{}\r\n", info);
    let (file, line) = match info.location() {
        Some(loc) => (loc.file().as_bytes(), loc.line()),
        None => (b"".as_slice(), 0),
    };
    let mut msg = [0u8; 48];
    let msg_len = {
        let mut cap = Msg {
            buf: &mut msg,
            n: 0,
        };
        let _ = write!(cap, "{}", info.message());
        cap.n
    };
    let mut raw = [0u8; 80];
    let n = crate::fault_log::format_panic_line(&mut raw, b"kern", file, line, &msg[..msg_len]);
    if let Ok(text) = core::str::from_utf8(&raw[..n]) {
        crate::logger::note_fault(text);
        #[cfg(feature = "board-pi4")]
        crate::act::halt_with(text);
    }
    halt();
}

fn halt() -> ! {
    #[cfg(feature = "board-pi4")]
    crate::act::halt_blink();
    #[cfg(not(feature = "board-pi4"))]
    loop {
        cortex_a::asm::wfe();
    }
}

struct Msg<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl Write for Msg<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            if self.n < self.buf.len() {
                self.buf[self.n] = byte;
                self.n += 1;
            }
        }
        Ok(())
    }
}
