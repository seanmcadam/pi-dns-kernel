//! Secondary cores.
//!
//! QEMU `virt` starts only core 0. The others are off until a PSCI
//! `CPU_ON`. QEMU 8.2's virt machine advertises the HVC conduit in the
//! DTB, including when the boot CPU entered at EL1, so this image issues
//! HVC. The entry point is the physical address of `secondary_entry` in
//! `boot.s`. That code turns the MMU on with the primary's TTBR1 and jumps
//! to `secondary_main`.
//!
//! Raspberry Pi 4 firmware parks cores 1-3 on the spin table at physical
//! `0xE0`, `0xE8`, and `0xF0` (core n watches `0xD8 + 8*n`). This image
//! writes the same entry point there and sends an event. That path is
//! compiled into the Pi 4 image and is untested on hardware. If the stub
//! is not parked on those words, the stores hit low RAM and the cores stay
//! offline. The boot line then reports fewer than 4 cores.

use crate::addr::virt_to_phys;
use core::sync::atomic::{AtomicBool, Ordering};

const NCPU: usize = 4;

#[no_mangle]
static mut SEC_SP: [u64; NCPU] = [0; NCPU];
#[no_mangle]
static mut SEC_TTBR1: u64 = 0;
#[no_mangle]
static mut SEC_MAIR: u64 = 0;
#[no_mangle]
static mut SEC_TCR: u64 = 0;

/// 16 KiB stack with an unmapped page under it and one at the initial SP.
/// The bytes stay in this array. `mmu::init` leaves both pages unmapped.
#[repr(C, align(4096))]
struct CpuStack {
    below: [u8; 4096],
    mem: [u8; 16 * 1024],
    above: [u8; 4096],
}

const fn blank_cpu_stack() -> CpuStack {
    CpuStack {
        below: [0; 4096],
        mem: [0; 16 * 1024],
        above: [0; 4096],
    }
}

static mut SEC_STACKS: [CpuStack; NCPU] = [
    blank_cpu_stack(),
    blank_cpu_stack(),
    blank_cpu_stack(),
    blank_cpu_stack(),
];

static STARTED: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn secondary_entry();
    static mut early_l0_id: u64;
    static early_l1_id: u64;
}

/// True when `va` is the unmapped page under or above a secondary stack.
pub fn is_stack_guard_va(va: u64) -> bool {
    let page = va & !4095;
    let mut cpu = 0;
    while cpu < NCPU {
        unsafe {
            let below = core::ptr::addr_of!(SEC_STACKS[cpu].below) as u64;
            let above = core::ptr::addr_of!(SEC_STACKS[cpu].above) as u64;
            if page == below || page == above {
                return true;
            }
        }
        cpu += 1;
    }
    false
}

pub fn is_stack_guard_pa(pa: u64) -> bool {
    is_stack_guard_va(crate::addr::phys_to_virt(pa & !4095))
}

pub fn online() -> u32 {
    crate::percpu::online_count()
}

pub fn boot(entry_el: u64) {
    if crate::board::el2_blocks_secondaries() {
        // HVC from EL1 is delivered to EL2, and EL2 has no PSCI handler.
        // The default QEMU virt machine does not implement EL2.
        crate::console::write_str(
            "EL2 is present, extra cores stay offline\r\n",
        );
        return;
    }
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    unsafe {
        let ttbr1: u64;
        let mair: u64;
        let tcr: u64;
        core::arch::asm!(
            "mrs {ttbr1}, ttbr1_el1",
            "mrs {mair}, mair_el1",
            "mrs {tcr}, tcr_el1",
            ttbr1 = out(reg) ttbr1,
            mair = out(reg) mair,
            tcr = out(reg) tcr,
            options(nomem, nostack),
        );
        SEC_TTBR1 = ttbr1;
        SEC_MAIR = mair;
        SEC_TCR = tcr;
        let mut cpu = 1;
        while cpu < NCPU {
            let top = core::ptr::addr_of!(SEC_STACKS[cpu].mem) as u64 + 16 * 1024;
            SEC_SP[cpu] = virt_to_phys(top);
            cpu += 1;
        }
        clean(core::ptr::addr_of!(SEC_SP) as *const u8, 8 * NCPU);
        clean(core::ptr::addr_of!(SEC_TTBR1) as *const u8, 8);
        clean(core::ptr::addr_of!(SEC_MAIR) as *const u8, 8);
        clean(core::ptr::addr_of!(SEC_TCR) as *const u8, 8);
        core::arch::asm!("dsb sy", options(nostack));
    }
    unsafe {
        let l1 = virt_to_phys(core::ptr::addr_of!(early_l1_id) as u64);
        let l0 = core::ptr::addr_of_mut!(early_l0_id);
        core::ptr::write_volatile(l0, l1 | 3);
        core::arch::asm!("dc cvac, {l0}", "dsb sy", l0 = in(reg) l0, options(nostack));
    }
    let entry = virt_to_phys(secondary_entry as *const () as u64);
    let mut cpu = 1usize;
    while cpu < NCPU {
        crate::board::start_secondary(cpu, entry, entry_el);
        cpu += 1;
    }
    let deadline = crate::timer::millis().wrapping_add(1000);
    while online() < NCPU as u32 && crate::timer::millis() < deadline {
        core::hint::spin_loop();
    }
}

unsafe fn clean(ptr: *const u8, len: usize) {
    let mut off = 0;
    while off < len + 64 {
        core::arch::asm!(
            "dc cvac, {ptr}",
            ptr = in(reg) ptr as usize + off,
            options(nostack),
        );
        off += 64;
    }
}

#[no_mangle]
pub extern "C" fn secondary_main(cpu: u64) -> ! {
    if cpu == 0 || cpu >= NCPU as u64 {
        loop {
            unsafe {
                core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
            }
        }
    }
    let id = cpu as usize;
    crate::percpu::init_secondary(id);
    crate::percpu::mark_online(id);
    crate::interrupts::init_secondary();
    loop {
        if id == 1 && crate::net::service::try_become_owner() {
            loop {
                crate::net::service::owner_idle();
            }
        }
        if !crate::task::schedule_one() {
            crate::percpu::set_asleep(true);
            unsafe {
                if id == 1 {
                    // WFI, not WFE, so the timer wakes this core to notice
                    // that the stack is ready. A `sev` from install does too.
                    core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
                } else {
                    core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
                }
            }
            crate::percpu::set_asleep(false);
        }
    }
}
