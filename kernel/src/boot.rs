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
/// A crash that stays up must not clear the PM watchdog. The CPU is about
/// to park, and a disarmed watchdog would leave the board dark.
static HOLD_WATCHDOG: AtomicBool = AtomicBool::new(false);
static LAST_PET: AtomicU64 = AtomicU64::new(0);
static SERVE_AT: AtomicU64 = AtomicU64::new(0);
static GOOD: AtomicBool = AtomicBool::new(false);
static RESTORE: AtomicBool = AtomicBool::new(false);
static KIND: AtomicU8 = AtomicU8::new(0);
static AT: AtomicU64 = AtomicU64::new(0);
static CAUSE: AtomicU32 = AtomicU32::new(0);
static ROLLBACK: AtomicBool = AtomicBool::new(false);
static CLASSIFIED: AtomicBool = AtomicBool::new(false);
static SEEN_STAGE: AtomicU32 = AtomicU32::new(0);
static MOUNTED: AtomicBool = AtomicBool::new(false);

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

/// Arm the watchdog. The attempt counter is applied once the firmware
/// report says which file actually booted.
pub fn open() {
    let recorded = saved().map(|rec| rec.reset).unwrap_or(0);
    crate::board::watchdog_arm();
    CAUSE.store(
        boot_rec::classify_reset(crate::board::last_rsts(), recorded),
        Ordering::Release,
    );
    ARMED.store(true, Ordering::Release);
    LAST_PET.store(crate::timer::millis(), Ordering::Release);
}

pub fn stage(stage: u32) {
    pet();
    SEEN_STAGE.store(stage, Ordering::Release);
    if !CLASSIFIED.load(Ordering::Acquire) {
        return;
    }
    if let Some(mut rec) = saved() {
        rec.stage = stage;
        remember(&rec);
    }
}

/// Apply the counter now that the tryboot flag and `TRYBOOT.TXT` are known.
pub fn classify(tryboot: bool, kernel: &str) {
    let image = boot_rec::image_id(version::BOOT_ID_STR.as_bytes());
    let prev = saved();
    let arrival = arrival(tryboot, kernel, prev, image);
    let (mut rec, action) = boot_rec::reconcile(prev, arrival, image);
    let seen = SEEN_STAGE.load(Ordering::Acquire);
    if seen != 0 {
        rec.stage = seen;
    }
    let rollback = boot_rec::allows_rollback(&rec, arrival);
    ACTION.store(action_code(action), Ordering::Release);
    ROLLBACK.store(rollback, Ordering::Release);
    CLASSIFIED.store(true, Ordering::Release);
    remember(&rec);
    if arrival == boot_rec::Arrival::Staged || arrival == boot_rec::Arrival::Other {
        crate::disklog::line("boot record cleared");
    }
    if action == Action::Fallback {
        begin_fallback();
    }
}

/// A staged upload or a promote replaces the kernel this streak was counting.
pub fn forget() {
    let image = boot_rec::image_id(version::BOOT_ID_STR.as_bytes());
    let prev = saved();
    let stopped = prev.map(|rec| boot_rec::stopped_at(&rec)).unwrap_or(0);
    let was_good = prev.is_some_and(|rec| rec.good != 0 || rec.stage == boot_rec::STAGE_GOOD);
    let mut rec = boot_rec::reconcile(None, boot_rec::Arrival::Staged, image).0;
    rec.prev_stage = stopped;
    if was_good {
        rec.good = 1;
        rec.stage = boot_rec::STAGE_GOOD;
        rec.prev_stage = boot_rec::STAGE_GOOD;
    } else if stopped != 0 {
        rec.stage = stopped;
    }
    ACTION.store(ACT_CONTINUE, Ordering::Release);
    ROLLBACK.store(false, Ordering::Release);
    CLASSIFIED.store(true, Ordering::Release);
    remember(&rec);
    crate::disklog::line("boot record cleared");
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
    n = push(
        &mut line,
        n,
        boot_rec::stage_name(rec.prev_stage).as_bytes(),
    );
    if rec.fallback == boot_rec::FB_ACTIVE {
        n = push(&mut line, n, b" fallback");
    }
    if let Ok(text) = core::str::from_utf8(&line[..n]) {
        crate::disklog::line(text);
    }
    let cause = boot_rec::reset_name(CAUSE.load(Ordering::Acquire));
    let mut reset_line = [0u8; 32];
    let mut reset_n = push(&mut reset_line, 0, b"last reset ");
    reset_n = push(&mut reset_line, reset_n, cause.as_bytes());
    if let Ok(text) = core::str::from_utf8(&reset_line[..reset_n]) {
        crate::disklog::line(text);
    }
    let mut rsts_line = [0u8; 24];
    let mut rsts_n = push(&mut rsts_line, 0, b"pm rsts ");
    rsts_n = push_hex(&mut rsts_line, rsts_n, crate::board::last_rsts());
    if let Ok(text) = core::str::from_utf8(&rsts_line[..rsts_n]) {
        crate::disklog::line(text);
    }
}

