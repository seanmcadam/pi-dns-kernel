//! IRQ controller bring-up.
//!
//! `VBAR_EL1` is installed in `boot.s` after the EL2-to-EL1 drop. This module
//! enables the GICv2 and the virtual-timer interrupt, then clears DAIF.I.
//! The poll loop is not preempted: the tick is only a time source. The Pi 4
//! GIC-400 path is untested on hardware.

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

pub fn enable() {
    unsafe {
        core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
    }
}

/// Called from the current-EL IRQ vector with general and SIMD registers
/// already saved. 1023 is spurious and is not completed. Any other id is
/// completed so the CPU interface cannot stick.
#[no_mangle]
pub extern "C" fn irq_handler() {
    let id = crate::gic::ack();
    if id == crate::gic::SPURIOUS_IRQ {
        return;
    }
    if id == crate::gic::TIMER_IRQ {
        crate::timer::on_tick();
    }
    crate::gic::eoi(id);
}
