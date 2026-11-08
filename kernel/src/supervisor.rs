//! Restarts an EL0 service after its main thread faults or exits.
//!
//! Teardown runs once: every thread of that process is stopped, its futex
//! and log-follow entries are dropped, its fds are closed through the network
//! owner, its user page tables are released, and its ASID is invalidated and
//! recycled. The backing pages are a fixed BSS pool, so the free-page count
//! is that pool minus the pages still held by live services.
//!
//! A service that stays up for 60 seconds goes back to a 100 ms restart
//! delay. Five deaths inside 60 seconds leave it down. `CRASH_LIMIT` is the
//! knob. The supervisor lock is taken by itself and dropped before `SCHED`,
//! `FD`, `CONFIG`, `FS`, or `LOGQ`.

use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use crate::spin::{Spin, SpinGuard};

pub const CRASH_LIMIT: u32 = 5;
const WINDOW_MS: u64 = 60_000;
const HEALTHY_MS: u64 = 60_000;
const BACKOFF_START_MS: u64 = 100;
const BACKOFF_CAP_MS: u64 = 10_000;
const PAGE_POOL: u32 = 4096;
const SLOTS: usize = 5;

const OFF: u8 = 0;
const RUNNING: u8 = 1;
const DRAINING: u8 = 2;
const BACKOFF: u8 = 3;
const GAVEUP: u8 = 4;

struct Svc {
    phase: u8,
    adopted: bool,
    accounted: bool,
    pending_restart: bool,
    asid: u8,
    restarts: u32,
    backoff_ms: u64,
    next_ms: u64,
    started_ms: u64,
    last_ec: u64,
    last_elr: u64,
    last_far: u64,
    last_ms: u64,
    crash_ms: [u64; 8],
    crash_n: u8,
}

const fn blank() -> Svc {
    Svc {
        phase: OFF,
        adopted: false,
        accounted: false,
        pending_restart: false,
        asid: 0,
        restarts: 0,
        backoff_ms: BACKOFF_START_MS,
        next_ms: 0,
        started_ms: 0,
        last_ec: 0,
        last_elr: 0,
        last_far: 0,
        last_ms: 0,
        crash_ms: [0; 8],
        crash_n: 0,
    }
}

static SUPER: Spin = Spin::new();
static mut SVC: [Svc; SLOTS] = [const { blank() }; SLOTS];
static mut ASID_USED: u32 = 0;
static mut USED_PAGES: u32 = 0;
static ASID_PUB: [AtomicU8; SLOTS] = [const { AtomicU8::new(0) }; SLOTS];
static DEAD: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

fn guard() -> SpinGuard<'static> {
    SUPER.lock()
}

fn svc(slot: usize) -> *mut Svc {
    unsafe { addr_of_mut!(SVC[slot]) }
}

pub fn dead(slot: usize) -> bool {
    slot < SLOTS && DEAD[slot].load(Ordering::Acquire) != 0
}

/// ASID installed for this process. Before the first claim, and after it is
/// released, this is the slot's preferred id so a boot-test copy still hits
/// the translation the scheduler used to install by hand.
pub fn asid(slot: usize) -> u64 {
    if slot >= SLOTS {
        return 0;
    }
    let live = ASID_PUB[slot].load(Ordering::Acquire);
    if live == 0 {
        (slot as u64) + 1
    } else {
        live as u64
    }
}

pub fn launch(slot: usize) -> Result<(), ()> {
    {
        let _hold = guard();
        if slot >= SLOTS {
            return Err(());
        }
        match unsafe { (*svc(slot)).phase } {
            RUNNING => return Ok(()),
            DRAINING | GAVEUP => return Err(()),
            _ => {}
        }
    }
    match slot {
        0 => crate::task::start_dns(),
        1 => crate::task::start_http(),
        2 => crate::task::start_spin(),
        3 => crate::task::start_log(),
        4 => crate::task::start_telnet(),
        _ => Err(()),
    }
}

