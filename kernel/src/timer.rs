use tock_registers::{
    interfaces::{Readable, Writeable},
    registers::ReadWrite,
};

const TIMER_BASE: usize = 0xFE00_3000;

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
    unsafe {
        TIMER = Some(&*(TIMER_BASE as *const TimerRegisters));
    }
}

pub fn get_ticks() -> u64 {
    unsafe {
        if let Some(timer) = TIMER {
            let lo = timer.clo.get() as u64;
            let hi = timer.chi.get() as u64;
            (hi << 32) | lo
        } else {
            0
        }
    }
}

pub fn delay_us(us: u64) {
    let start = get_ticks();
    let target = start + us;

    while get_ticks() < target {
        cortex_a::asm::nop();
    }
}

pub fn delay_ms(ms: u64) {
    delay_us(ms * 1000);
}
