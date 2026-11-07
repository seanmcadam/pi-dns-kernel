pub fn init() {
    // VBAR_EL1 is installed in boot.s, after the EL2-to-EL1 drop. IRQ and
    // FIQ stay masked. The vector stubs only report an interrupt if one
    // arrives. There is no timer interrupt yet.
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
