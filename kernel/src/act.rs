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

/// Silent time after the timer is armed before a stuck core is shown.
const HANG_MS: u64 = 60_000;

static FAULT: AtomicBool = AtomicBool::new(false);
static PROGRESS: AtomicU64 = AtomicU64::new(0);
static WATCH: AtomicBool = AtomicBool::new(false);

/// The idle loop calls this every pass. A core that stops calling it is
/// the hang the timer is watching for.
pub fn note_progress(now: u64) {
    PROGRESS.store(now, Ordering::Relaxed);
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
pub fn arm_watch() {
    note_progress(crate::timer::millis());
    WATCH.store(true, Ordering::Relaxed);
}

/// Timer tick. A core that has not touched the indicator for `HANG_MS`
/// paints the HDMI row and does not return.
pub fn check_hang() {
    if FAULT.load(Ordering::Relaxed) || !WATCH.load(Ordering::Relaxed) {
        return;
    }
    let now = crate::timer::millis();
    if now.saturating_sub(PROGRESS.load(Ordering::Relaxed)) > HANG_MS {
        hold("LOCKUP idle");
    }
}

/// One-shot fault indication. Does not return.
pub fn halt_blink() -> ! {
    hold("LOCKUP halt");
}

fn hold(msg: &str) -> ! {
    FAULT.store(true, Ordering::Relaxed);
    crate::interrupts::mask();
    crate::fb_con::emergency(msg);
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
