//! BCM2712 debug UART. This is `uart10` in the device tree: a PL011 at
//! physical `0x107d001000`, on the dedicated 3-pin connector, not GPIO 14/15.
//!
//! `clk_uart` is 9_216_000 Hz. 9216000 / (16 * 115200) = 5 exactly, so
//! IBRD is 5 and FBRD is 0. There is no GPIO mux to program.
//!
//! The early page tables map this address before `kernel_main`. The clock
//! in the device tree may not match the clock the firmware left running.
//! This has not been run on hardware.

use core::ptr::write_volatile;

const UARTIBRD: usize = 0x24;
const UARTFBRD: usize = 0x28;
const UARTLCR_H: usize = 0x2C;
const UARTCR: usize = 0x30;
const UARTIMSC: usize = 0x38;

const LCR_H_WLEN_8: u32 = 0x60;
const LCR_H_FEN: u32 = 0x10;
const CR_UARTEN: u32 = 0x01;
const CR_TXE: u32 = 0x100;
const CR_RXE: u32 = 0x200;

pub fn init_pi5() {
    let base = crate::addr::phys_to_virt(0x1_07d0_01000);
    unsafe {
        let reg = |offset: usize| (base + offset as u64) as *mut u32;
        write_volatile(reg(UARTCR), 0);
        write_volatile(reg(UARTIMSC), 0);
        write_volatile(reg(UARTIBRD), 5);
        write_volatile(reg(UARTFBRD), 0);
        write_volatile(reg(UARTLCR_H), LCR_H_FEN | LCR_H_WLEN_8);
        write_volatile(reg(UARTCR), CR_UARTEN | CR_TXE | CR_RXE);
    }
}
