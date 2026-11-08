//! IRQ controller bring-up.
//!
//! `VBAR_EL1` is installed in `boot.s` after the EL2-to-EL1 drop. This module
//! enables the GICv2 and the virtual-timer interrupt, then clears DAIF.I.
//! An IRQ taken at EL0 can end the current time slice. The handler
//! acknowledges the interrupt, stores an atomic, and returns. It does not
//! touch smoltcp and it does not take a lock. The Pi 4 GIC-400 path is
//! untested on hardware.

use core::sync::atomic::{AtomicU64, Ordering};

/// Returned by `irq_handler`. The EL0 path bumps the time slice only for a timer.
pub const CLASS_OTHER: u64 = 0;
pub const CLASS_TIMER: u64 = 1;
pub const CLASS_NET: u64 = 2;

static NIC_IRQS: AtomicU64 = AtomicU64::new(0);
static BLK_IRQS: AtomicU64 = AtomicU64::new(0);

pub fn irq_counts() -> (u64, u64) {
    (
        NIC_IRQS.load(Ordering::Relaxed),
        BLK_IRQS.load(Ordering::Relaxed),
    )
}

pub fn init() {
    crate::gic::init();
    crate::timer::arm();
    log::info!(
        "GICv2 virtual timer irq {} at {} Hz",
        crate::gic::TIMER_IRQ,
        crate::timer::TICK_HZ
    );
    enable();
}

/// GIC CPU interface and virtual timer for a core that is not core 0.
pub fn init_secondary() {
    crate::gic::init_cpu();
    crate::timer::arm();
    enable();
}

pub fn enable() {
    unmask();
}

/// Mask IRQ (DAIF.I). WFI still wakes when an interrupt is pending.
pub fn mask() {
    unsafe {
        core::arch::asm!("msr daifset, #2", options(nomem, nostack));
    }
}

pub fn unmask() {
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}

/// Wake a core sitting in WFE. Used when the block IRQ is delivered to a
/// different CPU than the thread waiting on the device.
pub fn event() {
    unsafe {
        core::arch::asm!("sev", options(nomem, nostack, preserves_flags));
    }
}

/// Called from the IRQ vector with general and SIMD registers already saved.
/// 1023 is spurious and is not completed. Any other id is completed so the
/// CPU interface cannot stick. Device status is acknowledged before EOI so
/// a level line does not reassert in the same window.
#[no_mangle]
pub extern "C" fn irq_handler() -> u64 {
    let id = crate::gic::ack();
    if id == crate::gic::SPURIOUS_IRQ {
        return CLASS_OTHER;
    }
    let class = classify(id);
    crate::gic::eoi(id);
    class
}

fn classify(id: u32) -> u64 {
    if id == crate::gic::TIMER_IRQ {
        crate::timer::on_tick();
        return CLASS_TIMER;
    }
    if id == crate::gic::SGI_NET {
        crate::net::service::note_kick();
        return CLASS_NET;
    }
    if id == crate::gic::SGI_KILL {
        return CLASS_OTHER;
    }
    match crate::board::handle_irq(id) {
        crate::board::IrqHit::Net => {
            NIC_IRQS.fetch_add(1, Ordering::Relaxed);
            crate::net::service::note_kick();
            return CLASS_NET;
        }
        crate::board::IrqHit::Block => {
            BLK_IRQS.fetch_add(1, Ordering::Relaxed);
            event();
            return CLASS_OTHER;
        }
        crate::board::IrqHit::None => {}
    }
    CLASS_OTHER
}
