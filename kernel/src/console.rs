//! Early UART output. QEMU's PL011 is at 0x09000000 and is already enabled.
//! The Pi 4 PL011 is at 0xFE201000 and is initialised before these writes.

#[cfg(not(feature = "pi4"))]
const UART_DR: *mut u32 = 0x0900_0000 as *mut u32;

#[cfg(feature = "pi4")]
const UART_DR: *mut u32 = 0xFE20_1000 as *mut u32;

pub fn write_str(s: &str) {
    unsafe {
        for byte in s.bytes() {
            core::ptr::write_volatile(UART_DR, byte as u32);
        }
    }
}
