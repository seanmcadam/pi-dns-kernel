//! Pi 4 green ACT LED on GPIO 42, active high.
//!
//! The counted boot flashes and the idle heartbeat are gone. HDMI is the
//! boot indicator. A fault lights the pin once and leaves it on. Virt and
//! Pi 5 do not call these functions.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const GPIO_BASE: u64 = 0xFE20_0000;
const GPFSEL4: u64 = GPIO_BASE + 0x10;
const GPSET1: u64 = GPIO_BASE + 0x20;
const GPCLR1: u64 = GPIO_BASE + 0x2C;
const ACT_SHIFT: u32 = 6;
const ACT_BIT: u32 = 1 << 10;

/// How long a core may take timer IRQs without reaching `note_core`.
/// The idle loop and a preemption both note well inside this window.
const STALL_MS: u64 = 4_000;
/// IRQs have stopped as well. Longer than `STALL_MS` so a core the
/// scheduler has not run yet is not reported as soon as the other cores.
const SILENT_MS: u64 = 15_000;
/// Heartbeat row period. The timer ticks at 100 Hz; the row does not.
const HB_MS: u64 = 500;

static FAULT: AtomicBool = AtomicBool::new(false);
static WATCH: AtomicBool = AtomicBool::new(false);
static LOOP: AtomicBool = AtomicBool::new(false);
static LAST_PAINT: AtomicU64 = AtomicU64::new(0);

static PROG_MS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static PROG_PC: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static TICK_MS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static TICK_PC: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The idle loop calls this every pass. A core that stops calling it, and
/// is not sitting in `wfi`, is the hang the timer is watching for.
#[inline(always)]
pub fn note_progress(_now: u64) {
    note_core(0);
}

/// Record that `cpu` reached a kernel checkpoint. The link register is the
/// caller. A later stall prints this when the timer IRQ itself has stopped.
#[inline(never)]
pub fn note_core(cpu: usize) {
    if cpu >= 4 {
        return;
    }
    let pc: u64;
    unsafe {
        core::arch::asm!(
            "mov {pc}, lr",
            pc = out(reg) pc,
            options(nomem, nostack, preserves_flags),
        );
    }
    PROG_PC[cpu].store(pc, Ordering::Relaxed);
    PROG_MS[cpu].store(crate::timer::millis(), Ordering::Release);
}

/// Timer IRQ on this core. `elr` is the PC the interrupt preempted.
pub fn note_tick(cpu: usize, elr: u64) {
    if cpu >= 4 {
        return;
    }
    TICK_PC[cpu].store(elr, Ordering::Relaxed);
    TICK_MS[cpu].store(crate::timer::millis(), Ordering::Release);
}

/// HDMI may refresh this core's checkpoint while drawing the status banner.
/// Boot logs before the idle loop must not, or a long EL0 test looks stalled.
pub fn looping() -> bool {
    LOOP.load(Ordering::Relaxed)
}

pub fn enter_loop() {
    LOOP.store(true, Ordering::Release);
    note_core(0);
}

/// Short label for the HDMI status line.
pub fn led_label() -> &'static str {
    if FAULT.load(Ordering::Relaxed) {
        "fault"
    } else {
        "off"
    }
}

/// Start the hang check. Call once the virtual timer is armed.
/// Does not mark core 0 as running: boot after this point is still
/// allowed to spend a long time in the EL0 tests.
pub fn arm_watch() {
    WATCH.store(true, Ordering::Relaxed);
}

