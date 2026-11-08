//! Two EL0 tasks: the boot test, then DNS and HTTP.
//!
//! Each task has its own user pages, TTBR0 root, ASID, kernel stack, and fd
//! table. They share the virtual window at `0x1_0000_0000`. Switching tasks
//! writes that task's TTBR0 and ASID, then erets into its saved context.
//!
//! A task yields when its socket would block or after it has finished one
//! turn. A timer IRQ at EL0 also ends the turn after two ticks (20 ms). The
//! IRQ returns to the poll loop instead of switching inside the handler, so
//! the NIC is polled on that path and smoltcp is never reentered. An IRQ
//! taken during a syscall stays at EL1 and does not preempt: the stack is
//! only touched from the poll loop. The boot test still watches the tick
//! count, and it now does so across a preemption.
//!
//! Syscall number in x8, arguments in x0-x5, result in x0.
//!   0 log(ptr, len)
//!   1 yield
//!   2 uptime_ticks
//!   3 exit(code)
//!   4-33 sockets, time, config, files, and the log task. See user_fd.
//!   34 thread_create(entry, stack, arg) -> tid
//!   35 thread_exit(code)
//!   36 thread_join(tid) -> exit code
//!   37 futex_wait(addr, expected) -> 0, or 1 if the word changed
//!   38 futex_wake(addr, n) -> count
//!   39 log_publish(ptr, len). Log task only. Never waits.
//!   40 log_follow(cursor, buf, cap, drops, flags) -> len.
//!      The caller sleeps when the ring is idle. flags 1 returns 0 instead.
//!   41 log_wake(). Wakes log followers. Does not publish a line.
//! Anything else returns -1. A log pointer outside the user window returns -2.
//! Neither path faults. The boot program proves the round trip on slot 0.
//! DNS reuses that slot. HTTP uses slot 1.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

core::arch::global_asm!(include_str!("task.s"));

const USER_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/user.bin"));
const DNS_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dns.bin"));
const HTTP_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/http.bin"));
const LOG_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/log.bin"));
const TELNET_BIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/telnet.bin"));
include!(concat!(env!("OUT_DIR"), "/dns_meta.rs"));
include!(concat!(env!("OUT_DIR"), "/http_meta.rs"));
include!(concat!(env!("OUT_DIR"), "/log_meta.rs"));
include!(concat!(env!("OUT_DIR"), "/telnet_meta.rs"));

const RESUME: u64 = 0;
const YIELD: u64 = 1;
const EXIT: u64 = 2;
const FAULT: u64 = 3;
const PREEMPT: u64 = 4;
const BLOCK: u64 = 5;

const LOG_PUBLISH: u64 = 39;
const LOG_FOLLOW: u64 = 40;
const LOG_WAKE: u64 = 41;
const THREAD_CREATE: u64 = 34;
const THREAD_EXIT: u64 = 35;
const THREAD_JOIN: u64 = 36;
const FUTEX_WAIT: u64 = 37;
const FUTEX_WAKE: u64 = 38;

/// The virtual timer is 100 Hz, so each tick is 10 ms. A task that never
/// yields is forced back to the poll loop after this many EL0 ticks.
const QUANTUM_TICKS: u64 = 2;

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