/// Called once the user image is mapped and the main thread is about to run.
pub fn begin(slot: usize) {
    let mut restart = false;
    let name = name(slot);
    {
        let _hold = guard();
        if slot >= SLOTS {
            return;
        }
        unsafe {
            let row = &mut *svc(slot);
            if row.asid == 0 {
                row.asid = claim_asid(slot);
                ASID_PUB[slot].store(row.asid, Ordering::Release);
            }
            if !row.accounted {
                USED_PAGES = USED_PAGES.saturating_add(pages(slot));
                row.accounted = true;
            }
            row.adopted = true;
            row.phase = RUNNING;
            row.started_ms = crate::timer::millis();
            if row.pending_restart {
                row.pending_restart = false;
                row.restarts = row.restarts.saturating_add(1);
                restart = true;
            }
        }
        DEAD[slot].store(0, Ordering::Release);
    }
    if restart {
        say(1, name, "restart", 0, 0, 0);
    }
}

pub fn note(slot: usize, ec: u64, elr: u64, far: u64) {
    if slot >= SLOTS {
        return;
    }
    let _hold = guard();
    unsafe {
        let row = &mut *svc(slot);
        if !row.adopted {
            return;
        }
        row.last_ec = ec;
        row.last_elr = elr;
        row.last_far = far;
        row.last_ms = crate::timer::millis();
    }
}

pub fn worker_fault(slot: usize, ec: u64, elr: u64, far: u64) {
    if slot >= SLOTS || dead(slot) {
        return;
    }
    let adopted = {
        let _hold = guard();
        unsafe { (*svc(slot)).adopted }
    };
    if !adopted {
        return;
    }
    say(2, name(slot), "worker fault", ec, elr, far);
}

/// Main thread faulted or exited. Safe to call again while the drain is
/// still waiting on another core or the network owner.
pub fn died(slot: usize) {
    if slot >= SLOTS {
        return;
    }
    let start = {
        let _hold = guard();
        let phase = unsafe { (*svc(slot)).phase };
        if !unsafe { (*svc(slot)).adopted } {
            return;
        }
        match phase {
            RUNNING => {
                unsafe { (*svc(slot)).phase = DRAINING };
                DEAD[slot].store(1, Ordering::Release);
                true
            }
            DRAINING => false,
            _ => return,
        }
    };
    crate::task::clear_live(slot);
    if start {
        crate::task::kill_slot(slot);
    }
    finish(slot);
}

pub fn tick() {
    let now = crate::timer::millis();
    crate::task::nudge_injected(now);
    let mut slot = 0;
    while slot < SLOTS {
        finish(slot);
        relax(slot, now);
        relaunch(slot, now);
        slot += 1;
    }
}

pub fn arm(slot: usize, worker: bool) -> i64 {
    if slot >= SLOTS {
        return -4;
    }
    {
        let _hold = guard();
        unsafe {
            let row = &*svc(slot);
            if !row.adopted || row.phase != RUNNING {
                return -1;
            }
        }
    }
    if worker {
        crate::task::inject_worker(slot)
    } else {
        let delay = if crate::percpu::slot() == slot { 500 } else { 0 };
        crate::task::inject_main(slot, delay)
    }
}

pub fn status(ptr: u64, cap: u64) -> i64 {
    if cap == 0 || cap > 4096 || !crate::mmu::user_write_ok(ptr, cap) {
        return -2;
    }
    let mut out = Text::new();
    let now = crate::timer::millis();
    let snap = {
        let _hold = guard();
        let mut rows = [const { Snap::blank() }; SLOTS];
        let mut i = 0;
        while i < SLOTS {
            unsafe {
                let row = &*svc(i);
                rows[i] = Snap {
                    phase: row.phase,
                    restarts: row.restarts,
                    ec: row.last_ec,
                    elr: row.last_elr,
                    far: row.last_far,
                    at: row.last_ms,
                    started: row.started_ms,
                };
            }
            i += 1;
        }
        let pages = unsafe { USED_PAGES };
        let asids = asid_count();
        (rows, pages, asids)
    };
    let fds = crate::user_fd::open_fds();
    let mut i = 0;
    while i < SLOTS {
        let row = &snap.0[i];
        out.push(name(i));
        out.push(" ");
        out.push(phase_name(row.phase));
        out.push(" restarts=");
        out.dec(row.restarts as u64);
        out.push(" ec=");
        out.hex(row.ec);
        out.push(" elr=");
        out.hex(row.elr);
        out.push(" far=");
        out.hex(row.far);
        out.push(" at=");
        out.dec(row.at);
        out.push(" uptime_ms=");
        let up = if row.phase == RUNNING {
            now.saturating_sub(row.started)
        } else {
            0
        };
        out.dec(up);
        out.push("\n");
        i += 1;
    }
    out.push("free_pages=");
    out.dec(PAGE_POOL.saturating_sub(snap.1) as u64);
    out.push("\nasids=");
    out.dec(snap.2 as u64);
    out.push("\nfds=");
    out.dec(fds as u64);
    out.push("\nlimit=");
    out.dec(CRASH_LIMIT as u64);
    out.push("\n");
    let n = out.len.min(cap as usize);
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(out.data.as_ptr(), ptr as *mut u8, n);
    }
    crate::mmu::set_pan(true);
    n as i64
}

