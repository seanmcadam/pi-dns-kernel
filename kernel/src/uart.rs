use core::fmt;
use tock_registers::{
    interfaces::{Readable, Writeable},
    register_bitfields, register_structs,
    registers::{ReadOnly, ReadWrite, WriteOnly},
};

// Mini UART (UART1) on Raspberry Pi 4
// Base address: 0xFE215040 (for Pi 4, using low peripheral mode)
const MINI_UART_BASE: usize = 0xFE21_5040;

register_bitfields! {
    u32,

    AUX_ENABLES [
        MINI_UART_ENABLE OFFSET(0) NUMBITS(1) []
    ],

    MU_IER [
        ENABLE_TX_INT OFFSET(1) NUMBITS(1) [],
        ENABLE_RX_INT OFFSET(0) NUMBITS(1) []
    ],

    MU_IIR [
        FIFO_CLEAR OFFSET(1) NUMBITS(2) []
    ],

    MU_LCR [
        DATA_SIZE OFFSET(0) NUMBITS(2) [
            SevenBit = 0b00,
            EightBit = 0b11
        ]
    ],

    MU_LSR [
        TX_IDLE OFFSET(6) NUMBITS(1) [],
        TX_EMPTY OFFSET(5) NUMBITS(1) [],
        RX_READY OFFSET(0) NUMBITS(1) []
    ],

    MU_CNTL [
        ENABLE_TX OFFSET(1) NUMBITS(1) [],
        ENABLE_RX OFFSET(0) NUMBITS(1) []
    ]
}

register_structs! {
    #[allow(non_snake_case)]
    RegisterBlock {
        (0x00 => AUX_IRQ: ReadOnly<u32>),
        (0x04 => AUX_ENABLES: ReadWrite<u32, AUX_ENABLES::Register>),
        (0x08 => _reserved1),
        (0x40 => MU_IO: ReadWrite<u32>),
        (0x44 => MU_IER: ReadWrite<u32, MU_IER::Register>),
        (0x48 => MU_IIR: ReadWrite<u32, MU_IIR::Register>),
        (0x4C => MU_LCR: ReadWrite<u32, MU_LCR::Register>),
        (0x50 => MU_MCR: ReadWrite<u32>),
        (0x54 => MU_LSR: ReadOnly<u32, MU_LSR::Register>),
        (0x58 => MU_MSR: ReadOnly<u32>),
        (0x5C => MU_SCRATCH: ReadWrite<u32>),
        (0x60 => MU_CNTL: ReadWrite<u32, MU_CNTL::Register>),
        (0x64 => MU_STAT: ReadOnly<u32>),
        (0x68 => MU_BAUD: ReadWrite<u32>),
        (0x6C => @END),
    }
}

pub struct Uart {
    base_addr: usize,
}

impl Uart {
    pub fn new() -> Self {
        Self {
            base_addr: MINI_UART_BASE - 0x40,
        }
    }

    fn registers(&self) -> &RegisterBlock {
        unsafe { &*(self.base_addr as *const RegisterBlock) }
    }

    pub fn init(&mut self) {
        let regs = self.registers();

        // Enable mini UART
        let val = regs.AUX_ENABLES.get() | (1 << 0);
        regs.AUX_ENABLES.set(val);

        // Disable interrupts
        regs.MU_IER.set(0);

        // Disable transmitter and receiver during setup
        regs.MU_CNTL.set(0);

        // 8-bit mode
        regs.MU_LCR.write(MU_LCR::DATA_SIZE::EightBit);

        // Set baud rate to 115200
        // Baud rate = system_clock / (8 * (baudrate_reg + 1))
        // 115200 = 250MHz / (8 * (baudrate_reg + 1))
        // baudrate_reg = 270
        regs.MU_BAUD.set(270);

        // Clear FIFOs
        regs.MU_IIR.write(MU_IIR::FIFO_CLEAR.val(0b11));

        // Enable transmitter and receiver
        regs.MU_CNTL
            .write(MU_CNTL::ENABLE_TX::SET + MU_CNTL::ENABLE_RX::SET);
    }

    pub fn write_byte(&mut self, byte: u8) {
        let regs = self.registers();

        // Wait for transmitter to be ready
        while !regs.MU_LSR.is_set(MU_LSR::TX_EMPTY) {
            cortex_a::asm::nop();
        }

        regs.MU_IO.set(byte as u32);
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        let regs = self.registers();

        if regs.MU_LSR.is_set(MU_LSR::RX_READY) {
            Some(regs.MU_IO.get() as u8)
        } else {
            None
        }
    }
}

impl fmt::Write for Uart {
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

unsafe impl Send for Uart {}
unsafe impl Sync for Uart {}
