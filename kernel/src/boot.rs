//! Early watchdog, boot-attempt counter, and backup fallback.
//!
//! The watchdog is armed before the first mailbox or SD call. The main loop
//! pets it. A stage that never returns is reset by that watchdog. The attempt
//! counter sits in the crash page, after the crash record, so a reset does
//! not need the card. The card is touched only after the filesystem mount
//! has returned, and only to rewrite `TRYBOOT.TXT` in place.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::boot_rec::{self, Action, Rec};

extern "C" {
    static mut __crash_start: u8;
}

static ACTION: AtomicU32 = AtomicU32::new(0);
static ARMED: AtomicBool = AtomicBool::new(false);
static HELD: AtomicBool = AtomicBool::new(false);
static STOPPED: AtomicBool = AtomicBool::new(false);
static LAST_PET: AtomicU64 = AtomicU64::new(0);
static SERVE_AT: AtomicU64 = AtomicU64::new(0);
static GOOD: AtomicBool = AtomicBool::new(false);
static RESTORE: AtomicBool = AtomicBool::new(false);
static KIND: AtomicU8 = AtomicU8::new(0);
static AT: AtomicU64 = AtomicU64::new(0);

const ACT_CONTINUE: u32 = 0;
const ACT_FALLBACK: u32 = 1;
const ACT_STAY: u32 = 2;

fn boot_slot() -> &'static mut [u8] {
    unsafe {
        let start = core::ptr::addr_of_mut!(__crash_start).add(crate::crash_rec::SLOT);
        core::slice::from_raw_parts_mut(start, boot_rec::LEN)
    }
}

fn remember(rec: &Rec) {
    let raw = boot_slot();
    let _ = boot_rec::store(raw, rec);
    clean(raw.as_ptr() as u64, boot_rec::LEN);
}

fn saved() -> Option<Rec> {
    boot_rec::load(boot_slot())
}

/// Arm the watchdog and count this boot. Called before any other init.
pub fn open() {
    crate::board::watchdog_arm();
    ARMED.store(true, Ordering::Release);
    LAST_PET.store(crate::timer::millis(), Ordering::Release);
    let (rec, action) = boot_rec::on_boot(saved());
    ACTION.store(
        match action {
            Action::Continue => ACT_CONTINUE,
            Action::Fallback => ACT_FALLBACK,
            Action::Stay => ACT_STAY,
        },
        Ordering::Release,
    );
    remember(&rec);
}

pub fn stage(stage: u32) {
    pet();
    if let Some(mut rec) = saved() {
        rec.stage = stage;
        remember(&rec);
    }
}

/// Log the previous boot once the console lock exists.
pub fn report() {
    let Some(rec) = saved() else {
        return;
    };
    let mut line = [0u8; 64];
    let mut n = push(&mut line, 0, b"boot attempts ");
    n = push_dec(&mut line, n, rec.attempts as u64);
    n = push(&mut line, n, b" stopped ");
    n = push(&mut line, n, boot_rec::stage_name(rec.prev_stage).as_bytes());
    if rec.fallback == boot_rec::FB_ACTIVE {
        n = push(&mut line, n, b" fallback");
    }
    if let Ok(text) = core::str::from_utf8(&line[..n]) {
        crate::disklog::line(text);
    }
}

/// After the mount returns. A fallback rewrite happens here, not before.
pub fn after_fs(mounted: bool) {
    stage(boot_rec::STAGE_FS);
    if ACTION.load(Ordering::Acquire) != ACT_FALLBACK {
        return;
    }
    #[cfg(feature = "board-pi5")]
    {
        let _ = mounted;
        abandon("boot fallback tryboot unsupported");
    }
    #[cfg(not(feature = "board-pi5"))]
    {
        if !mounted {
            abandon("boot fallback no card");
            return;
        }
        if !backup_ready() {
            abandon("boot fallback no backup");
            return;
        }
        if !write_fallback_tryboot() {
            abandon("boot fallback file failed");
            return;
        }
        if let Some(mut rec) = saved() {
            rec.fallback = boot_rec::FB_RESET;
            remember(&rec);
        }
        crate::disklog::line("boot fallback kernel8.bak");
        #[cfg(feature = "board-pi4")]
        {
            let refused = crate::board::power_tryboot();
            if refused {
                let _ = crate::fw::install_live_tryboot();
                abandon("boot fallback refused");
            }
        }
        #[cfg(feature = "board-virt")]
        {
            crate::disklog::line("boot fallback tryboot unsupported");
            let _ = crate::board::power(true);
        }
    }
}