/// Timer tick on every core. A core whose checkpoint is older than
/// `STALL_MS`, and that is not idle in `wfi`, freezes the panel with
/// its id and PC. A healthy core 0 also refreshes the heartbeat row.
pub fn check_hang() {
    if FAULT.load(Ordering::Relaxed) {
        park();
    }
    if !WATCH.load(Ordering::Relaxed) {
        return;
    }
    let me = crate::percpu::id();
    if me >= 4 {
        return;
    }
    let now = crate::timer::millis();
    let mut ages = [0u64; 4];
    let mut asleep = [false; 4];
    let mut started = [false; 4];
    let mut stall: Option<usize> = None;
    for cpu in 0..4 {
        let mark = PROG_MS[cpu].load(Ordering::Acquire);
        started[cpu] = mark != 0;
        asleep[cpu] = crate::percpu::asleep_on(cpu);
        ages[cpu] = if mark == 0 {
            0
        } else {
            now.saturating_sub(mark)
        };
        if !started[cpu] || asleep[cpu] || stall.is_some() {
            continue;
        }
        let tick_ms = TICK_MS[cpu].load(Ordering::Acquire);
        let tick_age = if tick_ms == 0 {
            u64::MAX
        } else {
            now.saturating_sub(tick_ms)
        };
        // This check runs on whichever core took the timer, including one
        // that is not the stuck core. After the idle loop, a core that has
        // already taken a tick and then stops checkpointing is reported in
        // two seconds even when its own IRQs are masked. A core that has
        // never been scheduled keeps the long window. Before the idle loop
        // the boot tests are allowed the old 4 s / 15 s split.
        let limit = if tick_ms == 0 {
            SILENT_MS
        } else if looping() {
            2_000
        } else if tick_age < 1_000 {
            STALL_MS
        } else {
            SILENT_MS
        };
        if ages[cpu] > limit {
            stall = Some(cpu);
        }
    }
    if let Some(cpu) = stall {
        report_stall(cpu, now);
    }
    if me == 0 {
        paint_heartbeat(now, &ages, &asleep, &started);
    }
}

fn report_stall(cpu: usize, now: u64) -> ! {
    if FAULT.swap(true, Ordering::AcqRel) {
        park();
    }
    crate::interrupts::mask();
    let tick_ms = TICK_MS[cpu].load(Ordering::Acquire);
    let tick_age = if tick_ms == 0 {
        u64::MAX
    } else {
        now.saturating_sub(tick_ms)
    };
    let irqs_on = tick_age < 1_000;
    let pc = if irqs_on {
        TICK_PC[cpu].load(Ordering::Relaxed)
    } else {
        PROG_PC[cpu].load(Ordering::Relaxed)
    };
    let mark = PROG_PC[cpu].load(Ordering::Relaxed);
    let mut buf = [0u8; 192];
    let mut n = 0;
    push(&mut buf, &mut n, "STALL c");
    push_dec(&mut buf, &mut n, cpu as u64);
    push(&mut buf, &mut n, " pc=");
    push_hex(&mut buf, &mut n, pc);
    push(&mut buf, &mut n, " ");
    push_symbol(&mut buf, &mut n, pc);
    push(
        &mut buf,
        &mut n,
        if irqs_on { " irqs=on\n" } else { " irqs=off\n" },
    );
    let (prog, total) = crate::fw::progress_pair();
    push(&mut buf, &mut n, "fw=");
    push(&mut buf, &mut n, crate::fw::phase_word());
    push(&mut buf, &mut n, " ");
    push_dec(&mut buf, &mut n, prog as u64);
    push(&mut buf, &mut n, "/");
    push_dec(&mut buf, &mut n, total as u64);
    push(&mut buf, &mut n, "\n");
    push(&mut buf, &mut n, "mark=");
    push_hex(&mut buf, &mut n, mark);
    push(&mut buf, &mut n, "\n");
    for other in 0..4 {
        if other > 0 {
            push(&mut buf, &mut n, " ");
        }
        push_dec(&mut buf, &mut n, other as u64);
        push(&mut buf, &mut n, ":");
        if crate::percpu::asleep_on(other) {
            push(&mut buf, &mut n, "z");
        } else if PROG_MS[other].load(Ordering::Relaxed) == 0 {
            push(&mut buf, &mut n, "-");
        } else {
            push_dec(
                &mut buf,
                &mut n,
                now.saturating_sub(PROG_MS[other].load(Ordering::Relaxed)),
            );
        }
    }
    let text = core::str::from_utf8(&buf[..n]).unwrap_or("STALL");
    // Straight to the UART data register and the framebuffer. No log queue
    // and no console lock: the core that is stuck may be holding either.
    raw_uart(text.as_bytes());
    raw_uart(b"\r\n");
    crate::fb_con::freeze(text);
    gpio_output();
    gpio_set(true);
    park();
}

fn paint_heartbeat(now: u64, ages: &[u64; 4], asleep: &[bool; 4], started: &[bool; 4]) {
    let last = LAST_PAINT.load(Ordering::Relaxed);
    if now.saturating_sub(last) < HB_MS {
        return;
    }
    LAST_PAINT.store(now, Ordering::Relaxed);
    let mut buf = [0u8; 80];
    let mut n = 0;
    for cpu in 0..4 {
        if cpu > 0 {
            push(&mut buf, &mut n, " ");
        }
        push_dec(&mut buf, &mut n, cpu as u64);
        push(&mut buf, &mut n, ":");
        if asleep[cpu] {
            push(&mut buf, &mut n, "z");
        } else if !started[cpu] {
            push(&mut buf, &mut n, "-");
        } else {
            push_dec(&mut buf, &mut n, ages[cpu].min(99999));
        }
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        crate::fb_con::paint_pinned_row(3, text);
    }
}

