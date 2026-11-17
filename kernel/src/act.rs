//! Pi 4 green ACT LED, plus the firmware GPIO expander.
//!
//! Current Pi 4 device trees (`bcm2711-rpi-4-b.dts`) wire the green activity
//! LED to GPIO 42, active high (`STATUS_LED_G_CLK`). The firmware expander
//! pin 2 is mailbox GPIO 130 (128+2) and is the red power LED on that tree
//! (`PWR_LED_OFF`, active low). Older write-ups call pin 130 the activity
//! LED. Both are driven with the same pulse count. GPIO 42 is the one that
//! lights the green LED on current boards. The mailbox path is best-effort
//! and is abandoned after one timed-out or refused call, so a missing
//! expander cannot stall boot.
//!
//! Counted pulses use the architectural counter. A counter that does not
//! advance hits a spin budget and returns. The panic pattern does not touch
//! the mailbox.

use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};

const GPIO_BASE: u64 = 0xFE20_0000;
const GPFSEL4: u64 = GPIO_BASE + 0x10;
const GPSET1: u64 = GPIO_BASE + 0x20;
const GPCLR1: u64 = GPIO_BASE + 0x2C;
const ACT_SHIFT: u32 = 6;
const ACT_BIT: u32 = 1 << 10;

const MBOX_BASE: u64 = 0xFE00_B880;
const MBOX_STATUS: u64 = 0x18;
const MBOX_WRITE: u64 = 0x20;
const MBOX_FULL: u32 = 0x8000_0000;
const MBOX_EMPTY: u32 = 0x4000_0000;
const REQ_OK: u32 = 0x8000_0000;
const TAG_SET_STATE: u32 = 0x0003_8041;
const TAG_GET_CONFIG: u32 = 0x0003_0043;
const TAG_SET_CONFIG: u32 = 0x0003_8043;
const EXP_GPIO: u32 = 130;

const ON_MS: u64 = 100;
const OFF_MS: u64 = 100;
const GAP_MS: u64 = 300;
const FAST_MS: u64 = 40;

#[repr(C, align(16))]
struct Msg {
    words: [u32; 16],
}

#[link_section = ".dma"]
static mut LED_MSG: Msg = Msg { words: [0; 16] };

static MBOX_DEAD: AtomicBool = AtomicBool::new(false);
static MBOX_READY: AtomicBool = AtomicBool::new(false);
/// The expander shares the property mailbox with the framebuffer. It stays
/// quiet until the framebuffer transaction has finished.
static EXP_ARM: AtomicBool = AtomicBool::new(false);
static EXP_ON: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Allow SET_GPIO_STATE on expander pin 130. Call only after the
/// framebuffer mailbox exchange has finished.
pub fn arm_expander() {
    EXP_ARM.store(true, Ordering::Relaxed);
}

/// `count` short flashes, then a gap. 1 is Rust, 2 is the strict map,
/// 3 is the framebuffer, 4 is user tasks. The framebuffer is brought up
/// before the strict map, so a healthy boot shows 1, then 3, then 2, then 4.
pub fn pattern(count: u32) {
    gpio_output();
    let mut n = 0;
    while n < count {
        set_both(true);
        wait_ms(ON_MS);
        set_both(false);
        wait_ms(OFF_MS);
        n += 1;
    }
    if count > 0 {
        wait_ms(GAP_MS);
    }
}

/// Fast continuous blink. Does not return. Mailbox is not used.
pub fn halt_blink() -> ! {
    gpio_output();
    gpio_set(false);
    loop {
        gpio_set(true);
        wait_ms(FAST_MS);
        gpio_set(false);
        wait_ms(FAST_MS);
    }
}

