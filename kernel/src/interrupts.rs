pub fn init() {
    // For now, we run with interrupts disabled
    // Future enhancement: set up GIC-400 for proper interrupt handling
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
