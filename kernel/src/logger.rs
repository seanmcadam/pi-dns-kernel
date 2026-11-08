//! Logging service front door.
//!
//! Before the EL0 log task is running, and after it dies, lines go straight
//! to the UART. That covers early boot, panics, and fault reports that call
//! the console directly. Once the task is live, records go into a fixed
//! queue. A full queue drops the record and counts it. It never waits.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, Ordering};
use log::{Level, LevelFilter, Metadata, Record};
use spin::Mutex;

const CAP: usize = 80;
const QLEN: usize = 32;

struct Slot {
    level: u8,
    flags: u8,
    len: u8,
    data: [u8; CAP],
}

struct Queue {
    slots: [Slot; QLEN],
    head: usize,
    len: usize,
}

impl Queue {
    const fn new() -> Self {
        const EMPTY: Slot = Slot {
            level: 0,
            flags: 0,
            len: 0,
            data: [0; CAP],
        };
        Self {
            slots: [EMPTY; QLEN],
            head: 0,
            len: 0,
        }
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static DROPS: AtomicU64 = AtomicU64::new(0);

struct Recent {
    buf: [u8; 1024],
    len: usize,
}

static RECENT: Mutex<Recent> = Mutex::new(Recent {
    buf: [0; 1024],
    len: 0,
});

struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::console::write_str(s);
        Ok(())
    }
}

struct Sink {
    buf: [u8; CAP],
    n: usize,
}

impl Write for Sink {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if self.n < CAP {
                self.buf[self.n] = byte;
                self.n += 1;
            }
        }
        Ok(())
    }
}

/// `flags` bit 0 asks the log task to append the text to `LOG.TXT`.
/// Returns 0 if queued, 1 if the queue was full.
pub fn post(level: u8, flags: u8, bytes: &[u8]) -> i64 {
    let mut queue = QUEUE.lock();
    if queue.len == QLEN {
        drop(queue);
        DROPS.fetch_add(1, Ordering::Relaxed);
        return 1;
    }
    let index = (queue.head + queue.len) % QLEN;
    let slot = &mut queue.slots[index];
    let n = bytes.len().min(CAP);
    slot.level = level;
    slot.flags = flags;
    slot.len = n as u8;
    slot.data[..n].copy_from_slice(&bytes[..n]);
    queue.len += 1;
    0
}

/// Copy one record into `out` as `level, flags, text`. Returns 0 if empty.
pub fn drain(out: &mut [u8]) -> usize {
    if out.len() < 2 {
        return 0;
    }
    let mut queue = QUEUE.lock();
    if queue.len == 0 {
        return 0;
    }
    let slot = &queue.slots[queue.head];
    let n = slot.len as usize;
    let copy = n.min(out.len() - 2);
    out[0] = slot.level;
    out[1] = slot.flags;
    out[2..2 + copy].copy_from_slice(&slot.data[..copy]);
    queue.head = (queue.head + 1) % QLEN;
    queue.len -= 1;
    2 + copy
}

pub fn emit_line(text: &str) {
    let _ = writeln!(UartWriter, "{text}\r");
    let mut recent = RECENT.lock();
    let line = text.as_bytes();
    let need = line.len() + 1;
    if need >= recent.buf.len() {
        return;
    }
    while recent.len + need > recent.buf.len() && recent.len > 0 {
        let len = recent.len;
        let cut = recent.buf[..len]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(len);
        recent.buf.copy_within(cut..len, 0);
        recent.len = len - cut;
    }
    let end = recent.len;
    recent.buf[end..end + line.len()].copy_from_slice(line);
    recent.buf[end + line.len()] = b'\n';
    recent.len += need;
}

pub fn recent(out: &mut [u8]) -> usize {
    let recent = RECENT.lock();
    let n = recent.len.min(out.len());
    out[..n].copy_from_slice(&recent.buf[..n]);
    n
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
            Level::Error => 3,
            Level::Warn => 2,
            Level::Info => 1,
            Level::Debug | Level::Trace => 4,
        };
        if crate::task::log_live() {
            let mut sink = Sink {
                buf: [0; CAP],
                n: 0,
            };
            let _ = write!(sink, "{}", record.args());
            post(level, 0, &sink.buf[..sink.n]);
            return;
        }
        let label = match record.level() {
            Level::Error => "ERROR",
            Level::Warn => "WARN ",
            Level::Info => "INFO ",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        };
        let _ = writeln!(UartWriter, "[{label}] {}\r", record.args());
    }

    fn flush(&self) {}
}

static LOGGER: Logger = Logger;

pub fn init() {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(LevelFilter::Info))
        .ok();
}