static mut LOG_TASK: Task = Task {
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

static mut SPIN_TASK: Task = Task {
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

static mut HTTP_TASK: Task = Task {
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
static mut PREEMPT_OK: bool = false;
static EL0_IRQS: AtomicU64 = AtomicU64::new(0);
static mut DNS_LIVE: bool = false;
static mut HTTP_LIVE: bool = false;
static mut SPIN_LIVE: bool = false;
static mut LOG_LIVE: bool = false;
static mut TELNET_LIVE: bool = false;

const DNS_SLOT: usize = 0;
const HTTP_SLOT: usize = 1;
const SPIN_SLOT: usize = 2;
const LOG_SLOT: usize = 3;
const TELNET_SLOT: usize = 4;
const TASK_SLOTS: usize = 5;
const DNS_ASID: u64 = 1;
const HTTP_ASID: u64 = 2;
const SPIN_ASID: u64 = 3;
const LOG_ASID: u64 = 4;
const TELNET_ASID: u64 = 5;

const LOG_CODE_BYTES: usize = 128 * 1024;
const LOG_DATA_BYTES: usize = 4096;
const LOG_STACK_BYTES: usize = 16 * 1024;
const TELNET_CODE_BYTES: usize = 128 * 1024;
const TELNET_DATA_BYTES: usize = 4096;
const TELNET_HEAP_BYTES: usize = 256 * 1024;
const TELNET_STACK_BYTES: usize = 16 * 1024;

/// `b .` — a task that never makes a syscall. Preemption is the only way out.
const SPIN_CODE: [u8; 4] = [0x00, 0x00, 0x00, 0x14];

extern "C" {
    fn enter_user(ctx: *mut TaskContext, kernel_sp: u64) -> u64;
    static __text_start: u8;
}

fn enter(ctx: *mut TaskContext, ksp: u64) -> u64 {
    crate::percpu::set_slice(0);
    unsafe { enter_user(ctx, ksp) }
}

fn still_scheduled(action: u64) -> bool {
    action == YIELD || action == PREEMPT
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

    let kernel = addr_of!(__text_start) as u64;
    let past = crate::mmu::USER_CODE_VA + crate::mmu::USER_CODE_BYTES;
    run_scenario(0, 0);
    run_scenario(1, 0);
    run_scenario(2, past);
    run_scenario(3, kernel);
    run_scenario(4, 0);
    run_scenario(5, 0);
    run_scenario(6, 0);
    crate::mmu::set_user_asid(false);
    crate::mmu::flush_tlb();
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
        (&mut *ctx).x[0] = scenario;
        (&mut *ctx).x[1] = arg1;
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + crate::mmu::USER_STACK_BYTES;
        let ksp = addr_of!(TASK.kstack) as u64 + KSTACK_BYTES as u64;
        crate::percpu::set_slot(0);
        crate::percpu::set_thread_index(0);
        crate::mmu::set_user_asid(true);
        loop {
            let action = enter(ctx, ksp);
            if action == YIELD {
                YIELD_OK = true;
                put("EL0 yield: back in kernel\r\n");
                continue;
            }
            if action == PREEMPT {
                continue;
            }
            if action == BLOCK {
                wait_if_blocked(0, 1);
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
    // Ticks must move, and a timer IRQ must have preempted scenario 0.
    // Re-entering the same task is what lets it observe the new count.
    let irq = unsafe { TICKS_SEEN && PREEMPT_OK } && EL0_IRQS.load(Ordering::Relaxed) > 0;
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
        let class = crate::interrupts::irq_handler();
        // Only the timer advances the slice. A NIC IRQ must not look like
        // the boot test's timer preemption, and it must not burn the quantum.
        if class == crate::interrupts::CLASS_TIMER && crate::percpu::bump_slice(QUANTUM_TICKS) {
            unsafe {
                if SCENARIO == 0 {
                    PREEMPT_OK = true;
                }
            }
            return PREEMPT;
        }
        // Network work outranks the user thread on the owner core.
        if class == crate::interrupts::CLASS_NET && crate::net::service::polling_here() {
            return PREEMPT;
        }
        return RESUME;
    }
    let esr = reg("esr_el1");
    let ec = (esr >> 26) & 0x3f;
    if ec == 0x15 {
        return after_syscall(syscall());
    }
    kill(ec);
    FAULT
}

/// A syscall that finished while a NIC kick was pending returns to the
/// owner loop with the result already stored in the user context. The
/// syscall itself is not aborted.
fn after_syscall(action: u64) -> u64 {
    if action == RESUME
        && crate::net::service::polling_here()
        && crate::net::service::kick_pending()
    {
        return PREEMPT;
    }
    action
}

fn syscall() -> u64 {
    let ctx = unsafe { &mut *crate::percpu::current() };
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
            if crate::percpu::thread_index() != 0 {
                return thread_exit(a0 as i64);
            }
            unsafe {
                EXIT_CODE = a0;
                if SCENARIO == 0 && a0 == 0 {
                    EXIT_OK = true;
                }
            }
            EXIT
        }
        THREAD_CREATE => thread_create(a0, a1, ctx.x[2]),
        THREAD_EXIT => thread_exit(a0 as i64),
        THREAD_JOIN => thread_join(a0),
        FUTEX_WAIT => futex_wait(a0, a1),
        FUTEX_WAKE => futex_wake(a0, a1),
        LOG_PUBLISH => {
            ctx.x[0] = log_publish(a0, a1);
            RESUME
        }
        LOG_FOLLOW => return log_follow(),
        LOG_WAKE => {
            wake_log_readers();
            ctx.x[0] = 0;
            RESUME
        }
        4..=33 => {
            let args = [ctx.x[0], ctx.x[1], ctx.x[2], ctx.x[3], ctx.x[4], ctx.x[5]];
            ctx.x[0] = crate::user_fd::call(num, args);
            if let Some(id) = crate::net::service::take_posted() {
                return match crate::net::service::wait(id) {
                    crate::net::service::Wait::Ready(result) => {
                        ctx.x[0] = result;
                        RESUME
                    }
                    crate::net::service::Wait::Sleep => BLOCK,
                };
            }
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

#[repr(C, align(4096))]
struct SpinMem {
    code: [u8; 4096],
    stack: [u8; 4096],
}

#[repr(C, align(4096))]
struct LogMem {
    code: [u8; LOG_CODE_BYTES],
    data: [u8; LOG_DATA_BYTES],
    stack: [u8; LOG_STACK_BYTES],
}

static mut LOG_MEM: LogMem = LogMem {
    code: [0; LOG_CODE_BYTES],
    data: [0; LOG_DATA_BYTES],
    stack: [0; LOG_STACK_BYTES],
};

#[repr(C, align(4096))]
struct TelnetMem {
    code: [u8; TELNET_CODE_BYTES],
    data: [u8; TELNET_DATA_BYTES],
    heap: [u8; TELNET_HEAP_BYTES],
    stack: [u8; TELNET_STACK_BYTES],
}

static mut TELNET_MEM: TelnetMem = TelnetMem {
    code: [0; TELNET_CODE_BYTES],
    data: [0; TELNET_DATA_BYTES],
    heap: [0; TELNET_HEAP_BYTES],
    stack: [0; TELNET_STACK_BYTES],
};

static mut TELNET_TASK: Task = Task {
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

static mut SPIN_MEM: SpinMem = SpinMem {
    code: [0; 4096],
    stack: [0; 4096],
};

static mut HTTP_MEM: UserMem = UserMem {
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
        crate::mmu::map_user_window(0, code_pa, data_pa, heap_pa, stack_pa);
    }
}

pub fn start_dns() -> Result<(), ()> {
    if !image_fits(DNS_BIN.len(), DNS_DATA_OFF, DNS_DATA_LEN) {
        put("EL0 dns: program does not fit\r\n");
        return Err(());
    }
    map_window();
    load_image(
        addr_of_mut!(USER_MEM),
        DNS_BIN,
        DNS_DATA_OFF,
        DNS_DATA_LEN,
        DNS_DATA_PLACE,
    );
    unsafe {
        SCENARIO = 0xff;
        let ctx = addr_of_mut!(TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + crate::mmu::USER_STACK_BYTES;
        crate::user_fd::set_task(DNS_SLOT);
        crate::mmu::use_user_slot(DNS_SLOT, DNS_ASID);
        let ksp = addr_of!(TASK.kstack) as u64 + KSTACK_BYTES as u64;
        crate::percpu::set_slot(DNS_SLOT);
        crate::percpu::set_thread_index(0);
        DNS_LIVE = true;
        loop {
            match enter(ctx, ksp) {
                YIELD | BLOCK => return Ok(()),
                PREEMPT => continue,
                _ => {
                    DNS_LIVE = false;
                    return Err(());
                }
            }
        }
    }
}

pub fn poll_dns() {
    resume(DNS_SLOT, DNS_ASID, addr_of_mut!(TASK), true);
}

pub fn start_http() -> Result<(), ()> {
    if !image_fits(HTTP_BIN.len(), HTTP_DATA_OFF, HTTP_DATA_LEN) {
        put("EL0 http: program does not fit\r\n");
        return Err(());
    }
    map_http_window();
    load_image(
        addr_of_mut!(HTTP_MEM),
        HTTP_BIN,
        HTTP_DATA_OFF,
        HTTP_DATA_LEN,
        HTTP_DATA_PLACE,
    );
    unsafe {
        SCENARIO = 0xff;
        let ctx = addr_of_mut!(HTTP_TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + crate::mmu::USER_STACK_BYTES;
        crate::user_fd::set_task(HTTP_SLOT);
        crate::mmu::use_user_slot(HTTP_SLOT, HTTP_ASID);
        let ksp = addr_of!(HTTP_TASK.kstack) as u64 + KSTACK_BYTES as u64;
        crate::percpu::set_slot(HTTP_SLOT);
        crate::percpu::set_thread_index(0);
        HTTP_LIVE = true;
        loop {
            match enter(ctx, ksp) {
                YIELD | BLOCK => return Ok(()),
                PREEMPT => continue,
                _ => {
                    HTTP_LIVE = false;
                    return Err(());
                }
            }
        }
    }
}

pub fn poll_http() {
    resume(HTTP_SLOT, HTTP_ASID, addr_of_mut!(HTTP_TASK), false);
}

pub fn log_live() -> bool {
    unsafe { LOG_LIVE }
}

pub fn start_log() -> Result<(), ()> {
    if LOG_BIN.len() > LOG_CODE_BYTES {
        put("EL0 log: program does not fit\r\n");
        return Err(());
    }
    unsafe {
        let code = addr_of_mut!(LOG_MEM.code) as *mut u8;
        core::ptr::write_bytes(code, 0, LOG_CODE_BYTES);
        core::ptr::copy_nonoverlapping(LOG_BIN.as_ptr(), code, LOG_BIN.len());
        if LOG_DATA_LEN > 0
            && LOG_DATA_OFF + LOG_DATA_LEN <= LOG_BIN.len()
            && LOG_DATA_PLACE + LOG_DATA_LEN <= LOG_DATA_BYTES
        {
            core::ptr::copy_nonoverlapping(
                LOG_BIN.as_ptr().add(LOG_DATA_OFF),
                addr_of_mut!(LOG_MEM.data).cast(),
                LOG_DATA_LEN,
            );
        }
        crate::mmu::map_user_sized(
            LOG_SLOT,
            addr_of!(LOG_MEM.code) as u64,
            LOG_CODE_BYTES as u64,
            addr_of!(LOG_MEM.data) as u64,
            LOG_DATA_BYTES as u64,
            addr_of!(LOG_MEM.stack) as u64,
            LOG_STACK_BYTES as u64,
            0,
            0,
        );
        maintain_code(code, LOG_BIN.len());
        let ctx = addr_of_mut!(LOG_TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + LOG_STACK_BYTES as u64;
        crate::user_fd::set_task(LOG_SLOT);
        crate::mmu::use_user_slot(LOG_SLOT, LOG_ASID);
        let ksp = addr_of!(LOG_TASK.kstack) as u64 + KSTACK_BYTES as u64;
        crate::percpu::set_slot(LOG_SLOT);
        crate::percpu::set_thread_index(0);
        LOG_LIVE = true;
        loop {
            match enter(ctx, ksp) {
                YIELD | BLOCK => return Ok(()),
                PREEMPT => continue,
                _ => {
                    LOG_LIVE = false;
                    return Err(());
                }
            }
        }
    }
}

pub fn poll_log() {
    if !unsafe { LOG_LIVE } {
        return;
    }
    if main_blocked(LOG_SLOT) {
        return;
    }
    crate::user_fd::set_task(LOG_SLOT);
    crate::mmu::use_user_slot(LOG_SLOT, LOG_ASID);
    unsafe {
        crate::percpu::set_slot(LOG_SLOT);
        crate::percpu::set_thread_index(0);
        let ctx = addr_of_mut!(LOG_TASK.ctx);
        let ksp = addr_of!(LOG_TASK.kstack) as u64 + KSTACK_BYTES as u64;
        let action = enter(ctx, ksp);
        if action == BLOCK {
            return;
        }
        if !still_scheduled(action) {
            LOG_LIVE = false;
            put("EL0 log stopped\r\n");
        }
    }
}

pub fn start_telnet() -> Result<(), ()> {
    if TELNET_BIN.len() > TELNET_CODE_BYTES {
        put("EL0 telnet: program does not fit\r\n");
        return Err(());
    }
    unsafe {
        let code = addr_of_mut!(TELNET_MEM.code) as *mut u8;
        core::ptr::write_bytes(code, 0, TELNET_CODE_BYTES);
        core::ptr::write_bytes(
            addr_of_mut!(TELNET_MEM.heap) as *mut u8,
            0,
            TELNET_HEAP_BYTES,
        );
        core::ptr::copy_nonoverlapping(TELNET_BIN.as_ptr(), code, TELNET_BIN.len());
        if TELNET_DATA_LEN > 0
            && TELNET_DATA_OFF + TELNET_DATA_LEN <= TELNET_BIN.len()
            && TELNET_DATA_PLACE + TELNET_DATA_LEN <= TELNET_DATA_BYTES
        {
            core::ptr::copy_nonoverlapping(
                TELNET_BIN.as_ptr().add(TELNET_DATA_OFF),
                addr_of_mut!(TELNET_MEM.data).cast(),
                TELNET_DATA_LEN,
            );
        }
        crate::mmu::map_user_sized(
            TELNET_SLOT,
            addr_of!(TELNET_MEM.code) as u64,
            TELNET_CODE_BYTES as u64,
            addr_of!(TELNET_MEM.data) as u64,
            TELNET_DATA_BYTES as u64,
            addr_of!(TELNET_MEM.stack) as u64,
            TELNET_STACK_BYTES as u64,
            addr_of!(TELNET_MEM.heap) as u64,
            TELNET_HEAP_BYTES as u64,
        );
        maintain_code(code, TELNET_BIN.len());
        let ctx = addr_of_mut!(TELNET_TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + TELNET_STACK_BYTES as u64;
        crate::user_fd::set_task(TELNET_SLOT);
        crate::mmu::use_user_slot(TELNET_SLOT, TELNET_ASID);
        let ksp = addr_of!(TELNET_TASK.kstack) as u64 + KSTACK_BYTES as u64;
        crate::percpu::set_slot(TELNET_SLOT);
        crate::percpu::set_thread_index(0);
        TELNET_LIVE = true;
        loop {
            match enter(ctx, ksp) {
                YIELD | BLOCK => return Ok(()),
                PREEMPT => continue,
                _ => {
                    TELNET_LIVE = false;
                    return Err(());
                }
            }
        }
    }
}

pub fn poll_telnet() {
    if !unsafe { TELNET_LIVE } {
        return;
    }
    if main_blocked(TELNET_SLOT) {
        return;
    }
    crate::user_fd::set_task(TELNET_SLOT);
    crate::mmu::use_user_slot(TELNET_SLOT, TELNET_ASID);
    unsafe {
        crate::percpu::set_slot(TELNET_SLOT);
        crate::percpu::set_thread_index(0);
        let ctx = addr_of_mut!(TELNET_TASK.ctx);
        let ksp = addr_of!(TELNET_TASK.kstack) as u64 + KSTACK_BYTES as u64;
        let action = enter(ctx, ksp);
        if action == BLOCK {
            return;
        }
        if !still_scheduled(action) {
            TELNET_LIVE = false;
            put("EL0 telnet stopped\r\n");
        }
    }
}

pub fn poll_spin() {
    if !unsafe { SPIN_LIVE } {
        return;
    }
    crate::user_fd::set_task(SPIN_SLOT);
    crate::mmu::use_user_slot(SPIN_SLOT, SPIN_ASID);
    unsafe {
        crate::percpu::set_slot(SPIN_SLOT);
        crate::percpu::set_thread_index(0);
        let ctx = addr_of_mut!(SPIN_TASK.ctx);
        let ksp = addr_of!(SPIN_TASK.kstack) as u64 + KSTACK_BYTES as u64;
        if !still_scheduled(enter(ctx, ksp)) {
            SPIN_LIVE = false;
            put("EL0 spin stopped\r\n");
        }
    }
}

/// Arm the EL0 program that branches to itself. The caller is in a syscall,
/// so this only publishes the task. The poll loop runs it.
pub fn arm_spinner() -> i64 {
    unsafe {
        if SPIN_LIVE {
            return 0;
        }
        let code = addr_of_mut!(SPIN_MEM.code) as *mut u8;
        core::ptr::write_bytes(code, 0, 4096);
        core::ptr::copy_nonoverlapping(SPIN_CODE.as_ptr(), code, SPIN_CODE.len());
        crate::mmu::map_user_stub(
            SPIN_SLOT,
            addr_of!(SPIN_MEM.code) as u64,
            addr_of!(SPIN_MEM.stack) as u64,
        );
        maintain_code(code, 64);
        let ctx = addr_of_mut!(SPIN_TASK.ctx);
        core::ptr::write_bytes(ctx, 0, 1);
        (*ctx).elr = crate::mmu::USER_CODE_VA;
        (*ctx).spsr = 0;
        (*ctx).sp_el0 = crate::mmu::USER_STACK_VA + 4096;
        SPIN_LIVE = true;
        let slot = crate::percpu::slot();
        crate::mmu::use_user_slot(slot, slot as u64 + 1);
        put("EL0 spin task armed\r\n");
        0
    }
}

fn resume(slot: usize, asid: u64, task: *mut Task, dns: bool) {
    let live = unsafe { if dns { DNS_LIVE } else { HTTP_LIVE } };
    if !live || main_blocked(slot) {
        return;
    }
    crate::user_fd::set_task(slot);
    crate::mmu::use_user_slot(slot, asid);
    unsafe {
        crate::percpu::set_slot(slot);
        crate::percpu::set_thread_index(0);
        let ctx = &mut (*task).ctx as *mut TaskContext;
        let ksp = addr_of!((*task).kstack) as u64 + KSTACK_BYTES as u64;
        let action = enter(ctx, ksp);
        if action == BLOCK || still_scheduled(action) {
            return;
        }
        if dns {
            DNS_LIVE = false;
            put("EL0 dns stopped\r\n");
        } else {
            HTTP_LIVE = false;
            put("EL0 http stopped\r\n");
        }
    }
}

fn map_http_window() {
    unsafe {
        let code_pa = addr_of!(HTTP_MEM.code) as u64;
        let data_pa = addr_of!(HTTP_MEM.data) as u64;
        let heap_pa = addr_of!(HTTP_MEM.heap) as u64;
        let stack_pa = addr_of!(HTTP_MEM.stack) as u64;
        crate::mmu::map_user_window(1, code_pa, data_pa, heap_pa, stack_pa);
    }
}

fn image_fits(bin_len: usize, data_off: usize, data_len: usize) -> bool {
    let code = crate::mmu::USER_CODE_BYTES as usize;
    if data_off == 0 {
        bin_len <= code
    } else {
        data_off >= code && data_off + data_len <= bin_len
    }
}

fn load_image(mem: *mut UserMem, bin: &[u8], data_off: usize, data_len: usize, data_place: usize) {
    unsafe {
        let code = addr_of_mut!((*mem).code) as *mut u8;
        let data = addr_of_mut!((*mem).data) as *mut u8;
        core::ptr::write_bytes(code, 0, crate::mmu::USER_CODE_BYTES as usize);
        core::ptr::write_bytes(data, 0, crate::mmu::USER_DATA_BYTES as usize);
        core::ptr::write_bytes(
            addr_of_mut!((*mem).heap) as *mut u8,
            0,
            crate::mmu::USER_HEAP_BYTES as usize,
        );
        core::ptr::write_bytes(
            addr_of_mut!((*mem).stack) as *mut u8,
            0,
            crate::mmu::USER_STACK_BYTES as usize,
        );
        let code_len = bin.len().min(crate::mmu::USER_CODE_BYTES as usize);
        core::ptr::copy_nonoverlapping(bin.as_ptr(), code, code_len);
        if data_len > 0
            && data_off + data_len <= bin.len()
            && data_place + data_len <= crate::mmu::USER_DATA_BYTES as usize
        {
            core::ptr::copy_nonoverlapping(bin.as_ptr().add(data_off), data.add(data_place), data_len);
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
        core::arch::asm!("dsb ish", "isb", options(nostack));
    }
}

const MAX_THREADS: usize = 32;
const ST_EMPTY: u8 = 0;
const ST_RUNNABLE: u8 = 1;
const ST_RUNNING: u8 = 2;
const ST_BLOCKED: u8 = 3;
const ST_EXITED: u8 = 4;
const WAITER_NONE: i16 = -1;
const WAITER_MAIN: i16 = -2;

struct PoolThread {
    task: Task,
    slot: u8,
    asid: u64,
    state: u8,
    exit_code: i64,
    waiter: i16,
    waiter_slot: u8,
    futex_addr: u64,
    futex_next: i16,
}

static SCHED: crate::spin::IrqSpin = crate::spin::IrqSpin::new();

pub fn with_sched<R>(f: impl FnOnce() -> R) -> R {
    let _guard = SCHED.lock();
    f()
}

/// Caller holds `SCHED`.
pub fn block_current_locked() {
    let slot = crate::percpu::slot();
    let index = crate::percpu::thread_index();
    if index == 0 {
        unsafe {
            MAIN_FUTEX[slot] = 0;
        }
        if slot < TASK_SLOTS {
            MAIN_BLOCKED[slot].store(true, Ordering::Release);
        }
    } else {
        let mine = (index - 1) as usize;
        unsafe {
            if mine < MAX_THREADS {
                // A pool thread can block on a socket after it was waiting on
                // a futex. Clear that address or the next futex_wake will
                // resume the socket syscall with a zero result and abandon
                // the bytes already queued in the socket.
                unlink_futex(mine);
                THREADS[mine].futex_addr = 0;
                THREADS[mine].state = ST_BLOCKED;
            }
        }
    }
}

fn unlink_futex(mine: usize) {
    unsafe {
        let mut prev = WAITER_NONE;
        let mut cur = FUTEX_HEAD;
        while cur >= 0 {
            let index = cur as usize;
            let next = THREADS[index].futex_next;
            if index == mine {
                if prev < 0 {
                    FUTEX_HEAD = next;
                } else {
                    THREADS[prev as usize].futex_next = next;
                }
                THREADS[mine].futex_next = WAITER_NONE;
                return;
            }
            prev = cur;
            cur = next;
        }
    }
}

/// Caller holds `SCHED`.
pub fn wake_locked(slot: usize, thread_index: u64, result: u64) {
    if thread_index == 0 {
        unsafe {
            (&mut *main_ctx(slot)).x[0] = result;
        }
        if slot < TASK_SLOTS {
            MAIN_BLOCKED[slot].store(false, Ordering::Release);
        }
        sev();
        return;
    }
    let mine = (thread_index - 1) as usize;
    unsafe {
        if mine < MAX_THREADS && THREADS[mine].state == ST_BLOCKED {
            THREADS[mine].task.ctx.x[0] = result;
            THREADS[mine].state = ST_RUNNABLE;
            queue_push(mine as i16);
            sev();
        }
    }
}
static MAIN_BLOCKED: [core::sync::atomic::AtomicBool; TASK_SLOTS] =
    [const { core::sync::atomic::AtomicBool::new(false) }; TASK_SLOTS];
static mut MAIN_FUTEX: [u64; TASK_SLOTS] = [0; TASK_SLOTS];

const fn blank_thread() -> PoolThread {
    PoolThread {
        task: Task {
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
        },
        slot: 0,
        asid: 0,
        state: ST_EMPTY,
        exit_code: 0,
        waiter: WAITER_NONE,
        waiter_slot: 0,
        futex_addr: 0,
        futex_next: WAITER_NONE,
    }
}

static mut THREADS: [PoolThread; MAX_THREADS] = [const { blank_thread() }; MAX_THREADS];
static mut RUN_Q: [i16; MAX_THREADS] = [-1; MAX_THREADS];
static mut RUN_HEAD: usize = 0;
static mut RUN_TAIL: usize = 0;
static mut RUN_LEN: usize = 0;
static mut FUTEX_HEAD: i16 = WAITER_NONE;

fn main_blocked(slot: usize) -> bool {
    slot < TASK_SLOTS && MAIN_BLOCKED[slot].load(Ordering::Acquire)
}

fn wait_if_blocked(slot: usize, asid: u64) {
    while main_blocked(slot) {
        crate::net::poll();
        if !main_blocked(slot) {
            break;
        }
        if !schedule_one() {
            if crate::net::polling_here() {
                crate::net::service::owner_idle();
            } else {
                unsafe {
                    core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
                }
            }
        }
    }
    crate::user_fd::set_task(slot);
    crate::mmu::use_user_slot(slot, asid);
    crate::percpu::set_slot(slot);
    crate::percpu::set_thread_index(0);
}

fn main_ctx(slot: usize) -> *mut TaskContext {
    unsafe {
        match slot {
            HTTP_SLOT => addr_of_mut!(HTTP_TASK.ctx),
            SPIN_SLOT => addr_of_mut!(SPIN_TASK.ctx),
            LOG_SLOT => addr_of_mut!(LOG_TASK.ctx),
            TELNET_SLOT => addr_of_mut!(TELNET_TASK.ctx),
            _ => addr_of_mut!(TASK.ctx),
        }
    }
}

fn sev() {
    unsafe {
        core::arch::asm!("sev", options(nomem, nostack, preserves_flags));
    }
}

fn queue_push(index: i16) {
    unsafe {
        if RUN_LEN >= MAX_THREADS {
            return;
        }
        RUN_Q[RUN_TAIL] = index;
        RUN_TAIL = (RUN_TAIL + 1) % MAX_THREADS;
        RUN_LEN += 1;
    }
}

fn queue_pop() -> Option<usize> {
    unsafe {
        if RUN_LEN == 0 {
            return None;
        }
        let index = RUN_Q[RUN_HEAD];
        RUN_HEAD = (RUN_HEAD + 1) % MAX_THREADS;
        RUN_LEN -= 1;
        if index < 0 {
            None
        } else {
            Some(index as usize)
        }
    }
}

pub fn schedule_one() -> bool {
    let index = {
        let _guard = SCHED.lock();
        let Some(index) = queue_pop() else {
            return false;
        };
        unsafe { THREADS[index].state = ST_RUNNING; }
        index
    };
    let (ctx, ksp, slot, asid) = unsafe {
        let thread = &mut THREADS[index];
        (
            addr_of_mut!(thread.task.ctx),
            addr_of!(thread.task.kstack) as u64 + KSTACK_BYTES as u64,
            thread.slot as usize,
            thread.asid,
        )
    };
    crate::percpu::set_slot(slot);
    crate::percpu::set_thread_index(index as u64 + 1);
    crate::user_fd::set_task(slot);
    crate::mmu::use_user_slot(slot, asid);
    let action = enter(ctx, ksp);
    crate::percpu::set_thread_index(0);
    let _guard = SCHED.lock();
    unsafe {
        let thread = &mut THREADS[index];
        match action {
            YIELD | PREEMPT => {
                if thread.state == ST_RUNNING {
                    thread.state = ST_RUNNABLE;
                    queue_push(index as i16);
                    sev();
                }
            }
            BLOCK => {}
            _ => {
                if thread.state == ST_RUNNING {
                    thread.exit_code = -1;
                    thread.state = ST_EXITED;
                    wake_waiter(index);
                }
            }
        }
    }
    true
}

fn wake_waiter(index: usize) {
    unsafe {
        let code = THREADS[index].exit_code;
        let waiter = THREADS[index].waiter;
        let slot = THREADS[index].waiter_slot as usize;
        THREADS[index].waiter = WAITER_NONE;
        if waiter == WAITER_MAIN {
            (&mut *main_ctx(slot)).x[0] = code as u64;
            MAIN_FUTEX[slot] = 0;
            if slot < TASK_SLOTS {
                MAIN_BLOCKED[slot].store(false, Ordering::Release);
            }
            sev();
        } else if waiter >= 0 {
            let other = waiter as usize;
            if other < MAX_THREADS {
                THREADS[other].task.ctx.x[0] = code as u64;
                THREADS[other].state = ST_RUNNABLE;
                queue_push(waiter);
                sev();
            }
        }
    }
}

fn thread_create(entry: u64, stack: u64, arg: u64) -> u64 {
    let ctx = unsafe { &mut *crate::percpu::current() };
    let code_end = crate::mmu::USER_CODE_VA + crate::mmu::USER_CODE_BYTES;
    if entry < crate::mmu::USER_CODE_VA || entry.saturating_add(4) > code_end || (entry & 3) != 0 {
        ctx.x[0] = (-4i64) as u64;
        return RESUME;
    }
    if stack < 256 || (stack & 15) != 0 || !crate::mmu::user_write_ok(stack - 256, 256) {
        ctx.x[0] = (-2i64) as u64;
        return RESUME;
    }
    let slot = crate::percpu::slot();
    let asid = slot as u64 + 1;
    let _guard = SCHED.lock();
    unsafe {
        let mut chosen = None;
        let mut i = 0;
        while i < MAX_THREADS {
            let state = THREADS[i].state;
            if state == ST_EMPTY || (state == ST_EXITED && THREADS[i].waiter == WAITER_NONE) {
                chosen = Some(i);
                break;
            }
            i += 1;
        }
        let Some(index) = chosen else {
            ctx.x[0] = (-6i64) as u64;
            return RESUME;
        };
        let thread = &mut THREADS[index];
        core::ptr::write_bytes(addr_of_mut!(thread.task.ctx), 0, 1);
        thread.task.ctx.x[0] = arg;
        thread.task.ctx.elr = entry;
        thread.task.ctx.spsr = 0;
        thread.task.ctx.sp_el0 = stack;
        thread.slot = slot as u8;
        thread.asid = asid;
        thread.state = ST_RUNNABLE;
        thread.exit_code = 0;
        thread.waiter = WAITER_NONE;
        thread.waiter_slot = slot as u8;
        thread.futex_addr = 0;
        thread.futex_next = WAITER_NONE;
        queue_push(index as i16);
        ctx.x[0] = (index as u64) + 1;
    }
    sev();
    RESUME
}

fn thread_exit(code: i64) -> u64 {
    let index = crate::percpu::thread_index();
    if index == 0 {
        return EXIT;
    }
    let _guard = SCHED.lock();
    unsafe {
        let mine = (index - 1) as usize;
        THREADS[mine].exit_code = code;
        THREADS[mine].state = ST_EXITED;
        wake_waiter(mine);
    }
    EXIT
}

fn thread_join(tid: u64) -> u64 {
    let ctx = unsafe { &mut *crate::percpu::current() };
    if tid == 0 || tid as usize > MAX_THREADS {
        ctx.x[0] = (-4i64) as u64;
        return RESUME;
    }
    let index = tid as usize - 1;
    let self_index = crate::percpu::thread_index();
    let slot = crate::percpu::slot();
    let _guard = SCHED.lock();
    unsafe {
        if THREADS[index].state == ST_EMPTY {
            ctx.x[0] = (-4i64) as u64;
            return RESUME;
        }
        if THREADS[index].state == ST_EXITED {
            ctx.x[0] = THREADS[index].exit_code as u64;
            THREADS[index].state = ST_EMPTY;
            return RESUME;
        }
        THREADS[index].waiter_slot = slot as u8;
        if self_index == 0 {
            THREADS[index].waiter = WAITER_MAIN;
            MAIN_FUTEX[slot] = 0;
            MAIN_BLOCKED[slot].store(true, Ordering::Release);
        } else {
            THREADS[index].waiter = (self_index - 1) as i16;
            THREADS[(self_index - 1) as usize].state = ST_BLOCKED;
        }
    }
    BLOCK
}

struct LogWait {
    slot: u8,
    index: u8,
    live: bool,
}

const LOG_WAIT_N: usize = 32;
static mut LOG_WAIT: [LogWait; LOG_WAIT_N] =
    [const { LogWait { slot: 0, index: 0, live: false } }; LOG_WAIT_N];

fn log_publish(ptr: u64, len: u64) -> u64 {
    if crate::percpu::slot() != LOG_SLOT {
        return (-4i64) as u64;
    }
    if len == 0 || len > 96 {
        return (-4i64) as u64;
    }
    if !crate::mmu::user_slice_ok(ptr, len) {
        return (-2i64) as u64;
    }
    let mut raw = [0u8; 96];
    let n = len as usize;
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, raw.as_mut_ptr(), n);
    }
    crate::mmu::set_pan(true);
    crate::logger::fan_publish(&raw[..n]);
    wake_log_readers();
    0
}

fn log_follow() -> u64 {
    let ctx = unsafe { &mut *crate::percpu::current() };
    let cursor_ptr = ctx.x[0];
    let buf = ctx.x[1];
    let cap = ctx.x[2];
    let drops_ptr = ctx.x[3];
    let nonblock = ctx.x[4] == 1;
    if (cursor_ptr & 7) != 0 || !crate::mmu::user_write_ok(cursor_ptr, 8) {
        ctx.x[0] = (-2i64) as u64;
        return RESUME;
    }
    if cap == 0 || cap > 96 || !crate::mmu::user_write_ok(buf, cap) {
        ctx.x[0] = if cap == 0 || cap > 96 {
            (-4i64) as u64
        } else {
            (-2i64) as u64
        };
        return RESUME;
    }
    if drops_ptr != 0 && ((drops_ptr & 7) != 0 || !crate::mmu::user_write_ok(drops_ptr, 8)) {
        ctx.x[0] = (-2i64) as u64;
        return RESUME;
    }
    let cursor = read_user_u64(cursor_ptr);
    if let Some(line) = crate::logger::fan_read(cursor) {
        return commit_fan(ctx, cursor_ptr, buf, cap, drops_ptr, &line);
    }
    if nonblock {
        ctx.x[0] = 0;
        return RESUME;
    }
    let _guard = SCHED.lock();
    let cursor = read_user_u64(cursor_ptr);
    if let Some(line) = crate::logger::fan_read(cursor) {
        return commit_fan(ctx, cursor_ptr, buf, cap, drops_ptr, &line);
    }
    if !enqueue_log_waiter() {
        ctx.x[0] = (-11i64) as u64;
        return RESUME;
    }
    block_current_locked();
    BLOCK
}

fn commit_fan(
    ctx: &mut TaskContext,
    cursor_ptr: u64,
    buf: u64,
    cap: u64,
    drops_ptr: u64,
    line: &crate::logger::FanLine,
) -> u64 {
    let n = line.len.min(cap as usize);
    if n == 0 {
        ctx.x[0] = 0;
        return RESUME;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(line.bytes.as_ptr(), buf as *mut u8, n);
        core::ptr::write_volatile(cursor_ptr as *mut u64, line.next);
        if drops_ptr != 0 {
            core::ptr::write_volatile(drops_ptr as *mut u64, line.dropped);
        }
    }
    crate::mmu::set_pan(true);
    ctx.x[0] = n as u64;
    RESUME
}

fn enqueue_log_waiter() -> bool {
    let slot = crate::percpu::slot() as u8;
    let index = crate::percpu::thread_index() as u8;
    unsafe {
        let mut i = 0;
        while i < LOG_WAIT_N {
            if !LOG_WAIT[i].live {
                LOG_WAIT[i] = LogWait {
                    slot,
                    index,
                    live: true,
                };
                return true;
            }
            i += 1;
        }
    }
    false
}

fn wake_log_readers() {
    let _guard = SCHED.lock();
    unsafe {
        let mut i = 0;
        while i < LOG_WAIT_N {
            if LOG_WAIT[i].live {
                let slot = LOG_WAIT[i].slot as usize;
                let index = LOG_WAIT[i].index as u64;
                LOG_WAIT[i].live = false;
                wake_locked(slot, index, 0);
            }
            i += 1;
        }
    }
}

fn read_user_u64(addr: u64) -> u64 {
    crate::mmu::set_pan(false);
    let value = unsafe { core::ptr::read_volatile(addr as *const u64) };
    crate::mmu::set_pan(true);
    value
}

fn read_user_u32(addr: u64) -> u32 {
    crate::mmu::set_pan(false);
    let value = unsafe { core::ptr::read_volatile(addr as *const u32) };
    crate::mmu::set_pan(true);
    value
}

fn futex_wait(addr: u64, expected: u64) -> u64 {
    let ctx = unsafe { &mut *crate::percpu::current() };
    if (addr & 3) != 0 || !crate::mmu::user_slice_ok(addr, 4) {
        ctx.x[0] = (-2i64) as u64;
        return RESUME;
    }
    let _guard = SCHED.lock();
    if read_user_u32(addr) != expected as u32 {
        ctx.x[0] = 1;
        return RESUME;
    }
    let index = crate::percpu::thread_index();
    let slot = crate::percpu::slot();
    unsafe {
        if index == 0 {
            MAIN_FUTEX[slot] = addr;
            MAIN_BLOCKED[slot].store(true, Ordering::Release);
        } else {
            let mine = (index - 1) as usize;
            THREADS[mine].state = ST_BLOCKED;
            THREADS[mine].futex_addr = addr;
            THREADS[mine].futex_next = FUTEX_HEAD;
            FUTEX_HEAD = mine as i16;
        }
    }
    ctx.x[0] = 0;
    BLOCK
}

fn futex_wake(addr: u64, n: u64) -> u64 {
    let ctx = unsafe { &mut *crate::percpu::current() };
    if (addr & 3) != 0 || !crate::mmu::user_slice_ok(addr, 4) {
        ctx.x[0] = (-2i64) as u64;
        return RESUME;
    }
    let mut woke = 0u64;
    let limit = if n == 0 { 1 } else { n };
    let _guard = SCHED.lock();
    unsafe {
        let mut slot = 0;
        while slot < TASK_SLOTS && woke < limit {
            if MAIN_BLOCKED[slot].load(Ordering::Acquire) && MAIN_FUTEX[slot] == addr {
                MAIN_FUTEX[slot] = 0;
                (&mut *main_ctx(slot)).x[0] = 0;
                MAIN_BLOCKED[slot].store(false, Ordering::Release);
                woke += 1;
                sev();
            }
            slot += 1;
        }
        let mut prev = WAITER_NONE;
        let mut cur = FUTEX_HEAD;
        while cur >= 0 && woke < limit {
            let index = cur as usize;
            let next = THREADS[index].futex_next;
            if THREADS[index].futex_addr == addr && THREADS[index].state == ST_BLOCKED {
                if prev < 0 {
                    FUTEX_HEAD = next;
                } else {
                    THREADS[prev as usize].futex_next = next;
                }
                THREADS[index].futex_next = WAITER_NONE;
                THREADS[index].task.ctx.x[0] = 0;
                THREADS[index].state = ST_RUNNABLE;
                queue_push(cur);
                woke += 1;
                sev();
            } else {
                prev = cur;
            }
            cur = next;
        }
    }
    ctx.x[0] = woke;
    RESUME
}
