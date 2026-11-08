//! Network owner.
//!
//! smoltcp and the NIC are not reentrant, and both virtio-net and the Pi 4
//! GENET driver expose one transmit/receive queue here. Splitting smoltcp
//! into per-socket interfaces would make those stacks fight over one MAC
//! and one address. Virtio multiqueue is not what the GENET driver speaks.
//!
//! One core owns the device. Core 1 takes that role once the stack is up
//! and then only polls. Until then, and when EL2 leaves the machine with
//! one core, core 0 owns it and polls from the kernel loop. Every other
//! core enqueues a socket request on a lock-free multi-producer ring and
//! sleeps. The owner runs the request, copies the result into the caller's
//! user buffer, and wakes the thread. A request that has to wait for the
//! wire stays queued; the thread stays asleep until the next poll finishes it.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use smoltcp::iface::SocketHandle;

const JOBS_N: usize = 32;
const Q_N: usize = 32;
const FREE: u8 = 0;
const QUEUED: u8 = 1;
const DONE: u8 = 2;

pub const OP_CANCEL: u8 = 0;
pub const OP_UDP_RECV: u8 = 1;
pub const OP_UDP_SEND: u8 = 2;
pub const OP_TCP_RECV: u8 = 3;
pub const OP_TCP_SEND: u8 = 4;
pub const OP_TCP_ACCEPT: u8 = 5;
pub const OP_TCP_CLOSE: u8 = 6;
pub const OP_UDP_BIND: u8 = 7;
pub const OP_TCP_LISTEN: u8 = 8;

pub enum Step {
    Pending,
    Done(i64),
}

pub enum Wait {
    Ready(u64),
    Sleep,
}

pub struct Body {
    pub op: u8,
    pub task: u8,
    pub fd: u8,
    pub conn_fd: u8,
    pub port: u16,
    pub cap: u32,
    pub len: u32,
    pub copy_len: u32,
    pub aux_len: u32,
    pub ip: u32,
    pub gen: u32,
    pub asid: u64,
    pub user_ptr: u64,
    pub user_aux: u64,
    pub handle_raw: usize,
    pub listen: [Option<SocketHandle>; 4],
    pub addr: [u8; 8],
    pub buf: [u8; 4096],
}

struct Job {
    state: AtomicU8,
    result: AtomicI64,
    waiter_tid: AtomicU64,
    waiter_slot: AtomicU8,
    body: UnsafeCell<Body>,
}

unsafe impl Sync for Job {}

struct Cell {
    seq: AtomicUsize,
    val: AtomicUsize,
}

struct Queue {
    tail: AtomicUsize,
    head: AtomicUsize,
    cells: [Cell; Q_N],
}

static JOBS: [Job; JOBS_N] = [EMPTY_JOB; JOBS_N];
static QUEUE: Queue = Queue::new();
static POSTED: [AtomicUsize; 4] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];
static IN_STACK: AtomicBool = AtomicBool::new(false);
static READY: AtomicBool = AtomicBool::new(false);
static TURN: AtomicUsize = AtomicUsize::new(0);
static mut STATE: Option<super::NetworkState> = None;
static mut PENDING: [usize; JOBS_N] = [0; JOBS_N];
static mut PENDING_LEN: usize = 0;

const EMPTY_JOB: Job = Job {
    state: AtomicU8::new(FREE),
    result: AtomicI64::new(0),
    waiter_tid: AtomicU64::new(u64::MAX),
    waiter_slot: AtomicU8::new(0),
    body: UnsafeCell::new(Body {
        op: 0,
        task: 0,
        fd: 0,
        conn_fd: 0xff,
        port: 0,
        cap: 0,
        len: 0,
        copy_len: 0,
        aux_len: 0,
        ip: 0,
        gen: 0,
        asid: 0,
        user_ptr: 0,
        user_aux: 0,
        handle_raw: 0,
        listen: [None, None, None, None],
        addr: [0; 8],
        buf: [0; 4096],
    }),
};

impl Queue {
    const fn new() -> Self {
        const CELL: Cell = Cell {
            seq: AtomicUsize::new(0),
            val: AtomicUsize::new(0),
        };
        let mut cells = [CELL; Q_N];
        let mut i = 0;
        while i < Q_N {
            cells[i].seq = AtomicUsize::new(i);
            i += 1;
        }
        Self {
            tail: AtomicUsize::new(0),
            head: AtomicUsize::new(0),
            cells,
        }
    }

