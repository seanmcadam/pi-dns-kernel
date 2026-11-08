//! Spin locks.
//!
//! Lock order, outermost first. A holder may take a later lock. It must not
//! take an earlier one.
//!
//! 1. `SCHED` — run queue, thread state, futex waiters, join waiters, and
//!    socket sleepers.
//!    IRQ-safe (the holder masks IRQ). The timer IRQ does not take it: it
//!    only counts the tick and, at EL0, returns to the scheduler.
//! 2. `FD` — that process's file-descriptor table. One lock per EL0 task.
//!    Not IRQ-safe. A socket syscall drops it before sleeping.
//! 3. `CONFIG` — config records. Not IRQ-safe. May take `FS` and then `LOGQ`.
//! 4. `FS` — the FAT volume. Not IRQ-safe. One lock, any core.
//! 5. `LOGQ` — the log queue and the recent-log buffer. Not IRQ-safe.
//!    The telnet fan-out is not this lock. It is a single-producer ring.
//!    Publishing a line does not take `LOGQ`. Waking a reader takes `SCHED`
//!    and nothing else.
//!
//! smoltcp and the NIC have no lock. Core 1 owns them after the stack is up
//! (core 0 until then, and on a one-core boot). Other cores enqueue a
//! request and sleep on `SCHED`. The owner may take `FD` to publish a
//! bind or accept, drop it, then take `SCHED` to wake the caller. It never
//! holds `FD` and `SCHED` together. `FS` stays one lock because the FAT
//! library is not reentrant and the block device has one command slot.
//! The block waiter holds that device mutex with IRQs enabled and sleeps
//! on `wfe`. The IRQ handler must not take it, or any lock in this list:
//! it acknowledges the GIC and the virtio-mmio status register and stores
//! an atomic. The timer IRQ takes none of these.
//!
//! The UART and the panic path take no locks.
//!
//! The supervisor lock is not in this list. It is taken alone and dropped
//! before any lock above, including `SCHED` and `LOGQ`.

use core::sync::atomic::{AtomicBool, Ordering};

pub struct Spin {
    locked: AtomicBool,
}

impl Spin {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    pub fn lock(&self) -> SpinGuard<'_> {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        SpinGuard { lock: self }
    }
}

pub struct SpinGuard<'a> {
    lock: &'a Spin,
}

impl Drop for SpinGuard<'_> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}

/// Masks IRQ on this core for the critical section.
pub struct IrqSpin {
    locked: AtomicBool,
}

impl IrqSpin {
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    pub fn lock(&self) -> IrqGuard<'_> {
        let daif: u64;
        unsafe {
            core::arch::asm!(
                "mrs {daif}, daif",
                "msr daifset, #2",
                daif = out(reg) daif,
                options(nomem, nostack),
            );
        }
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        IrqGuard { lock: self, daif }
    }
}

pub struct IrqGuard<'a> {
    lock: &'a IrqSpin,
    daif: u64,
}

impl Drop for IrqGuard<'_> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        unsafe {
            core::arch::asm!(
                "msr daif, {daif}",
                daif = in(reg) self.daif,
                options(nomem, nostack),
            );
        }
    }
}
