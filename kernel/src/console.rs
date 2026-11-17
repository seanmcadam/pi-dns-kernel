//! Early UART output. The board names the PL011 data register. QEMU virt's
//! UART is already enabled. The Pi 4 PL011 and the Pi 5 debug UART are
//! initialised by `early_init` before these writes. The pointer is the high
//! alias: TTBR0 does not map devices.

pub fn write_str(s: &str) {
    let uart = crate::addr::phys_to_virt(crate::board::UART_PHYS) as *mut u32;
    unsafe {
        for byte in s.bytes() {
            core::ptr::write_volatile(uart, byte as u32);
        }
    }
    // The Pi 4 HDMI console is a second sink. It does not write the UART.
    #[cfg(feature = "board-pi4")]
    crate::fb_con::push(s);
}