    fn push(&self, value: usize) -> bool {
        let mut pos = self.tail.load(Ordering::Relaxed);
        loop {
            let cell = &self.cells[pos % Q_N];
            let seq = cell.seq.load(Ordering::Acquire);
            let dif = seq as isize - pos as isize;
            if dif == 0 {
                match self.tail.compare_exchange_weak(
                    pos,
                    pos.wrapping_add(1),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        cell.val.store(value, Ordering::Relaxed);
                        cell.seq.store(pos.wrapping_add(1), Ordering::Release);
                        return true;
                    }
                    Err(next) => pos = next,
                }
            } else if dif < 0 {
                return false;
            } else {
                pos = self.tail.load(Ordering::Relaxed);
            }
        }
    }

    fn pop(&self) -> Option<usize> {
        let pos = self.head.load(Ordering::Relaxed);
        let cell = &self.cells[pos % Q_N];
        let seq = cell.seq.load(Ordering::Acquire);
        if seq != pos.wrapping_add(1) {
            return None;
        }
        let value = cell.val.load(Ordering::Relaxed);
        cell.seq.store(pos.wrapping_add(Q_N), Ordering::Release);
        self.head.store(pos.wrapping_add(1), Ordering::Relaxed);
        Some(value)
    }
}

pub fn install(state: super::NetworkState) {
    while IN_STACK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    unsafe {
        STATE = Some(state);
    }
    READY.store(true, Ordering::Release);
    IN_STACK.store(false, Ordering::Release);
}

pub fn try_become_owner() -> bool {
    if crate::percpu::id() != 1 || !READY.load(Ordering::Acquire) {
        return false;
    }
    while IN_STACK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }
    TURN.store(1, Ordering::Release);
    IN_STACK.store(false, Ordering::Release);
    true
}

fn owner_is_me() -> bool {
    TURN.load(Ordering::Acquire) == crate::percpu::id()
}

pub fn polling_here() -> bool {
    READY.load(Ordering::Acquire) && owner_is_me()
}

pub fn drive() {
    if !READY.load(Ordering::Acquire) || !owner_is_me() {
        return;
    }
    if IN_STACK
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    if !owner_is_me() {
        IN_STACK.store(false, Ordering::Release);
        return;
    }
    unsafe {
        if let Some(state) = STATE.as_mut() {
            super::poll_device(state);
            crate::user_fd::recycle_all(&mut state.stack);
            pull();
            run_pending(&mut state.stack);
            pull();
            run_pending(&mut state.stack);
        }
    }
    IN_STACK.store(false, Ordering::Release);
}

fn pull() {
    while let Some(id) = QUEUE.pop() {
        unsafe {
            if PENDING_LEN < JOBS_N {
                PENDING[PENDING_LEN] = id;
                PENDING_LEN += 1;
            }
        }
    }
}

fn run_pending(stack: &mut super::NetworkStack) {
    let mut i = 0;
    unsafe {
        while i < PENDING_LEN {
            let id = PENDING[i];
            if JOBS[id].state.load(Ordering::Acquire) == FREE {
                PENDING_LEN -= 1;
                PENDING[i] = PENDING[PENDING_LEN];
                continue;
            }
            match crate::user_fd::perform(id, stack) {
                Step::Pending => i += 1,
                Step::Done(result) => {
                    PENDING_LEN -= 1;
                    PENDING[i] = PENDING[PENDING_LEN];
                    complete(id, result);
                }
            }
        }
    }
}

fn complete(id: usize, result: i64) {
    let result = if copy_out(id) { result } else { -2 };
    crate::task::with_sched(|| {
        let job = &JOBS[id];
        let tid = job.waiter_tid.load(Ordering::Relaxed);
        if tid != u64::MAX {
            let slot = job.waiter_slot.load(Ordering::Relaxed) as usize;
            free_job(id);
            crate::task::wake_locked(slot, tid, result as u64);
        } else {
            job.result.store(result, Ordering::Relaxed);
            job.state.store(DONE, Ordering::Release);
        }
    });
}

