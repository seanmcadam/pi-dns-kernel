//! Per-core timer ticks. The IRQ handler updates these and takes no lock.
//!
//! A tick that interrupted EL0 counts as user time for that core and for the
//! slot in `TPIDR`. A tick that interrupted EL1 while this core has marked
//! itself asleep counts as idle. Anything else is kernel time.

use core::sync::atomic::{AtomicU64, Ordering};

pub const CPUS: usize = crate::percpu::NCPU;
const SLOTS: usize = 7;

static USER: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static KERN: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static IDLE: [AtomicU64; CPUS] = [const { AtomicU64::new(0) }; CPUS];
static SLOT: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

pub fn on_timer() {
    let cpu = crate::percpu::id();
    if cpu >= CPUS {
        return;
    }
    let spsr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {spsr}, spsr_el1",
            spsr = out(reg) spsr,
            options(nomem, nostack)
        );
    }
    if spsr & 0xf == 0 {
        USER[cpu].fetch_add(1, Ordering::Relaxed);
        let slot = crate::percpu::slot();
        if slot < SLOTS {
            SLOT[slot].fetch_add(1, Ordering::Relaxed);
        }
    } else if crate::percpu::asleep() {
        IDLE[cpu].fetch_add(1, Ordering::Relaxed);
    } else {
        KERN[cpu].fetch_add(1, Ordering::Relaxed);
    }
}

/// `(user, kernel, idle)` ticks at 100 Hz.
pub fn cpu(cpu: usize) -> (u64, u64, u64) {
    if cpu >= CPUS {
        return (0, 0, 0);
    }
    (
        USER[cpu].load(Ordering::Relaxed),
        KERN[cpu].load(Ordering::Relaxed),
        IDLE[cpu].load(Ordering::Relaxed),
    )
}

pub fn slot_ticks(slot: usize) -> u64 {
    if slot >= SLOTS {
        0
    } else {
        SLOT[slot].load(Ordering::Relaxed)
    }
}
