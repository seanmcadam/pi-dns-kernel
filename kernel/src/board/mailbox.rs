// VideoCore property mailbox, channel 8.
//
// The caller picks the register base. This file does not know which board
// it is on. The buffer lives in .dma (Normal non-cacheable, 64-byte
// aligned) so it does not share a cache line with the next object. A
// faulting status read returns -1 and does not write the device. The
// property tags themselves are built by the HDMI process.

use core::ptr::{addr_of_mut, copy_nonoverlapping};
use core::sync::atomic::{AtomicU32, Ordering};

const STATUS: u64 = 0x18;
const WRITE: u64 = 0x20;
const FULL: u32 = 0x8000_0000;
const EMPTY: u32 = 0x4000_0000;

#[repr(C, align(64))]
struct MailboxProp {
    words: [u32; 256],
}

const _: () = assert!(core::mem::align_of::<MailboxProp>() == 64);
const _: () = assert!(core::mem::size_of::<MailboxProp>() % 64 == 0);

#[link_section = ".dma"]
static mut PROP: MailboxProp = MailboxProp { words: [0; 256] };

pub fn run(base: u64, words: &mut [u32]) -> i64 {
    if words.is_empty() || words.len() > 256 {
        return -4;
    }
    // Callers pass the physical mailbox. After the MMU is on, a load of
    // that address is a translation fault, not an external abort, so the
    // probe helper does not catch it.
    let base = crate::addr::phys_to_virt(base);
    if crate::exception::read_u32_catching((base + STATUS) as *const u32).is_none() {
        return -1;
    }
    SPINS.store(0, Ordering::Relaxed);
    let start = crate::timer::get_ticks();
    unsafe {
        let prop = addr_of_mut!(PROP);
        if (prop as usize) % 64 != 0 {
            return -6;
        }
        let dst = addr_of_mut!((*prop).words);
        copy_nonoverlapping(words.as_ptr(), dst.cast(), words.len());
        // The mailbox write is Device memory. Without this barrier the GPU
        // can observe the buffer address before these stores.
        core::arch::asm!("dsb sy", options(nostack));
        let virt = prop as *const u8;
        let phys = crate::addr::virt_to_phys(prop as u64);
        if !exchange(base, phys, start) {
            return -5;
        }
        // This runs after `.dma` is Normal non-cacheable. Drop a stale
        // write-back line so the copy below is the GPU's reply.
        drop_lines(virt, words.len() * 4);
        copy_nonoverlapping(dst.cast::<u32>(), words.as_mut_ptr(), words.len());
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

static SPINS: AtomicU32 = AtomicU32::new(0);

fn timed_out(start: u64) -> bool {
    let freq = crate::timer::frequency();
    if freq == 0 {
        // CNTFRQ is not programmed. A zero frequency used to make every
        // call an instant timeout, so the flag clear was never sent.
        return SPINS.fetch_add(1, Ordering::Relaxed) > 2_000_000;
    }
    // A fifth of a second was shorter than the VideoCore property channel
    // needs right after the HDMI mailbox traffic. The early tryboot clear
    // then timed out and left the EEPROM flag set.
    crate::timer::get_ticks().wrapping_sub(start) > freq
}

fn drop_lines(ptr: *const u8, len: usize) {
    let start = (ptr as usize) & !63;
    let end = (ptr as usize).saturating_add(len);
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc ivac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}
