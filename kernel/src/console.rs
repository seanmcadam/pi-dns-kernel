//! Early UART output. QEMU's PL011 is at physical 0x09000000 and is already
//! enabled. The Pi 4 PL011 is at physical 0xFE201000 and is initialised before
//! these writes. The pointer is the high alias: TTBR0 does not map devices.

#[cfg(not(feature = "pi4"))]
const UART_DR: *mut u32 = crate::addr::phys_to_virt(0x0900_0000) as *mut u32;

#[cfg(feature = "pi4")]
const UART_DR: *mut u32 = crate::addr::phys_to_virt(0xFE20_1000) as *mut u32;

pub fn write_str(s: &str) {
    unsafe {
        for byte in s.bytes() {
            core::ptr::write_volatile(UART_DR, byte as u32);
        }
    }
}
