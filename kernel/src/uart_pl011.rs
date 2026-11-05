/// PL011 UART driver for QEMU virt machine
/// Base address: 0x09000000

use core::fmt;
use core::ptr::{read_volatile, write_volatile};

const PL011_BASE: usize = 0x0900_0000;

// PL011 register offsets
const UARTDR: usize = 0x00;     // Data Register
const UARTFR: usize = 0x18;     // Flag Register
const UARTIBRD: usize = 0x24;   // Integer Baud Rate Divisor
const UARTFBRD: usize = 0x28;   // Fractional Baud Rate Divisor
const UARTLCR_H: usize = 0x2C;  // Line Control Register
const UARTCR: usize = 0x30;     // Control Register
const UARTIMSC: usize = 0x38;   // Interrupt Mask Set/Clear

// Flag register bits
const FR_TXFF: u32 = 1 << 5;    // Transmit FIFO full
const FR_RXFE: u32 = 1 << 4;    // Receive FIFO empty

// Line control register bits
const LCR_H_WLEN_8: u32 = 0x60; // 8 bit word length
const LCR_H_FEN: u32 = 0x10;    // Enable FIFOs

// Control register bits
const CR_UARTEN: u32 = 0x01;    // UART enable
const CR_TXE: u32 = 0x100;      // Transmit enable
const CR_RXE: u32 = 0x200;      // Receive enable

pub struct Pl011Uart {
    base_addr: usize,
}

impl Pl011Uart {
    pub fn new() -> Self {
        Self {
            base_addr: PL011_BASE,
        }
    }
    
    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.base_addr + offset) as *const u32) }
    }
    
    fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.base_addr + offset) as *mut u32, value) }
    }
    
    pub fn init(&mut self) {
        // Disable UART
        self.write_reg(UARTCR, 0);
        
        // Clear all interrupts
        self.write_reg(UARTIMSC, 0);
        
        // Set baud rate to 115200
        // Assuming 24MHz clock: divisor = 24000000 / (16 * 115200) = 13.02
        // Integer part = 13, fractional part = 0.02 * 64 = 1
        self.write_reg(UARTIBRD, 13);
        self.write_reg(UARTFBRD, 1);
        
        // Enable FIFOs and set 8-bit word length
        self.write_reg(UARTLCR_H, LCR_H_FEN | LCR_H_WLEN_8);
        
        // Enable UART, TX, and RX
        self.write_reg(UARTCR, CR_UARTEN | CR_TXE | CR_RXE);
    }
    
    pub fn write_byte(&mut self, byte: u8) {
        // Wait for TX FIFO to have space
        while (self.read_reg(UARTFR) & FR_TXFF) != 0 {
            core::hint::spin_loop();
        }
        
        self.write_reg(UARTDR, byte as u32);
    }
    
    pub fn read_byte(&mut self) -> Option<u8> {
        // Check if RX FIFO has data
        if (self.read_reg(UARTFR) & FR_RXFE) == 0 {
            Some(self.read_reg(UARTDR) as u8)
        } else {
            None
        }
    }
}

impl fmt::Write for Pl011Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Convert \n to \r\n
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