struct Snap {
    phase: u8,
    restarts: u32,
    ec: u64,
    elr: u64,
    far: u64,
    at: u64,
    started: u64,
}

impl Snap {
    const fn blank() -> Self {
        Self {
            phase: OFF,
            restarts: 0,
            ec: 0,
            elr: 0,
            far: 0,
            at: 0,
            started: 0,
        }
    }
}

fn finish(slot: usize) {
    {
        let _hold = guard();
        if slot >= SLOTS || unsafe { (*svc(slot)).phase } != DRAINING {
            return;
        }
    }
    if crate::task::slot_busy(slot) {
        return;
    }
    if !crate::user_fd::release_fds(slot) {
        return;
    }
    let (asid, ec, elr, far) = {
        let _hold = guard();
        if unsafe { (*svc(slot)).phase } != DRAINING {
            return;
        }
        unsafe {
            let row = &*svc(slot);
            (row.asid, row.last_ec, row.last_elr, row.last_far)
        }
    };
    crate::mmu::release_user_root(slot);
    if asid != 0 {
        crate::mmu::tlbi_asid(asid as u64);
    }
    crate::task::poke_others();
    let outcome = {
        let _hold = guard();
        if unsafe { (*svc(slot)).phase } != DRAINING {
            return;
        }
        unsafe {
            let row = &mut *svc(slot);
            if row.asid != 0 {
                ASID_USED &= !(1u32 << row.asid);
                row.asid = 0;
            }
            ASID_PUB[slot].store(0, Ordering::Release);
            if row.accounted {
                USED_PAGES = USED_PAGES.saturating_sub(pages(slot));
                row.accounted = false;
            }
            let gave = record_crash(row);
            row.pending_restart = !gave;
            row.phase = if gave { GAVEUP } else { BACKOFF };
            gave
        }
    };
    let label = name(slot);
    if outcome {
        say(3, label, "gave up", ec, elr, far);
    } else {
        say(2, label, "fault", ec, elr, far);
    }
}

fn record_crash(row: &mut Svc) -> bool {
    let now = crate::timer::millis();
    if row.crash_n < 8 {
        row.crash_ms[row.crash_n as usize] = now;
        row.crash_n += 1;
    } else {
        let mut i = 1;
        while i < 8 {
            row.crash_ms[i - 1] = row.crash_ms[i];
            i += 1;
        }
        row.crash_ms[7] = now;
    }
    let mut hits = 0u32;
    let mut i = 0;
    while i < row.crash_n as usize {
        if now.saturating_sub(row.crash_ms[i]) <= WINDOW_MS {
            hits += 1;
        }
        i += 1;
    }
    if hits >= CRASH_LIMIT {
        return true;
    }
    row.next_ms = now.saturating_add(row.backoff_ms);
    let doubled = row.backoff_ms.saturating_mul(2);
    row.backoff_ms = if doubled > BACKOFF_CAP_MS {
        BACKOFF_CAP_MS
    } else {
        doubled
    };
    false
}

fn relax(slot: usize, now: u64) {
    let _hold = guard();
    unsafe {
        let row = &mut *svc(slot);
        if row.phase != RUNNING {
            return;
        }
        if now.saturating_sub(row.started_ms) < HEALTHY_MS {
            return;
        }
        if row.last_ms != 0 && now.saturating_sub(row.last_ms) < HEALTHY_MS {
            return;
        }
        row.backoff_ms = BACKOFF_START_MS;
        row.crash_n = 0;
        row.crash_ms = [0; 8];
    }
}

