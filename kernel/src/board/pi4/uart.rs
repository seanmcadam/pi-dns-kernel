/// Pi 4 PL011 UART at physical 0xFE201000. QEMU uses `console` instead.
/// This module is compiled only for the Pi 4 image, which is untested on hardware.

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const UARTDR: usize = 0x00;
const UARTFR: usize = 0x18;
const UARTIBRD: usize = 0x24;
const UARTFBRD: usize = 0x28;
const UARTLCR_H: usize = 0x2C;
const UARTCR: usize = 0x30;
const UARTIMSC: usize = 0x38;

const FR_TXFF: u32 = 1 << 5;
const LCR_H_WLEN_8: u32 = 0x60;
const LCR_H_FEN: u32 = 0x10;
const CR_UARTEN: u32 = 0x01;
const CR_TXE: u32 = 0x100;
const CR_RXE: u32 = 0x200;

pub struct Pl011Uart {
    base_addr: usize,
}

impl Pl011Uart {
    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base_addr + offset) as *const u32) }
    }

    fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base_addr + offset) as *mut u32, value) }
    }

    /// Program 8-bit 115200 using an integer and fractional divisor.
    pub fn init_baud(&mut self, ibrd: u32, fbrd: u32) {
        self.write_reg(UARTCR, 0);
        self.write_reg(UARTIMSC, 0);
        self.write_reg(UARTIBRD, ibrd);
        self.write_reg(UARTFBRD, fbrd);
        self.write_reg(UARTLCR_H, LCR_H_FEN | LCR_H_WLEN_8);
        self.write_reg(UARTCR, CR_UARTEN | CR_TXE | CR_RXE);
    }

    pub fn write_byte(&mut self, byte: u8) {
        while (self.read_reg(UARTFR) & FR_TXFF) != 0 {
            core::hint::spin_loop();
        }
        self.write_reg(UARTDR, byte as u32);
    }
}

/// Pi 4 PL011 on GPIO 14/15. The UART clock is 48 MHz after `enable_uart=1`.
/// 48000000 / (16 * 115200) = 26.0416, so IBRD 26 and FBRD 3.
pub fn init_pi4() {
    unsafe {
        const GPFSEL1: *mut u32 = crate::addr::phys_to_virt(0xFE20_0004) as *mut u32;
        const GPIO_PUP_PDN: *mut u32 = crate::addr::phys_to_virt(0xFE20_00E4) as *mut u32;
        let mut sel = read_volatile(GPFSEL1);
        sel &= !((7 << 12) | (7 << 15));
        sel |= (4 << 12) | (4 << 15);
        write_volatile(GPFSEL1, sel);
        let mut pull = read_volatile(GPIO_PUP_PDN);
        pull &= !((3 << 28) | (3 << 30));
        pull |= (1 << 28) | (1 << 30);
        write_volatile(GPIO_PUP_PDN, pull);
    }
    Pl011Uart {
        base_addr: crate::addr::phys_to_virt(0xFE20_1000) as usize,
    }
    .init_baud(26, 3);
}

impl fmt::Write for Pl011Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

unsafe impl Send for Pl011Uart {}
unsafe impl Sync for Pl011Uart {}
