//! One EL0 task, entered at boot to prove the round trip.
//!
//! Cooperative yield returns to the kernel and the kernel resumes the same
//! task. A timer IRQ at EL0 saves the task, ticks, and erets to it. There is
//! no preemption: the IRQ does not switch tasks.
//!
//! Syscall number in x8, arguments in x0-x5, result in x0.
//!   0 log(ptr, len)
//!   1 yield
//!   2 uptime_ticks
//!   3 exit(code)
//!   4-17 sockets, time, and config. See user_fd.
//! Anything else returns -1. A log pointer outside the user window returns -2.
//! Neither path faults. The boot program proves the round trip. After the
//! network is up, `start_dns` loads the DNS program into the same window and
//! the poll loop resumes it on every yield. Yield is cooperative.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

core::arch::global_asm!(include_str!("task.s"));

const USER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/user.bin"));
const DNS_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dns.bin"));
include!(concat!(env!("OUT_DIR"), "/dns_meta.rs"));

const RESUME: u64 = 0;
const YIELD: u64 = 1;
const EXIT: u64 = 2;
const FAULT: u64 = 3;

#[repr(C, align(16))]
pub struct TaskContext {
    pub x: [u64; 31],
    pub sp_el0: u64,
    pub elr: u64,
    pub spsr: u64,
    pub fpsr: u64,
    pub fpcr: u64,
    pub q: [u64; 64],
}

const _: () = assert!(core::mem::offset_of!(TaskContext, sp_el0) == 248);
const _: () = assert!(core::mem::offset_of!(TaskContext, elr) == 256);
const _: () = assert!(core::mem::offset_of!(TaskContext, spsr) == 264);
const _: () = assert!(core::mem::offset_of!(TaskContext, fpsr) == 272);
const _: () = assert!(core::mem::offset_of!(TaskContext, fpcr) == 280);
const _: () = assert!(core::mem::offset_of!(TaskContext, q) == 288);

#[repr(C, align(16))]
struct KernelResume {
    _x19_x30: [u64; 12],
    _sp: u64,
    _daif: u64,
}

#[repr(C, align(4096))]
struct UserMem {
    code: [u8; 512 * 1024],
    data: [u8; 64 * 1024],
    heap: [u8; 1536 * 1024],
    stack: [u8; 64 * 1024],
}

const KSTACK_BYTES: usize = 64 * 1024;

// Context, then a gap, then the stack. Without the gap the 32 KiB stack
// ended on the first byte of the context, so an EL1 IRQ frame or a kernel
// spill sat on the registers ERET restores. A fault there aborts at EL1
// and halts the kernel.
#[repr(C, align(16))]
struct Task {
    ctx: TaskContext,
    _gap: [u8; 4096],
    kstack: [u8; KSTACK_BYTES],
}

#[no_mangle]
static mut CURRENT_CTX: *mut TaskContext = core::ptr::null_mut();
#[no_mangle]
static mut TASK_KSP: u64 = 0;
#[no_mangle]
static mut KERNEL_RESUME: KernelResume = KernelResume {
    _x19_x30: [0; 12],
    _sp: 0,
    _daif: 0,
};
#[no_mangle]
static mut EL0_ACTION: u64 = 0;

static mut TASK: Task = Task {
    ctx: TaskContext {
        x: [0; 31],
        sp_el0: 0,
        elr: 0,
        spsr: 0,
        fpsr: 0,
        fpcr: 0,
        q: [0; 64],
    },
    _gap: [0; 4096],
    kstack: [0; KSTACK_BYTES],
};

static mut SCENARIO: u64 = 0;
static mut EXIT_CODE: u64 = 0;
static mut LOG_OK: bool = false;
static mut YIELD_OK: bool = false;
static mut TICKS_SEEN: bool = false;
static mut EXIT_OK: bool = false;
static mut BAD_SYSCALL: bool = false;
static mut BAD_POINTER: bool = false;
static mut KERN_KILLED: bool = false;
static mut PRIV_KILLED: bool = false;
static mut EXEC_KILLED: bool = false;
static EL0_IRQS: AtomicU64 = AtomicU64::new(0);
static mut DNS_LIVE: bool = false;

