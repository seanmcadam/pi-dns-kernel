use core::fmt::{self, Write};
use log::{Level, LevelFilter, Metadata, Record};

const UART_DR: *mut u32 = 0x0900_0000 as *mut u32;

struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            unsafe {
                core::ptr::write_volatile(UART_DR, byte as u32);
            }
        }
        Ok(())
    }
}

struct Logger;

impl log::Log for Logger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let level = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let _ = writeln!(UartWriter, "[{}] {}\r", level, record.args());
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

pub fn init() {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(LevelFilter::Info))
        .ok();
}