fn set_both(on: bool) {
    gpio_set(on);
    exp_set(on);
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

fn exp_set(on: bool) {
    if !EXP_ARM.load(Ordering::Relaxed) || MBOX_DEAD.load(Ordering::Relaxed) {
        return;
    }
    if !MBOX_READY.load(Ordering::Relaxed) && !exp_config() {
        MBOX_DEAD.store(true, Ordering::Relaxed);
        return;
    }
    let lit = EXP_ON.load(Ordering::Relaxed);
    let state = if on { lit } else { lit ^ 1 };
    if !mbox_tag(TAG_SET_STATE, &[EXP_GPIO, state]) {
        MBOX_DEAD.store(true, Ordering::Relaxed);
    }
}

fn exp_config() -> bool {
    let mut got = [0u32; 5];
    let polarity = if mbox_tag_inout(TAG_GET_CONFIG, &[EXP_GPIO, 0, 0, 0, 0], &mut got) {
        got[2]
    } else {
        1
    };
    // direction 1 is output. state 0 is the level Linux writes. Pin 130 is
    // active low on the current tree, so the lit level is 0 unless the
    // firmware's own polarity says otherwise. A refused config still lets
    // SET_GPIO_STATE try once.
    let lit = if polarity == 0 { 1 } else { 0 };
    EXP_ON.store(lit, Ordering::Relaxed);
    let configured = mbox_tag(TAG_SET_CONFIG, &[EXP_GPIO, 1, polarity, 0, 0, lit]);
    if configured || mbox_tag(TAG_SET_STATE, &[EXP_GPIO, lit]) {
        MBOX_READY.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

fn mbox_tag(tag: u32, values: &[u32]) -> bool {
    let mut out = [0u32; 8];
    mbox_tag_inout(tag, values, &mut out)
}

fn mbox_tag_inout(tag: u32, values: &[u32], out: &mut [u32]) -> bool {
    if values.len() > 8 || out.len() < values.len() {
        return false;
    }
    let start = crate::timer::get_ticks();
    unsafe {
        let msg = addr_of_mut!(LED_MSG);
        let dst = addr_of_mut!((*msg).words).cast::<u32>();
        let words = 6 + values.len();
        dst.write_volatile((words * 4) as u32);
        dst.add(1).write_volatile(0);
        dst.add(2).write_volatile(tag);
        dst.add(3).write_volatile((values.len() * 4) as u32);
        dst.add(4).write_volatile(0);
        let mut i = 0;
        while i < values.len() {
            dst.add(5 + i).write_volatile(values[i]);
            i += 1;
        }
        dst.add(5 + values.len()).write_volatile(0);
        clean(msg as *const u8, words * 4);
        let phys = crate::addr::virt_to_phys(msg as u64) as u32;
        if !mbox_send((phys & !0xF) | 8, start) {
            return false;
        }
        clean(msg as *const u8, words * 4);
        if dst.add(1).read_volatile() != REQ_OK {
            return false;
        }
        let mut n = 0;
        while n < values.len() && n < out.len() {
            out[n] = dst.add(5 + n).read_volatile();
            n += 1;
        }
    }
    true
}

fn mbox_send(word: u32, start: u64) -> bool {
    let base = crate::addr::phys_to_virt(MBOX_BASE);
    let mut spins = 0u32;
    loop {
        spins = spins.saturating_add(1);
        if spins > 200_000 || led_timeout(start) {
            return false;
        }
        let Some(status) = crate::exception::read_u32_catching((base + MBOX_STATUS) as *const u32)
        else {
            return false;
        };
        if status & MBOX_FULL == 0 {
            break;
        }
    }
    unsafe {
        write_volatile((base + MBOX_WRITE) as *mut u32, word);
    }
    loop {
        spins = spins.saturating_add(1);
        if spins > 400_000 || led_timeout(start) {
            return false;
        }
        let Some(status) = crate::exception::read_u32_catching((base + MBOX_STATUS) as *const u32)
        else {
            return false;
        };
        if status & MBOX_EMPTY != 0 {
            continue;
        }
        let Some(value) = crate::exception::read_u32_catching(base as *const u32) else {
            return false;
        };
        if value & 0xF == 8 {
            return true;
        }
    }
}

fn led_timeout(start: u64) -> bool {
    let freq = crate::timer::frequency();
    if freq == 0 {
        return true;
    }
    crate::timer::get_ticks().wrapping_sub(start) > freq / 20
}

fn clean(ptr: *const u8, len: usize) {
    let start = (ptr as usize) & !63;
    let end = (ptr as usize).saturating_add(len);
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc civac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}

fn wait_ms(ms: u64) {
    let freq = crate::timer::frequency();
    if freq == 0 || ms == 0 {
        return;
    }
    let ticks = freq / 1000 * ms;
    let start = crate::timer::get_ticks();
    let mut spins = 0u32;
    while crate::timer::get_ticks().wrapping_sub(start) < ticks {
        spins = spins.saturating_add(1);
        // A stuck counter must not park the core. The budget is large
        // enough for a short blink when the counter is moving.
        if spins > 8_000_000 {
            return;
        }
        core::hint::spin_loop();
    }
}