fn relaunch(slot: usize, now: u64) {
    {
        let _hold = guard();
        unsafe {
            let row = &*svc(slot);
            if row.phase != BACKOFF || now < row.next_ms {
                return;
            }
        }
    }
    if crate::task::slot_busy(slot) || !crate::user_fd::idle(slot) {
        let _ = crate::user_fd::release_fds(slot);
        return;
    }
    let _ = launch(slot);
}

fn claim_asid(prefer_slot: usize) -> u8 {
    let prefer = (prefer_slot as u32) + 1;
    unsafe {
        if prefer >= 1 && prefer <= 31 && ASID_USED & (1u32 << prefer) == 0 {
            ASID_USED |= 1u32 << prefer;
            return prefer as u8;
        }
        let mut i = 1u32;
        while i <= 31 {
            if ASID_USED & (1u32 << i) == 0 {
                ASID_USED |= 1u32 << i;
                return i as u8;
            }
            i += 1;
        }
    }
    1
}

fn asid_count() -> u32 {
    let mut n = 0u32;
    let bits = unsafe { ASID_USED };
    let mut i = 1u32;
    while i <= 31 {
        if bits & (1u32 << i) != 0 {
            n += 1;
        }
        i += 1;
    }
    n
}

fn pages(slot: usize) -> u32 {
    match slot {
        0 | 1 => 544,
        2 => 2,
        3 => 37,
        4 => 101,
        _ => 0,
    }
}

fn name(slot: usize) -> &'static str {
    match slot {
        0 => "dns",
        1 => "http",
        2 => "spin",
        3 => "log",
        4 => "telnet",
        _ => "task",
    }
}

fn phase_name(phase: u8) -> &'static str {
    match phase {
        RUNNING => "running",
        DRAINING => "draining",
        BACKOFF => "backoff",
        GAVEUP => "gaveup",
        _ => "off",
    }
}

fn say(level: u8, name: &str, what: &str, ec: u64, elr: u64, far: u64) {
    let mut text = Text::new();
    text.push("supervisor ");
    text.push(name);
    text.push(" ");
    text.push(what);
    if ec != 0 || elr != 0 || far != 0 {
        text.push(" ec=");
        text.hex(ec);
        text.push(" elr=");
        text.hex(elr);
        text.push(" far=");
        text.hex(far);
    }
    let line = text.as_str();
    if crate::task::log_live() {
        match level {
            3 => log::error!("{line}"),
            2 => log::warn!("{line}"),
            _ => log::info!("{line}"),
        }
    } else {
        crate::logger::announce(line);
    }
}

struct Text {
    data: [u8; 800],
    len: usize,
}

impl Text {
    const fn new() -> Self {
        Self {
            data: [0; 800],
            len: 0,
        }
    }

    fn push(&mut self, text: &str) {
        for byte in text.bytes() {
            if self.len < self.data.len() {
                self.data[self.len] = byte;
                self.len += 1;
            }
        }
    }

    fn dec(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut n = 0;
        if value == 0 {
            self.push("0");
            return;
        }
        while value > 0 && n < digits.len() {
            digits[n] = b'0' + (value % 10) as u8;
            value /= 10;
            n += 1;
        }
        while n > 0 {
            n -= 1;
            if self.len < self.data.len() {
                self.data[self.len] = digits[n];
                self.len += 1;
            }
        }
    }

    fn hex(&mut self, mut value: u64) {
        self.push("0x");
        if value == 0 {
            self.push("0");
            return;
        }
        let mut digits = [0u8; 16];
        let mut n = 0;
        while value > 0 && n < digits.len() {
            let nybble = (value & 0xf) as u8;
            digits[n] = if nybble < 10 {
                b'0' + nybble
            } else {
                b'a' + (nybble - 10)
            };
            value >>= 4;
            n += 1;
        }
        while n > 0 {
            n -= 1;
            if self.len < self.data.len() {
                self.data[self.len] = digits[n];
                self.len += 1;
            }
        }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.data[..self.len]).unwrap_or("")
    }
}
