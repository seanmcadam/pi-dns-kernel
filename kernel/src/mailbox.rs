use tock_registers::{
    interfaces::{Readable, Writeable},
    registers::{ReadOnly, ReadWrite},
};

const MAILBOX_BASE: usize = 0xFE00_B880;

#[repr(C)]
struct MailboxRegisters {
    read: ReadOnly<u32>,
    _padding1: [u32; 3],
    peek: ReadOnly<u32>,
    sender: ReadOnly<u32>,
    status: ReadOnly<u32>,
    config: ReadWrite<u32>,
    write: ReadWrite<u32>,
}

const MAILBOX_FULL: u32 = 0x8000_0000;
const MAILBOX_EMPTY: u32 = 0x4000_0000;

pub struct Mailbox {
    registers: &'static MailboxRegisters,
}

impl Mailbox {
    pub fn new() -> Self {
        Self {
            registers: unsafe { &*(MAILBOX_BASE as *const MailboxRegisters) },
        }
    }

    pub fn call(&self, channel: u32, data: u32) -> Result<u32, &'static str> {
        // Wait for mailbox to be not full
        while (self.registers.status.get() & MAILBOX_FULL) != 0 {
            cortex_a::asm::nop();
        }

        // Write data with channel
        let msg = (data & !0xF) | (channel & 0xF);
        self.registers.write.set(msg);

        // Wait for response
        loop {
            while (self.registers.status.get() & MAILBOX_EMPTY) != 0 {
                cortex_a::asm::nop();
            }

            let response = self.registers.read.get();
            if (response & 0xF) == channel {
                return Ok(response & !0xF);
            }
        }
    }
}
