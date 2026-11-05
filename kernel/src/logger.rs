use crate::uart_pl011::Pl011Uart;
use core::fmt::Write;
use log::{Level, LevelFilter, Metadata, Record};
use spin::Mutex;

static LOGGER: Logger = Logger {
    uart: Mutex::new(None),
};

struct Logger {
    uart: Mutex<Option<Pl011Uart>>,
}

impl log::Log for Logger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let mut uart = self.uart.lock();
            if uart.is_none() {
                let mut new_uart = Pl011Uart::new();
                new_uart.init();
                *uart = Some(new_uart);
            }
            
            if let Some(uart) = uart.as_mut() {
                let level = match record.level() {
                    Level::Error => "ERROR",
                    Level::Warn => "WARN ",
                    Level::Info => "INFO ",
                    Level::Debug => "DEBUG",
                    Level::Trace => "TRACE",
                };
                
                writeln!(uart, "[{}] {}\r", level, record.args()).ok();
            }
        }
    }

    fn flush(&self) {}
}

pub fn init() {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(LevelFilter::Info))
        .expect("Failed to set logger");
}