extern "C" {
    fn enter_user(ctx: *mut TaskContext, kernel_sp: u64) -> u64;
    static __text_start: u8;
}

pub fn run_boot_tests() {
    if USER_BIN.len() > crate::mmu::USER_CODE_BYTES as usize {
        put("EL0 test: user program does not fit\r\n");
        return;
    }
    map_window();
    publish_code();
    if crate::mmu::pan_enabled() {
        put("EL0 PAN on\r\n");
    } else {
        put("EL0 PAN not implemented\r\n");
    }

    let kernel = unsafe { addr_of!(__text_start) as u64 };
    let past = crate::mmu::USER_CODE_VA + crate::mmu::USER_CODE_BYTES;
    run_scenario(0, 0);
    run_scenario(1, 0);
    run_scenario(2, past);
    run_scenario(3, kernel);
    run_scenario(4, 0);
    run_scenario(5, 0);
    crate::mmu::set_user_asid(false);
    crate::mmu::flush_tlb();
    unsafe {
        CURRENT_CTX = core::ptr::null_mut();
    }
    print_summary();
}

fn publish_code() {
    unsafe {
        let dst = addr_of_mut!(USER_MEM.code) as *mut u8;
        core::ptr::copy_nonoverlapping(USER_BIN.as_ptr(), dst, USER_BIN.len());
        let user = crate::mmu::USER_CODE_VA;
        let mut off = 0usize;
        while off < USER_BIN.len() {
            let kva = dst as usize + off;
            let uva = user as usize + off;
            core::arch::asm!(
                "dc cvau, {kva}",
                "ic ivau, {kva}",
                "ic ivau, {uva}",
                kva = in(reg) kva,
                uva = in(reg) uva,
                options(nostack),
            );
            off += 64;
        }
        core::arch::asm!("dsb sy", "isb", options(nostack));
    }
}

fn run_scenario(scenario: u64, arg1: u64) {
    unsafe {
        SCENARIO = scenario;
        let ctx = addr_of_mut!(TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).x[0] = scenario;
        (*ctx).x[1] = arg1;
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + crate::mmu::USER_STACK_BYTES;
        CURRENT_CTX = ctx;
        let ksp = addr_of!(TASK.kstack) as u64 + KSTACK_BYTES as u64;
        TASK_KSP = ksp;
        crate::mmu::set_user_asid(true);
        loop {
            let action = enter_user(ctx, ksp);
            if action == YIELD {
                YIELD_OK = true;
                put("EL0 yield: back in kernel\r\n");
                continue;
            }
            break;
        }
    }
}

fn put(s: &str) {
    crate::console::write_str(s);
}

pub fn print_summary() {
    put("EL0 test: log ");
    put(if unsafe { LOG_OK } { "ok" } else { "FAIL" });
    put(", yield ");
    put(if unsafe { YIELD_OK } { "ok" } else { "FAIL" });
    put(", irq ");
    let irq = unsafe { TICKS_SEEN } && EL0_IRQS.load(Ordering::Relaxed) > 0;
    put(if irq { "ok" } else { "FAIL" });
    put(", exit ");
    put(if unsafe { EXIT_OK } { "0" } else { "FAIL" });
    put(", bad-syscall ");
    put(if unsafe { BAD_SYSCALL } { "-1" } else { "FAIL" });
    put(", bad-pointer ");
    put(if unsafe { BAD_POINTER } {
        "rejected"
    } else {
        "FAIL"
    });
    put(", kernel-read ");
    put(if unsafe { KERN_KILLED } {
        "killed"
    } else {
        "FAIL"
    });
    put(", priv ");
    put(if unsafe { PRIV_KILLED } {
        "killed"
    } else {
        "FAIL"
    });
    put(", exec-data ");
    put(if unsafe { EXEC_KILLED } {
        "killed"
    } else {
        "FAIL"
    });
    put("\r\n");
}

