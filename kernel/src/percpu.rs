//! Per-CPU data, pointed at by `TPIDR_EL1`.
//!
//! `task.s` uses these offsets. They are the current EL0 context, that
//! thread's kernel stack, the kernel resume block, and the preemption slice.

use crate::task::TaskContext;
use core::sync::atomic::{AtomicU32, Ordering};

pub const NCPU: usize = 4;

#[repr(C, align(64))]
pub struct PerCpu {
    pub current: *mut TaskContext,
    pub ksp: u64,
    pub resume_regs: [u64; 12],
    pub resume_sp: u64,
    pub resume_daif: u64,
    pub action: u64,
    pub cpu_id: u64,
    pub slice: u64,
    pub thread_index: u64,
    pub slot: u64,
}

const _: () = assert!(core::mem::offset_of!(PerCpu, current) == 0);
const _: () = assert!(core::mem::offset_of!(PerCpu, ksp) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, resume_regs) == 16);
const _: () = assert!(core::mem::offset_of!(PerCpu, resume_sp) == 112);
const _: () = assert!(core::mem::offset_of!(PerCpu, resume_daif) == 120);
const _: () = assert!(core::mem::offset_of!(PerCpu, action) == 128);
const _: () = assert!(core::mem::offset_of!(PerCpu, cpu_id) == 136);
const _: () = assert!(core::mem::offset_of!(PerCpu, slice) == 144);
const _: () = assert!(core::mem::offset_of!(PerCpu, thread_index) == 152);
const _: () = assert!(core::mem::offset_of!(PerCpu, slot) == 160);

static mut CPUS: [PerCpu; NCPU] = [EMPTY; NCPU];

const EMPTY: PerCpu = PerCpu {
    current: core::ptr::null_mut(),
    ksp: 0,
    resume_regs: [0; 12],
    resume_sp: 0,
    resume_daif: 0,
    action: 0,
    cpu_id: 0,
    slice: 0,
    thread_index: 0,
    slot: 0,
};

static BOOTED: AtomicU32 = AtomicU32::new(0);

fn this() -> *mut PerCpu {
    let ptr: u64;
    unsafe {
        core::arch::asm!("mrs {ptr}, tpidr_el1", ptr = out(reg) ptr, options(nomem, nostack));
    }
    ptr as *mut PerCpu
}

pub fn init_bsp() {
    unsafe {
        let cpu = core::ptr::addr_of_mut!(CPUS[0]);
        (*cpu).cpu_id = 0;
        core::arch::asm!(
            "msr tpidr_el1, {cpu}",
            "isb",
            cpu = in(reg) cpu,
            options(nostack),
        );
    }
    mark_online(0);
}

pub fn init_secondary(id: usize) {
    unsafe {
        let cpu = core::ptr::addr_of_mut!(CPUS[id]);
        core::ptr::write(cpu, EMPTY);
        (*cpu).cpu_id = id as u64;
        core::arch::asm!(
            "msr tpidr_el1, {cpu}",
            "isb",
            cpu = in(reg) cpu,
            options(nostack),
        );
    }
}

pub fn id() -> usize {
    unsafe { (*this()).cpu_id as usize }
}

pub fn current() -> *mut TaskContext {
    unsafe { (*this()).current }
}

pub fn set_slice(value: u64) {
    unsafe { (*this()).slice = value }
}

pub fn bump_slice(quantum: u64) -> bool {
    unsafe {
        let cpu = this();
        (*cpu).slice = (*cpu).slice.wrapping_add(1);
        if (*cpu).slice >= quantum {
            (*cpu).slice = 0;
            true
        } else {
            false
        }
    }
}

pub fn thread_index() -> u64 {
    unsafe { (*this()).thread_index }
}

pub fn set_thread_index(index: u64) {
    unsafe { (*this()).thread_index = index }
}

pub fn slot() -> usize {
    unsafe { (*this()).slot as usize }
}

pub fn set_slot(slot: usize) {
    unsafe { (*this()).slot = slot as u64 }
}

pub fn mark_online(cpu: usize) {
    if cpu < 32 {
        BOOTED.fetch_or(1 << cpu, Ordering::Release);
    }
}

pub fn online_count() -> u32 {
    BOOTED.load(Ordering::Acquire).count_ones()
}