fn copy_out(id: usize) -> bool {
    let body = unsafe { &mut *JOBS[id].body.get() };
    if body.copy_len == 0 && body.aux_len == 0 {
        return true;
    }
    let task = body.task as usize;
    let asid = body.asid;
    let ptr = body.user_ptr;
    let aux = body.user_aux;
    let n = body.copy_len as usize;
    let aux_n = body.aux_len as usize;
    let old = ttbr0();
    crate::mmu::use_user_slot(task, asid);
    let mut ok = true;
    crate::mmu::set_pan(false);
    if n > 0 {
        if crate::mmu::user_write_ok(ptr, n as u64) {
            unsafe {
                core::ptr::copy_nonoverlapping(body.buf.as_ptr(), ptr as *mut u8, n);
            }
        } else {
            ok = false;
        }
    }
    if ok && aux_n > 0 {
        if crate::mmu::user_write_ok(aux, aux_n as u64) {
            unsafe {
                core::ptr::copy_nonoverlapping(body.addr.as_ptr(), aux as *mut u8, aux_n);
            }
        } else {
            ok = false;
        }
    }
    crate::mmu::set_pan(true);
    set_ttbr0(old);
    ok
}

fn ttbr0() -> u64 {
    let old: u64;
    unsafe {
        core::arch::asm!("mrs {old}, ttbr0_el1", old = out(reg) old, options(nomem, nostack));
    }
    old
}

fn set_ttbr0(old: u64) {
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {old}",
            "isb",
            old = in(reg) old,
            options(nostack),
        );
    }
}

pub fn submit(op: u8, fill: impl FnOnce(&mut Body)) -> Option<usize> {
    let id = alloc_job()?;
    let body = unsafe { &mut *JOBS[id].body.get() };
    *body = Body {
        op,
        task: crate::percpu::slot() as u8,
        fd: 0,
        conn_fd: 0xff,
        port: 0,
        cap: 0,
        len: 0,
        copy_len: 0,
        aux_len: 0,
        ip: 0,
        gen: 0,
        asid: crate::percpu::slot() as u64 + 1,
        user_ptr: 0,
        user_aux: 0,
        handle_raw: 0,
        listen: [None, None, None, None],
        addr: [0; 8],
        buf: [0; 4096],
    };
    fill(body);
    JOBS[id].result.store(0, Ordering::Relaxed);
    JOBS[id].waiter_tid.store(u64::MAX, Ordering::Relaxed);
    if !QUEUE.push(id) {
        free_job(id);
        return None;
    }
    let cpu = crate::percpu::id();
    if cpu < POSTED.len() {
        POSTED[cpu].store(id + 1, Ordering::Release);
    }
    sev();
    Some(id)
}

pub fn take_posted() -> Option<usize> {
    let cpu = crate::percpu::id();
    if cpu >= POSTED.len() {
        return None;
    }
    let value = POSTED[cpu].swap(0, Ordering::Acquire);
    if value == 0 {
        None
    } else {
        Some(value - 1)
    }
}

pub fn wait(id: usize) -> Wait {
    if owner_is_me() {
        drive();
    }
    crate::task::with_sched(|| {
        let job = &JOBS[id];
        if job.state.load(Ordering::Acquire) == DONE {
            let result = job.result.load(Ordering::Relaxed) as u64;
            free_job(id);
            Wait::Ready(result)
        } else {
            job.waiter_slot
                .store(crate::percpu::slot() as u8, Ordering::Relaxed);
            job.waiter_tid
                .store(crate::percpu::thread_index(), Ordering::Relaxed);
            crate::task::block_current_locked();
            Wait::Sleep
        }
    })
}

pub fn body(id: usize) -> &'static mut Body {
    unsafe { &mut *JOBS[id].body.get() }
}

pub fn handle_of(raw: usize) -> SocketHandle {
    unsafe { core::mem::transmute(raw) }
}

pub fn handle_raw(handle: SocketHandle) -> usize {
    unsafe { core::mem::transmute(handle) }
}

pub fn cancel_fd(task: usize, fd: usize, except: usize) {
    unsafe {
        let mut i = 0;
        while i < PENDING_LEN {
            let id = PENDING[i];
            if id != except {
                let body = &*JOBS[id].body.get();
                if body.task as usize == task && body.fd as usize == fd {
                    (*JOBS[id].body.get()).op = OP_CANCEL;
                }
            }
            i += 1;
        }
    }
}

fn alloc_job() -> Option<usize> {
    let mut id = 0;
    while id < JOBS_N {
        if JOBS[id]
            .state
            .compare_exchange(FREE, QUEUED, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return Some(id);
        }
        id += 1;
    }
    None
}

fn free_job(id: usize) {
    JOBS[id].waiter_tid.store(u64::MAX, Ordering::Relaxed);
    JOBS[id].state.store(FREE, Ordering::Release);
}

fn sev() {
    unsafe {
        core::arch::asm!("sev", options(nomem, nostack, preserves_flags));
    }
}
