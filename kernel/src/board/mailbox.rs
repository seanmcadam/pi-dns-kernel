// VideoCore property mailbox, channel 8.
//
// The caller picks the register base. This file does not know which board
// it is on. The buffer lives in .dma (Normal non-cacheable, 16-byte
// aligned). A faulting status read returns -1 and does not write the device.
// The property tags themselves are built by the HDMI process.

use core::ptr::{addr_of, addr_of_mut, copy_nonoverlapping};

const STATUS: u64 = 0x18;
const WRITE: u64 = 0x20;
const FULL: u32 = 0x8000_0000;
const EMPTY: u32 = 0x4000_0000;

#[link_section = ".dma"]
static mut PROP: [u32; 256] = [0; 256];

pub fn run(base: u64, words: &mut [u32]) -> i64 {
    if words.is_empty() || words.len() > 256 {
        return -4;
    }
    if crate::exception::read_u32_catching((base + STATUS) as *const u32).is_none() {
        return -1;
    }
    let start = crate::timer::get_ticks();
    unsafe {
        copy_nonoverlapping(words.as_ptr(), addr_of_mut!(PROP).cast(), words.len());
        let phys = crate::addr::virt_to_phys(addr_of!(PROP) as u64);
        if !exchange(base, phys, start) {
            return -5;
        }
        copy_nonoverlapping(
            addr_of!(PROP).cast::<u32>(),
            words.as_mut_ptr(),
            words.len(),
        );
    }
    0
}

fn exchange(base: u64, phys: u64, start: u64) -> bool {
    loop {
        if timed_out(start) {
            return false;
        }
        let Some(status) = reg(base, STATUS) else {
            return false;
        };
        if status & FULL == 0 {
            break;
        }
    }
    let msg = ((phys as u32) & !0xF) | 8;
    unsafe {
        core::ptr::write_volatile((base + WRITE) as *mut u32, msg);
    }
    loop {
        if timed_out(start) {
            return false;
        }
        let Some(status) = reg(base, STATUS) else {
            return false;
        };
        if status & EMPTY != 0 {
            continue;
        }
        let Some(value) = reg(base, 0) else {
            return false;
        };
        if value & 0xF == 8 {
            return true;
        }
    }
}

fn reg(base: u64, off: u64) -> Option<u32> {
    crate::exception::read_u32_catching((base + off) as *const u32)
}

fn timed_out(start: u64) -> bool {
    let freq = crate::timer::frequency();
    if freq == 0 {
        return true;
    }
    crate::timer::get_ticks().wrapping_sub(start) > freq / 5
}