/// The card is mounted. The fallback file is written from `classify`, after
/// the firmware report, so a tryboot of `kernel8.new` is not treated as the
/// backup.
pub fn after_fs(mounted: bool) {
    stage(boot_rec::STAGE_FS);
    MOUNTED.store(mounted, Ordering::Release);
}

fn begin_fallback() {
    let mounted = MOUNTED.load(Ordering::Acquire);
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
        image: [0; 16],
        reset: 0,
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
    let cause = CAUSE.load(Ordering::Acquire);
    out[68..72].copy_from_slice(&cause.to_le_bytes());
    80
}

/// Park is next, and this crash is not asking for an immediate reboot.
/// Leave the countdown running so the halt still resets the board.
pub fn hold_watchdog() {
    HOLD_WATCHDOG.store(true, Ordering::Release);
}

pub fn disarm() {
    STOPPED.store(true, Ordering::Release);
    if HOLD_WATCHDOG.load(Ordering::Acquire) {
        return;
    }
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
    let extra = if HELD.load(Ordering::Acquire) && crate::board::pm_watchdog_live() {
        3_000
    } else {
        0
    };
    if crate::timer::millis().saturating_sub(last) < boot_rec::WATCHDOG_MS + extra {
        return;
    }
    if let Some(mut rec) = saved() {
        rec.reset = boot_rec::RESET_WATCHDOG;
        remember(&rec);
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
    if ROLLBACK.load(Ordering::Acquire)
        && fallback >= boot_rec::FB_ACTIVE
        && fallback != boot_rec::FB_ABANDONED
    {
        RESTORE.store(true, Ordering::Release);
        if crate::fw::boot_rollback() {
            crate::disklog::line("boot fallback rollback");
        } else {
            crate::disklog::line("boot fallback rollback failed");
        }
    }
}

fn force_fail() {
    let rec = saved().map(boot_rec::simulate_fail).unwrap_or_else(|| {
        boot_rec::simulate_fail(Rec {
            attempts: 0,
            stage: boot_rec::STAGE_SERVE,
            prev_stage: 0,
            fallback: 0,
            good: 0,
            restored: 0,
            image: [0; 16],
            reset: 0,
        })
    });
    remember(&rec);
    crate::disklog::line("boot fail simulated");
    let _ = crate::board::power(true);
}

fn hold_hang() {
    HELD.store(true, Ordering::Release);
    note_watchdog_hold();
    #[cfg(feature = "board-pi4")]
    crate::act::suspend_watch();
    crate::disklog::line("watchdog hold");
    raw_line(b"watchdog hold\r\n");
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack));
        }
    }
}

/// The PM watchdog does not pass through this function, and the firmware
/// clears `PM_RSTS` before the next kernel reads it. The flag in the boot
/// record is what the next boot reports.
fn note_watchdog_hold() {
    let image = boot_rec::image_id(version::BOOT_ID_STR.as_bytes());
    let mut rec =
        saved().unwrap_or_else(|| boot_rec::reconcile(None, boot_rec::Arrival::Same, image).0);
    rec.reset = boot_rec::RESET_WATCHDOG;
    remember(&rec);
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

pub fn classify_bytes(tryboot: bool, text: &[u8]) {
    classify(tryboot, kernel_token(text));
}

fn kernel_token(text: &[u8]) -> &str {
    let key = b"kernel=";
    let Some(at) = text.windows(key.len()).position(|window| window == key) else {
        return "";
    };
    let rest = &text[at + key.len()..];
    let end = rest
        .iter()
        .position(|byte| *byte == b'\n' || *byte == b'\r' || *byte == b' ')
        .unwrap_or(rest.len());
    core::str::from_utf8(&rest[..end]).unwrap_or("")
}

fn arrival(tryboot: bool, kernel: &str, prev: Option<Rec>, image: [u8; 16]) -> boot_rec::Arrival {
    if tryboot && kernel == "kernel8.bak" {
        return boot_rec::Arrival::Backup;
    }
    if tryboot {
        return boot_rec::Arrival::Staged;
    }
    if prev.is_some_and(|rec| rec.image != [0; 16] && rec.image != image) {
        return boot_rec::Arrival::Other;
    }
    boot_rec::Arrival::Same
}

fn action_code(action: Action) -> u32 {
    match action {
        Action::Continue => ACT_CONTINUE,
        Action::Fallback => ACT_FALLBACK,
        Action::Stay => ACT_STAY,
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

fn push(out: &mut [u8], n: usize, bytes: &[u8]) -> usize {
    minfmt::push_bytes(out, n, bytes)
}

fn push_hex(out: &mut [u8], mut n: usize, value: u32) -> usize {
    let mut tmp = [0u8; 8];
    let width = minfmt::push_hex(&mut tmp, 0, value as u64, 8);
    let mut i = 0;
    while i < width {
        if n + 2 > out.len() {
            return n;
        }
        out[n] = tmp[i];
        out[n + 1] = tmp[i + 1];
        n += 2;
        i += 2;
    }
    n
}

fn push_dec(out: &mut [u8], n: usize, value: u64) -> usize {
    minfmt::push_dec(out, n, value)
}