pub fn serving() {
    stage(boot_rec::STAGE_SERVE);
    if SERVE_AT.load(Ordering::Acquire) == 0 {
        SERVE_AT.store(crate::timer::millis(), Ordering::Release);
    }
}

pub fn poll() {
    let kind = KIND.load(Ordering::Acquire);
    let at = AT.load(Ordering::Acquire);
    if kind != 0 && at != 0 && crate::timer::millis() >= at && KIND.swap(0, Ordering::AcqRel) != 0 {
        if kind == 1 {
            hold_hang();
        } else {
            force_fail();
        }
    }
    pet();
    if GOOD.load(Ordering::Acquire) || SERVE_AT.load(Ordering::Acquire) == 0 {
        return;
    }
    let started = SERVE_AT.load(Ordering::Acquire);
    if crate::timer::millis().saturating_sub(started) < boot_rec::GOOD_MS {
        return;
    }
    mark_boot_good();
}

/// HTTP task. Kind 1 hangs without resetting. Kind 2 marks a failed boot.
pub fn arm(kind: u8) -> i64 {
    if crate::percpu::slot() != crate::task::HTTP_SLOT {
        return -3;
    }
    if kind != 1 && kind != 2 {
        return -4;
    }
    if KIND.swap(kind, Ordering::AcqRel) != 0 {
        return 1;
    }
    AT.store(
        crate::timer::millis().saturating_add(300),
        Ordering::Release,
    );
    0
}

pub fn status(out: &mut [u8]) -> usize {
    if out.len() < 80 {
        return 0;
    }
    out[..80].fill(0);
    let rec = saved().unwrap_or(Rec {
        attempts: 0,
        stage: 0,
        prev_stage: 0,
        fallback: 0,
        good: 0,
        restored: 0,
    });
    let good = u32::from(rec.good != 0 || GOOD.load(Ordering::Acquire));
    out[0..4].copy_from_slice(&rec.attempts.to_le_bytes());
    out[4..8].copy_from_slice(&good.to_le_bytes());
    let active = u32::from(rec.fallback != 0);
    out[8..12].copy_from_slice(&active.to_le_bytes());
    out[12..16].copy_from_slice(&rec.stage.to_le_bytes());
    out[16..20].copy_from_slice(&rec.prev_stage.to_le_bytes());
    put_text(&mut out[20..36], kernel_name(rec.fallback));
    put_text(&mut out[36..52], boot_rec::stage_name(rec.prev_stage));
    put_text(&mut out[52..68], boot_rec::stage_name(rec.stage));
    80
}

pub fn disarm() {
    STOPPED.store(true, Ordering::Release);
    ARMED.store(false, Ordering::Release);
    crate::board::watchdog_disarm();
}

pub fn check_deadline() {
    if !ARMED.load(Ordering::Acquire) || STOPPED.load(Ordering::Acquire) {
        return;
    }
    let last = LAST_PET.load(Ordering::Acquire);
    if last == 0 {
        return;
    }
    if crate::timer::millis().saturating_sub(last) < boot_rec::WATCHDOG_MS {
        return;
    }
    if ARMED
        .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    raw_line(b"watchdog reset\r\n");
    let _ = crate::board::crash_reset();
}

pub fn fallback_keeps_backup() -> bool {
    saved().is_some_and(|rec| {
        rec.fallback == boot_rec::FB_RESET || rec.fallback == boot_rec::FB_ACTIVE
    })
}

pub fn take_restore_tryboot() -> bool {
    RESTORE.swap(false, Ordering::AcqRel)
}

