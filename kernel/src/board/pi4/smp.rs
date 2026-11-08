//! Pi 4 spin table. Cores 1-3 watch physical `0xE0`, `0xE8`, and `0xF0`
//! (core n at `0xD8 + 8*n`). Untested on hardware. QEMU raspi4b parks the
//! extra cores the same way when it boots a raw kernel.

use crate::addr::phys_to_virt;

pub fn start_secondary(cpu: usize, entry: u64, _entry_el: u64) {
    let cell = phys_to_virt(0xd8 + (cpu as u64) * 8) as *mut u64;
    unsafe {
        core::ptr::write_volatile(cell, entry);
        core::arch::asm!(
            "dc cvac, {cell}",
            "dsb ish",
            "sev",
            cell = in(reg) cell,
            options(nostack),
        );
    }
}
