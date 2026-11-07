pub fn init() {
    // VBAR_EL1 is installed in boot.s. IRQ and FIQ stay masked. The vector
    // stubs only report an interrupt if one arrives. Timer interrupts and
    // the EL2-to-EL1 drop are not part of this kernel yet.
}

pub fn enable() {
    unsafe {
        core::arch::asm!("msr daifclr, #2");
    }
}

pub fn disable() {
    unsafe {
        core::arch::asm!("msr daifset, #2");
    }
}
