use core::sync::atomic::{AtomicU64, Ordering};

/// The interrupt is the ARM virtual timer, `CNTV_*` / `CNTVCT_EL0`, wired to
/// PPI 11 (INTID 27). EL1 owns that timer after `boot.s` sets `CNTVOFF_EL2`
/// to 0, so virtual time matches physical time. The physical timer stays
/// available for a later EL2 user. `get_ticks` still reads `CNTPCT_EL0`,
/// which the block driver uses as a deadline; with a zero offset it advances
/// at this same rate.
///
/// The compare register is moved forward by whole ticks, including ticks that
/// were missed while the host was busy, capped so a long stall cannot spin
/// inside the handler. The deadline is a counter value, not a spin count.
pub const TICK_HZ: u64 = 100;

static TICKS: AtomicU64 = AtomicU64::new(0);
static INTERVAL: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let freq = get_timer_frequency();
    log::info!("generic timer frequency {freq} Hz, virtual timer {TICK_HZ} Hz");
}

/// Program the first deadline and enable the virtual timer interrupt.
/// Called after the GIC will accept it, and before IRQs are unmasked.
pub fn arm() {
    let freq = get_timer_frequency();
    let interval = freq / TICK_HZ;
    if interval == 0 {
        log::warn!("generic timer frequency too low to arm");
        return;
    }
    INTERVAL.store(interval, Ordering::Relaxed);
    write_ctl(0);
    write_cval(virtual_count().wrapping_add(interval));
    // ENABLE=1, IMASK=0.
    write_ctl(1);
    unsafe {
        core::arch::asm!("isb", options(nomem, nostack, preserves_flags));
    }
}

/// One interrupt. Safe to call with IRQs masked. Does not allocate or take a lock.
pub fn on_tick() {
    let interval = INTERVAL.load(Ordering::Relaxed);
    if interval == 0 {
        return;
    }
    let now = virtual_count();
    let mut next = read_cval().wrapping_add(interval);
    let mut steps = 1u64;
    while next <= now && steps < 1000 {
        next = next.wrapping_add(interval);
        steps += 1;
    }
    if next <= now {
        next = now.wrapping_add(interval);
    }
    write_cval(next);
    crate::cpu_stat::on_timer();
    // One published 100 Hz counter. Every core has its own compare register
    // and uses it for preemption. Adding all four would report 400 Hz.
    if crate::percpu::id() == 0 {
        TICKS.fetch_add(steps, Ordering::Relaxed);
    }
}

pub fn irq_ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

pub fn uptime_secs() -> u64 {
    let freq = get_timer_frequency();
    if freq == 0 {
        return 0;
    }
    virtual_count() / freq
}

fn virtual_count() -> u64 {
    let ticks: u64;
    unsafe {
        core::arch::asm!(
            "mrs {ticks}, cntvct_el0",
            ticks = out(reg) ticks,
            options(nomem, nostack)
        );
    }
    ticks
}

fn read_cval() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!(
            "mrs {value}, cntv_cval_el0",
            value = out(reg) value,
            options(nomem, nostack)
        );
    }
    value
}

fn write_cval(value: u64) {
    unsafe {
        core::arch::asm!(
            "msr cntv_cval_el0, {value}",
            value = in(reg) value,
            options(nomem, nostack)
        );
    }
}

fn write_ctl(value: u64) {
    unsafe {
        core::arch::asm!(
            "msr cntv_ctl_el0, {value}",
            value = in(reg) value,
            options(nomem, nostack)
        );
    }
}

pub fn millis() -> u64 {
    let freq = get_timer_frequency();
    if freq == 0 {
        return 0;
    }
    get_ticks() * 1000 / freq
}

pub fn get_ticks() -> u64 {
    // Read generic timer counter (available on all ARMv8 systems)
    // This returns a 64-bit monotonic counter
    let ticks: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntpct_el0", out(reg) ticks);
    }
    ticks
}

pub fn frequency() -> u64 {
    get_timer_frequency()
}

fn get_timer_frequency() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    freq
}