fn push_symbol(buf: &mut [u8], n: &mut usize, pc: u64) {
    let mut best_at = 0u64;
    let mut best = "unknown";
    let mut consider = |addr: u64, name: &'static str| {
        if addr != 0 && addr <= pc && addr >= best_at {
            best_at = addr;
            best = name;
        }
    };
    consider(
        (crate::smp::secondary_main as extern "C" fn(u64) -> !) as usize as u64,
        "secondary_main",
    );
    consider(
        (crate::task::schedule_one as fn() -> bool) as usize as u64,
        "schedule_one",
    );
    consider(
        (crate::net::service::owner_idle as fn()) as usize as u64,
        "owner_idle",
    );
    consider(
        (crate::task::el0_dispatch as extern "C" fn(u64) -> u64) as usize as u64,
        "el0_dispatch",
    );
    consider(
        (crate::fw::syscall as fn([u64; 6]) -> i64) as usize as u64,
        "fw_syscall",
    );
    consider(
        (crate::fs::digest as fn(&str) -> Result<fwupd::Digest, crate::fs::FsError>) as usize
            as u64,
        "fs_digest",
    );
    push(buf, n, best);
    let delta = pc.wrapping_sub(best_at);
    if best_at != 0 && delta < 0x10000 {
        push(buf, n, "+");
        push_hex_short(buf, n, delta);
    }
}

fn push_hex_short(buf: &mut [u8], n: &mut usize, value: u64) {
    if value == 0 {
        push(buf, n, "0");
        return;
    }
    let mut tmp = [0u8; 16];
    let mut i = tmp.len();
    let mut rest = value;
    while rest > 0 && i > 0 {
        i -= 1;
        let nibble = (rest & 0xf) as u8;
        tmp[i] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        rest >>= 4;
    }
    push(buf, n, unsafe { core::str::from_utf8_unchecked(&tmp[i..]) });
}

fn push(buf: &mut [u8], n: &mut usize, text: &str) {
    for byte in text.bytes() {
        if *n >= buf.len() {
            return;
        }
        buf[*n] = byte;
        *n += 1;
    }
}

fn push_dec(buf: &mut [u8], n: &mut usize, mut value: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if value == 0 {
        push(buf, n, "0");
        return;
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    push(buf, n, unsafe { core::str::from_utf8_unchecked(&tmp[i..]) });
}

fn push_hex(buf: &mut [u8], n: &mut usize, value: u64) {
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as u8;
        if *n >= buf.len() {
            return;
        }
        buf[*n] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        *n += 1;
    }
}

/// UART data register, one byte at a time. Does not take the console lock
/// or the log queue.
fn raw_uart(bytes: &[u8]) {
    let uart = crate::addr::phys_to_virt(crate::board::UART_PHYS) as *mut u32;
    for &byte in bytes {
        unsafe {
            write_volatile(uart, byte as u32);
        }
    }
}

fn park() -> ! {
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack));
        }
    }
}

/// One-shot fault indication. Does not return.
pub fn halt_blink() -> ! {
    hold("LOCKUP halt");
}

/// Paint `text` on the pinned HDMI rows and stop. Does not reboot.
/// Newlines separate the three rows. The scrolling log stops.
pub fn halt_with(text: &str) -> ! {
    hold(text);
}

fn hold(msg: &str) -> ! {
    FAULT.store(true, Ordering::Relaxed);
    crate::interrupts::mask();
    crate::fb_con::freeze(msg);
    gpio_output();
    gpio_set(true);
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack));
        }
    }
}

fn gpio_output() {
    let sel = crate::addr::phys_to_virt(GPFSEL4) as *mut u32;
    unsafe {
        let mut value = read_volatile(sel);
        value &= !(7 << ACT_SHIFT);
        value |= 1 << ACT_SHIFT;
        write_volatile(sel, value);
    }
}

fn gpio_set(on: bool) {
    let reg = if on { GPSET1 } else { GPCLR1 };
    unsafe {
        write_volatile(crate::addr::phys_to_virt(reg) as *mut u32, ACT_BIT);
    }
}
