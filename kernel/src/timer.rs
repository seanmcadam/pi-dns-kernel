use tock_registers::{
    interfaces::Readable,
    registers::ReadWrite,
};

// For QEMU virt machine, use generic timer (cntpct_el0)
// This is a CPU register, not MMIO
const TIMER_BASE: usize = 0xFE00_3000; // Only used for Pi 4 hardware

#[repr(C)]
struct TimerRegisters {
    cs: ReadWrite<u32>,
    clo: ReadWrite<u32>,
    chi: ReadWrite<u32>,
    c0: ReadWrite<u32>,
    c1: ReadWrite<u32>,
    c2: ReadWrite<u32>,
    c3: ReadWrite<u32>,
}

static mut TIMER: Option<&'static TimerRegisters> = None;

pub fn init() {
    // For virt machine, we use the generic timer which doesn't need initialization
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

pub fn delay_us(us: u64) {
    // Get timer frequency
    let freq = get_timer_frequency();
    
    let start = get_ticks();
    let target = start + (us * freq) / 1_000_000;
    
    while get_ticks() < target {
        core::hint::spin_loop();
    }
}

pub fn delay_ms(ms: u64) {
    delay_us(ms * 1000);
}

fn get_timer_frequency() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    freq
}
