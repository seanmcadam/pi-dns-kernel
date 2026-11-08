//! Spin locks.
//!
//! Lock order, outermost first. A holder may take a later lock. It must not
//! take an earlier one.
//!
//! 1. `SCHED` — run queue, thread state, futex waiters, join waiters.
//!    IRQ-safe (the holder masks IRQ). The timer IRQ does not take it: it
//!    only counts the tick and, at EL0, returns to the scheduler.
//! 2. `FD` — that process's file-descriptor table. One lock per EL0 task.
//!    Not IRQ-safe. Socket syscalls take it and then `NET`.
//! 3. `CONFIG` — config records. Not IRQ-safe. May take `FS` and then `LOGQ`.
//! 4. `FS` — the FAT volume. Not IRQ-safe. One lock, any core.
//! 5. `NET` — smoltcp and the NIC. Not IRQ-safe. One lock, any core.
//! 6. `LOGQ` — the log queue and the recent-log buffer. Not IRQ-safe.
//!
//! `NET` and `FS` stay one lock each because neither library is reentrant
//! and the block device has a single command slot. Pinning them to core 0
//! would need a second queue for every send, recv, and file syscall. Any
//! core may take the lock. The timer IRQ never does, so a core that is
//! interrupted while holding one cannot deadlock on it.
//!
//! The UART and the panic path take no locks.

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
