//! Logging service front door.
//!
//! Before the EL0 log task is running, and after it dies, lines go straight
//! to the UART. That covers early boot, panics, and fault reports that call
//! the console directly. Once the task is live, records go into a fixed
//! queue. A full queue drops the record and counts it. It never waits.
//!
//! Fault records use `try_lock` on the queue and the recent buffer. If
//! either lock is already held, that step is skipped. A fatal halt does
//! not drain the queue: the UART line is the record that is already
//! written, and `LOG.TXT` or telnet only see the line if another core's
//! log task drains the queue before the machine stops.

use core::cell::UnsafeCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
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
    push_recent(&mut recent, text);
}

/// Queue one error line for the log task. Never waits.
///
/// Returns whether the log task will print and, because the persist flag
/// is set, append the line. A busy or full queue returns false and counts
/// a drop only when the queue was full. The caller prints the UART line.
pub fn try_queue_fault(text: &str) -> bool {
    let live = crate::task::log_live();
    let mut queue_free = false;
    let mut full = false;
    let stored = if live {
        match QUEUE.try_lock() {
            Some(mut queue) => {
                queue_free = true;
                if queue.len == QLEN {
                    full = true;
                    false
                } else {
                    let bytes = text.as_bytes();
                    let n = bytes.len().min(CAP);
                    let index = (queue.head + queue.len) % QLEN;
                    let slot = &mut queue.slots[index];
                    slot.level = 3;
                    slot.flags = 1;
                    slot.len = n as u8;
                    slot.data[..n].copy_from_slice(&bytes[..n]);
                    queue.len += 1;
                    true
                }
            }
            None => false,
        }
    } else {
        false
    };
    match crate::fault_log::action(true, live, queue_free, full) {
        crate::fault_log::FaultAction::Queue => stored,
        crate::fault_log::FaultAction::Direct => {
            if full {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
            false
        }
        crate::fault_log::FaultAction::Nested => false,
    }
}

/// UART, and the recent buffer when its lock is free. Does not publish to
/// the telnet ring: the log task is the only publisher, and a fault may
/// have interrupted it mid-publish.
pub fn direct_fault(text: &str) {
    let _ = writeln!(UartWriter, "{text}\r");
    if let Some(mut recent) = RECENT.try_lock() {
        push_recent(&mut recent, text);
    }
}

/// Best-effort fault record. Matches `fault_log::action`: a nested call
/// only writes a fixed UART line, and a busy, full, or down queue falls
/// back to the UART. The queue itself is never waited on.
pub fn note_fault(text: &str) {
    if !crate::fault_log::enter() {
        if crate::fault_log::action(false, false, false, false)
            == crate::fault_log::FaultAction::Nested
        {
            crate::console::write_str("\r\nnested fault log\r\n");
        }
        return;
    }
    // The UART line is written even when the queue accepts the record.
    // The log task may be the one that faulted, or it may not drain
    // before a halt. A second copy appears if that task is still running.
    direct_fault(text);
    let _ = try_queue_fault(text);
    crate::fault_log::leave();
}

fn push_recent(recent: &mut Recent, text: &str) {
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

/// Broadcast ring for telnet readers.
///
/// The log task is the only publisher. It stores one line and moves on.
/// Each reader keeps its own cursor. A reader that falls behind by more
/// than the ring skips ahead and is told how many lines it missed. Nothing
/// in this path waits, and it does not take the log queue lock.

const FAN_LINES: usize = 64;
const FAN_CAP: usize = 96;

struct FanSlot {
    seq: AtomicU64,
    len: AtomicUsize,
    data: UnsafeCell<[u8; FAN_CAP]>,
}

unsafe impl Sync for FanSlot {}

const fn empty_fan() -> FanSlot {
    FanSlot {
        seq: AtomicU64::new(0),
        len: AtomicUsize::new(0),
        data: UnsafeCell::new([0; FAN_CAP]),
    }
}

static FAN: [FanSlot; FAN_LINES] = [const { empty_fan() }; FAN_LINES];
static FAN_HEAD: AtomicU64 = AtomicU64::new(0);

pub struct FanLine {
    pub next: u64,
    pub dropped: u64,
    pub len: usize,
    pub bytes: [u8; FAN_CAP],
}

/// UART, the recent buffer, and the telnet ring. Used when the log task
/// itself is down, so a crash line is not stuck behind that task.
pub fn announce(text: &str) {
    emit_line(text);
    fan_publish(text.as_bytes());
}

pub fn fan_publish(text: &[u8]) {
    let n = text.len().min(FAN_CAP);
    let seq = FAN_HEAD.fetch_add(1, Ordering::Relaxed) + 1;
    let slot = &FAN[(seq as usize - 1) % FAN_LINES];
    unsafe {
        let dst = &mut *slot.data.get();
        dst[..n].copy_from_slice(&text[..n]);
    }
    slot.len.store(n, Ordering::Relaxed);
    slot.seq.store(seq, Ordering::Release);
}

pub fn fan_read(cursor: u64) -> Option<FanLine> {
    let head = FAN_HEAD.load(Ordering::Acquire);
    if cursor >= head {
        return None;
    }
    let mut cur = cursor;
    let mut dropped = 0u64;
    let lag = head - cur;
    if lag > FAN_LINES as u64 {
        dropped = lag - FAN_LINES as u64;
        cur = head - FAN_LINES as u64;
    }
    let mut tries = 0;
    while tries < FAN_LINES {
        tries += 1;
        let seq = cur + 1;
        if seq > head {
            return None;
        }
        let slot = &FAN[(seq as usize - 1) % FAN_LINES];
        let seen = slot.seq.load(Ordering::Acquire);
        if seen == seq {
            let n = slot.len.load(Ordering::Relaxed).min(FAN_CAP);
            let mut bytes = [0u8; FAN_CAP];
            unsafe {
                core::ptr::copy_nonoverlapping(slot.data.get().cast::<u8>(), bytes.as_mut_ptr(), n);
            }
            if slot.seq.load(Ordering::Acquire) != seq {
                let now = slot.seq.load(Ordering::Acquire);
                if now > seq {
                    dropped += now - seq;
                    cur = now - 1;
                    continue;
                }
                continue;
            }
            return Some(FanLine {
                next: seq,
                dropped,
                len: n,
                bytes,
            });
        }
        if seen > seq {
            dropped += seen - seq;
            cur = seen - 1;
            continue;
        }
        return None;
    }
    None
}

pub fn init() {
    log::set_logger(&LOGGER)
        .map(|()| log::set_max_level(LevelFilter::Info))
        .ok();
}