pub fn note_restored() {
    if let Some(mut rec) = saved() {
        rec.restored = 1;
        rec.fallback = boot_rec::FB_NONE;
        rec.good = 1;
        rec.stage = boot_rec::STAGE_GOOD;
        remember(&rec);
    }
    crate::disklog::line("boot fallback restored");
}

fn mark_boot_good() {
    if GOOD.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(mut rec) = saved() else {
        return;
    };
    let fallback = rec.fallback;
    rec = boot_rec::mark_good(rec);
    remember(&rec);
    crate::disklog::line("boot good");
    if fallback >= boot_rec::FB_ACTIVE && fallback != boot_rec::FB_ABANDONED {
        RESTORE.store(true, Ordering::Release);
        if crate::fw::boot_rollback() {
            crate::disklog::line("boot fallback rollback");
        } else {
            crate::disklog::line("boot fallback rollback failed");
        }
    }
}

fn force_fail() {
    let rec = saved()
        .map(boot_rec::simulate_fail)
        .unwrap_or_else(|| boot_rec::simulate_fail(Rec {
            attempts: 0,
            stage: boot_rec::STAGE_SERVE,
            prev_stage: 0,
            fallback: 0,
            good: 0,
            restored: 0,
        }));
    remember(&rec);
    crate::disklog::line("boot fail simulated");
    let _ = crate::board::power(true);
}

fn hold_hang() {
    HELD.store(true, Ordering::Release);
    raw_line(b"watchdog hold\r\n");
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack));
        }
    }
}

fn pet() {
    if HELD.load(Ordering::Acquire) || STOPPED.load(Ordering::Acquire) {
        return;
    }
    LAST_PET.store(crate::timer::millis(), Ordering::Release);
    crate::board::watchdog_pet();
}

fn abandon(text: &str) {
    ACTION.store(ACT_STAY, Ordering::Release);
    if let Some(mut rec) = saved() {
        rec.fallback = boot_rec::FB_ABANDONED;
        remember(&rec);
    }
    crate::disklog::line(text);
}

#[cfg(not(feature = "board-pi5"))]
fn backup_ready() -> bool {
    match crate::fs::file_len("KERNEL8.BAK") {
        Ok(len) => len > 0,
        Err(_) => false,
    }
}

#[cfg(not(feature = "board-pi5"))]
fn write_fallback_tryboot() -> bool {
    let kernel = "kernel8.bak";
    let address = 0x8_0000;
    let mut out = [0u8; 96];
    let Ok(n) = fwupd::fill_tryboot(&mut out, kernel, address, true) else {
        return false;
    };
    crate::fs::write_named("TRYBOOT.TXT", &out[..n], false).is_ok()
}

fn kernel_name(fallback: u32) -> &'static str {
    if fallback >= boot_rec::FB_ACTIVE
        && fallback != boot_rec::FB_ABANDONED
        && crate::board::tryboot_latched() & 1 != 0
    {
        if cfg!(feature = "board-pi5") {
            "k2712.bak"
        } else {
            "kernel8.bak"
        }
    } else {
        crate::fw::booted_name()
    }
}

fn put_text(out: &mut [u8], text: &str) {
    let n = text.len().min(out.len());
    out[..n].copy_from_slice(&text.as_bytes()[..n]);
}

fn clean(start: u64, len: usize) {
    let mut addr = start & !63;
    let end = start.wrapping_add(len as u64);
    while addr < end {
        unsafe {
            core::arch::asm!("dc cvac, {addr}", addr = in(reg) addr, options(nostack));
        }
        addr = addr.wrapping_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}

fn raw_line(bytes: &[u8]) {
    let uart = crate::addr::phys_to_virt(crate::board::UART_PHYS) as *mut u32;
    for &byte in bytes {
        unsafe {
            core::ptr::write_volatile(uart, byte as u32);
        }
    }
}

fn push(out: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    for byte in bytes {
        if n >= out.len() {
            return n;
        }
        out[n] = *byte;
        n += 1;
    }
    n
}

fn push_dec(out: &mut [u8], n: usize, mut value: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if value == 0 {
        i -= 1;
        tmp[i] = b'0';
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    push(out, n, &tmp[i..])
}