#[no_mangle]
pub extern "C" fn el0_no_task() -> ! {
    put("EL0 exception with no task\r\n");
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

#[no_mangle]
pub extern "C" fn el0_dispatch(kind: u64) -> u64 {
    if kind == 1 {
        EL0_IRQS.fetch_add(1, Ordering::Relaxed);
        crate::interrupts::irq_handler();
        return RESUME;
    }
    let esr = reg("esr_el1");
    let ec = (esr >> 26) & 0x3f;
    if ec == 0x15 {
        return syscall();
    }
    kill(ec);
    FAULT
}

fn syscall() -> u64 {
    let ctx = unsafe { &mut *CURRENT_CTX };
    let num = ctx.x[8];
    let a0 = ctx.x[0];
    let a1 = ctx.x[1];
    match num {
        0 => {
            ctx.x[0] = sys_log(a0, a1);
            RESUME
        }
        1 => {
            ctx.x[0] = 0;
            YIELD
        }
        2 => {
            ctx.x[0] = crate::timer::irq_ticks();
            RESUME
        }
        3 => {
            unsafe {
                EXIT_CODE = a0;
                if SCENARIO == 0 && a0 == 0 {
                    EXIT_OK = true;
                }
            }
            EXIT
        }
        4..=17 => {
            let args = [ctx.x[0], ctx.x[1], ctx.x[2], ctx.x[3], ctx.x[4], ctx.x[5]];
            ctx.x[0] = crate::user_fd::call(num, args);
            RESUME
        }
        _ => {
            unsafe { BAD_SYSCALL = true };
            put("EL0 syscall: unknown ");
            dec(num);
            put(" -> -1\r\n");
            ctx.x[0] = (-1i64) as u64;
            RESUME
        }
    }
}

fn sys_log(ptr: u64, len: u64) -> u64 {
    let mut buf = [0u8; 128];
    if len > buf.len() as u64 || !crate::mmu::user_slice_ok(ptr, len) {
        unsafe { BAD_POINTER = true };
        put("EL0 log: rejected pointer\r\n");
        return (-2i64) as u64;
    }
    crate::mmu::set_pan(false);
    let mut i = 0u64;
    while i < len {
        buf[i as usize] = unsafe { core::ptr::read_volatile((ptr + i) as *const u8) };
        i += 1;
    }
    crate::mmu::set_pan(true);
    put("user: ");
    put(core::str::from_utf8(&buf[..len as usize]).unwrap_or("?"));
    put("\r\n");
    let text = core::str::from_utf8(&buf[..len as usize]).unwrap_or("");
    unsafe {
        if SCENARIO == 0 && text == "hello from el0" {
            LOG_OK = true;
        }
        if SCENARIO == 0 && text == "ticks advanced" {
            TICKS_SEEN = true;
        }
    }
    0
}

fn kill(ec: u64) {
    let far = reg("far_el1");
    let elr = reg("elr_el1");
    put("EL0 killed: scenario ");
    dec(unsafe { SCENARIO });
    put(" ec=");
    hex(ec);
    put(" elr=");
    hex(elr);
    put(" far=");
    hex(far);
    put("\r\n");
    unsafe {
        match SCENARIO {
            3 => KERN_KILLED = true,
            4 => PRIV_KILLED = true,
            5 => EXEC_KILLED = true,
            _ => {}
        }
    }
}

fn reg(name: &str) -> u64 {
    let value: u64;
    unsafe {
        match name {
            "esr_el1" => core::arch::asm!("mrs {value}, esr_el1", value = out(reg) value),
            "far_el1" => core::arch::asm!("mrs {value}, far_el1", value = out(reg) value),
            "elr_el1" => core::arch::asm!("mrs {value}, elr_el1", value = out(reg) value),
            _ => value = 0,
        }
    }
    value
}

fn dec(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 {
        put("0");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    put(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

fn hex(mut n: u64) {
    put("0x");
    let mut buf = [0u8; 16];
    let mut i = 16;
    while i > 0 {
        i -= 1;
        let nib = (n & 0xf) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
        n >>= 4;
    }
    put(unsafe { core::str::from_utf8_unchecked(&buf) });
}

static mut USER_MEM: UserMem = UserMem {
    code: [0; 512 * 1024],
    data: [0; 64 * 1024],
    heap: [0; 1536 * 1024],
    stack: [0; 64 * 1024],
};

fn map_window() {
    unsafe {
        let code_pa = addr_of!(USER_MEM.code) as u64;
        let data_pa = addr_of!(USER_MEM.data) as u64;
        let heap_pa = addr_of!(USER_MEM.heap) as u64;
        let stack_pa = addr_of!(USER_MEM.stack) as u64;
        crate::mmu::map_user_window(code_pa, data_pa, heap_pa, stack_pa);
    }
}

pub fn start_dns() -> Result<(), ()> {
    if DNS_BIN.len() > crate::mmu::USER_CODE_BYTES as usize {
        put("EL0 dns: program does not fit\r\n");
        return Err(());
    }
    map_window();
    load_dns_image();
    unsafe {
        SCENARIO = 0xff;
        let ctx = addr_of_mut!(TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + crate::mmu::USER_STACK_BYTES;
        CURRENT_CTX = ctx;
        let ksp = addr_of!(TASK.kstack) as u64 + KSTACK_BYTES as u64;
        TASK_KSP = ksp;
        crate::mmu::set_user_asid(true);
        DNS_LIVE = true;
        if enter_user(ctx, ksp) == YIELD {
            Ok(())
        } else {
            DNS_LIVE = false;
            CURRENT_CTX = core::ptr::null_mut();
            Err(())
        }
    }
}

pub fn poll_dns() {
    if !unsafe { DNS_LIVE } {
        return;
    }
    unsafe {
        let ctx = addr_of_mut!(TASK.ctx);
        let ksp = TASK_KSP;
        if enter_user(ctx, ksp) != YIELD {
            DNS_LIVE = false;
            CURRENT_CTX = core::ptr::null_mut();
            put("EL0 dns stopped\r\n");
        }
    }
}

fn load_dns_image() {
    unsafe {
        let code = addr_of_mut!(USER_MEM.code) as *mut u8;
        let data = addr_of_mut!(USER_MEM.data) as *mut u8;
        core::ptr::write_bytes(code, 0, crate::mmu::USER_CODE_BYTES as usize);
        core::ptr::write_bytes(data, 0, crate::mmu::USER_DATA_BYTES as usize);
        core::ptr::write_bytes(
            addr_of_mut!(USER_MEM.heap) as *mut u8,
            0,
            crate::mmu::USER_HEAP_BYTES as usize,
        );
        core::ptr::write_bytes(
            addr_of_mut!(USER_MEM.stack) as *mut u8,
            0,
            crate::mmu::USER_STACK_BYTES as usize,
        );
        let code_len = DNS_BIN.len().min(crate::mmu::USER_CODE_BYTES as usize);
        core::ptr::copy_nonoverlapping(DNS_BIN.as_ptr(), code, code_len);
        if DNS_DATA_LEN > 0
            && DNS_DATA_OFF + DNS_DATA_LEN <= DNS_BIN.len()
            && DNS_DATA_PLACE + DNS_DATA_LEN <= crate::mmu::USER_DATA_BYTES as usize
        {
            core::ptr::copy_nonoverlapping(
                DNS_BIN.as_ptr().add(DNS_DATA_OFF),
                data.add(DNS_DATA_PLACE),
                DNS_DATA_LEN,
            );
        }
        maintain_code(code, code_len);
    }
}

fn maintain_code(kernel_ptr: *mut u8, len: usize) {
    unsafe {
        let user = crate::mmu::USER_CODE_VA as usize;
        let mut off = 0;
        while off < len {
            let kva = kernel_ptr as usize + off;
            let uva = user + off;
            core::arch::asm!(
                "dc cvau, {kva}",
                "ic ivau, {kva}",
                "ic ivau, {uva}",
                kva = in(reg) kva,
                uva = in(reg) uva,
                options(nostack),
            );
            off += 64;
        }
        core::arch::asm!("dsb sy", "isb", options(nostack));
    }
}
