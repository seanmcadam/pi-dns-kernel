//! Network owner.
//!
//! smoltcp and the NIC are not reentrant, and both virtio-net and the Pi 4
//! GENET driver expose one transmit/receive queue here. Splitting smoltcp
//! into per-socket interfaces would make those stacks fight over one MAC
//! and one address. Virtio multiqueue is not what the GENET driver speaks.
//!
//! One core owns the device. Core 1 takes that role once the stack is up.
//! It sleeps in `wfi` until a virtio used-ring IRQ, an SGI from a core that
//! queued a socket job, or the timer (which is also how `poll_delay` is
//! serviced). When none of those are pending it runs one user thread, so
//! the NIC stays the highest priority and the core is not a dedicated poll
//! loop. Until the handoff, and when EL2 leaves the machine with one core,
//! core 0 owns it and drives from the kernel loop on the same kick and
//! deadline. Every other core enqueues a socket request on a lock-free
//! multi-producer ring and sleeps. The owner runs the request, copies the
//! result into the caller's user buffer, and wakes the thread. A request
//! that has to wait for the wire stays queued; the thread stays asleep
//! until the next drive finishes it.
//!
//! The IRQ handler only sets `KICK` (and acknowledges the device). It does
//! not take `SCHED` or any other lock. The owner rechecks `KICK`, the job
//! ring, and the deadline with IRQs masked before `wfi`, so a kick that
//! arrives after the check and is EOI'd before the sleep cannot be lost:
//! with DAIF.I set the interrupt stays pending and `wfi` returns.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
#[cfg(feature = "board-pi4")]
use core::sync::atomic::AtomicU32;

use smoltcp::iface::SocketHandle;

const JOBS_N: usize = 64;
const Q_N: usize = 64;

/// Listening sockets reserved on one TCP port. Sixteen telnet connections
/// stay open, and a spare listener is still there for the next handshake.
pub const TCP_LISTENERS: usize = 18;
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
    pub listen: [Option<SocketHandle>; TCP_LISTENERS],
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
static KICK: AtomicBool = AtomicBool::new(false);
static DEADLINE_MS: AtomicU64 = AtomicU64::new(0);
static BUSY_TICKS: AtomicU64 = AtomicU64::new(0);
static IDLE_TICKS: AtomicU64 = AtomicU64::new(0);
/// Pi 4 only. Set once a zero `poll_delay` has already been given an
/// immediate second look. The next zero sleeps instead of spinning.
#[cfg(feature = "board-pi4")]
static ZERO_DELAY_HELD: AtomicBool = AtomicBool::new(false);
/// Last rx/tx counts, so a poll that moved no frame can sleep longer
/// than one that just did. Pi 4 only.
#[cfg(feature = "board-pi4")]
static SEEN_RX: AtomicU32 = AtomicU32::new(0);
#[cfg(feature = "board-pi4")]
static SEEN_TX: AtomicU32 = AtomicU32::new(0);
/// Millisecond of the last poll that moved a frame or still had one queued.
#[cfg(feature = "board-pi4")]
static LAST_FRAME_MS: AtomicU64 = AtomicU64::new(0);
/// A poll that sent or received something may run again on the next tick.
/// RX still wakes the owner, so this is only the backup.
#[cfg(feature = "board-pi4")]
const ACTIVE_POLL_MS: u64 = 10;
/// How long after the last frame the short backup stays in force. One
/// empty poll is not an idle link: at 50 qps the gap is 20 ms. Treating
/// that gap as quiet slept 200 ms, which was the 187 ms median in the
/// first window of the PR59 stress run, and the ring then fell behind.
#[cfg(feature = "board-pi4")]
const IDLE_HOLD_MS: u64 = 100;
/// Nothing has moved for `IDLE_HOLD_MS`. Sleeping a full timer tick was
/// most of the idle CPU: each poll cost about as much as the 10 ms cap,
/// so `net_busy_ms` tracked wall clock. 200 ms still bounds a missed RX
/// IRQ on a link that is actually idle, well inside the 2 s gratuitous
/// ARP, and the owner spends that wait in `wfi`.
#[cfg(feature = "board-pi4")]
const QUIET_POLL_MS: u64 = 200;
/// Millisecond when the current `drive` entered the stack. Zero when the
/// owner is not inside it. The Pi 4 main loop halts if this stays set.
#[cfg(feature = "board-pi4")]
static DRIVE_ENTER: AtomicU64 = AtomicU64::new(0);
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
        listen: [None; TCP_LISTENERS],
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

    fn pending(&self) -> bool {
        let pos = self.head.load(Ordering::Relaxed);
        let cell = &self.cells[pos % Q_N];
        cell.seq.load(Ordering::Acquire) == pos.wrapping_add(1)
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
    // Core 1 waits in WFI. An SGI wakes that; `sev` covers a WFE.
    sev();
    crate::gic::send_sgi(crate::gic::SGI_NET, 1);
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
    crate::board::retarget_nic(1);
    // Drive once on the new owner even if the last deadline was far away.
    note_kick();
    true
}

fn owner_is_me() -> bool {
    TURN.load(Ordering::Acquire) == crate::percpu::id()
}

pub fn polling_here() -> bool {
    READY.load(Ordering::Acquire) && owner_is_me()
}

/// Device IRQ or socket-queue IPI. Atomic only. Safe from the IRQ handler.
pub fn note_kick() {
    KICK.store(true, Ordering::Release);
}

/// `note_kick`, and if this core is not the owner, SGI that core.
///
/// A GENET interrupt lands on whichever CPU the SPI targets. Until the
/// line is moved, that is CPU 0, while core 1 owns the NIC and is in
/// `wfi`. Setting `KICK` on the other core does not wake it.
pub fn wake_owner() {
    note_kick();
    let owner = TURN.load(Ordering::Acquire);
    if owner != crate::percpu::id() {
        crate::gic::send_sgi(crate::gic::SGI_NET, owner);
    }
}

pub fn kick_pending() -> bool {
    KICK.load(Ordering::Acquire)
}

/// Milliseconds inside `drive` and inside `wfi`.
///
/// The counters underneath are CNTPCT deltas. The printed value is
/// `ticks * 1000 / CNTFRQ`. A zero poll delay while the link is down
/// would otherwise spin the owner inside `drive`.
pub fn owner_time() -> (u64, u64) {
    let freq = crate::timer::frequency().max(1);
    (
        BUSY_TICKS.load(Ordering::Relaxed).saturating_mul(1000) / freq,
        IDLE_TICKS.load(Ordering::Relaxed).saturating_mul(1000) / freq,
    )
}

/// How long the current `drive` has been inside the stack, or 0 if it is
/// not. A multi-second age means `poll` did not return (the RX callback
/// is still running) and the owner will never reach `wfi`.
#[cfg(feature = "board-pi4")]
pub fn drive_age(now: u64) -> u64 {
    let entered = DRIVE_ENTER.load(Ordering::Acquire);
    if entered == 0 {
        0
    } else {
        now.saturating_sub(entered)
    }
}

fn work_pending() -> bool {
    KICK.load(Ordering::Acquire)
        || crate::timer::millis() >= DEADLINE_MS.load(Ordering::Acquire)
        || QUEUE.pending()
}

/// Drive if this core owns the NIC and a kick, a job, or `poll_delay` is due.
pub fn pump() {
    if !polling_here() || !work_pending() {
        return;
    }
    drive();
}

/// NIC first. Then one user thread. Then sleep until an interrupt.
///
/// The masked recheck closes the lost-wakeup window: an IRQ that is taken
/// and EOI'd after the first check is visible in `KICK` or stays pending
/// across `wfi` because IRQs are masked.
pub fn owner_idle() {
    if !polling_here() {
        return;
    }
    #[cfg(feature = "board-pi4")]
    crate::act::note_core(crate::percpu::id());
    if work_pending() {
        drive();
        return;
    }
    if crate::task::schedule_one() {
        return;
    }
    crate::interrupts::mask();
    if !work_pending() {
        let t0 = crate::timer::get_ticks();
        crate::percpu::set_asleep(true);
        unsafe {
            core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
        }
        // Refresh the checkpoint before clearing the idle flag. The timer
        // runs as soon as IRQs are unmasked, and it would otherwise see a
        // stale mark on a core that was only asleep.
        #[cfg(feature = "board-pi4")]
        crate::act::note_core(crate::percpu::id());
        crate::percpu::set_asleep(false);
        IDLE_TICKS.fetch_add(crate::timer::get_ticks().wrapping_sub(t0), Ordering::Relaxed);
    }
    crate::interrupts::unmask();
}

fn notify_owner() {
    wake_owner();
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
    // Drop a kick that was already visible. One that arrives during the
    // poll is stored again and the owner loop drives once more.
    KICK.store(false, Ordering::Release);
    let t0 = crate::timer::get_ticks();
    #[cfg(feature = "board-pi4")]
    DRIVE_ENTER.store(crate::timer::millis(), Ordering::Release);
    unsafe {
        let state_slot = core::ptr::addr_of_mut!(STATE);
        if let Some(state) = (*state_slot).as_mut() {
            super::poll_device(state);
            crate::user_fd::recycle_all(&mut state.stack);
            pull();
            run_pending(&mut state.stack);
            pull();
            run_pending(&mut state.stack);
            arm_deadline(state);
        }
    }
    #[cfg(feature = "board-pi4")]
    DRIVE_ENTER.store(0, Ordering::Release);
    BUSY_TICKS.fetch_add(crate::timer::get_ticks().wrapping_sub(t0), Ordering::Relaxed);
    IN_STACK.store(false, Ordering::Release);
}

/// A zero delay means the stack wants another look.
///
/// While the link is down that look cannot send, and 50 ms is what lets
/// the owner reach `wfi`. Once the link is up, virt and Pi 5 poll again
/// immediately so a discover or a queued datagram is not held.
///
/// On Pi 4 that immediate retry is allowed once (`deadline == now`). A
/// socket that stays at `PollAt::Now` would otherwise ask again on every
/// return and the owner would never `wfi`. The retry's own transmit does
/// not cancel the following sleep: TX completion does not kick. RX still
/// arrives through the GENET line.
fn zero_delay_deadline(now: u64, quiet: bool) -> u64 {
    if !crate::board::nic_link() {
        #[cfg(feature = "board-pi4")]
        ZERO_DELAY_HELD.store(false, Ordering::Relaxed);
        return now.saturating_add(50);
    }
    #[cfg(feature = "board-pi4")]
    {
        if ZERO_DELAY_HELD.swap(true, Ordering::Relaxed) {
            let pause = if quiet { QUIET_POLL_MS } else { ACTIVE_POLL_MS };
            return now.saturating_add(pause);
        }
    }
    let _ = quiet;
    now
}

/// Pi 4: no frame has moved for `IDLE_HOLD_MS`. A single empty poll used
/// to take the long sleep immediately, so a steady query stream waited
/// out `QUIET_POLL_MS` whenever the RX line did not wake the owner.
#[cfg(feature = "board-pi4")]
fn frames_quiet(now: u64) -> bool {
    let (rx, tx) = crate::board::packet_counts();
    let prev_rx = SEEN_RX.swap(rx, Ordering::Relaxed);
    let prev_tx = SEEN_TX.swap(tx, Ordering::Relaxed);
    if prev_rx != rx || prev_tx != tx || crate::board::rx_pending() {
        LAST_FRAME_MS.store(now, Ordering::Relaxed);
        return false;
    }
    now.saturating_sub(LAST_FRAME_MS.load(Ordering::Relaxed)) >= IDLE_HOLD_MS
}

fn arm_deadline(state: &mut super::NetworkState) {
    let now = crate::timer::millis();
    #[cfg(feature = "board-pi4")]
    let quiet = frames_quiet(now);
    #[cfg(not(feature = "board-pi4"))]
    let quiet = false;
    let timestamp = smoltcp::time::Instant::from_millis(now as i64);
    let mut deadline = match state.stack.poll_delay(timestamp) {
        Some(delay) => {
            let ms = delay.total_millis();
            if ms == 0 {
                zero_delay_deadline(now, quiet)
            } else {
                #[cfg(feature = "board-pi4")]
                ZERO_DELAY_HELD.store(false, Ordering::Relaxed);
                now.saturating_add(ms)
            }
        }
        None => u64::MAX,
    };
    // The DHCP fallback is a kernel deadline, not a smoltcp timer. Without
    // this cap the owner sleeps through `timeout_ms` and never applies it.
    if let Some(dhcp) = state.dhcp.as_ref() {
        if !dhcp.finished {
            let at = dhcp.started_ms.saturating_add(dhcp.timeout_ms);
            if at < deadline {
                deadline = at;
            }
        }
    }
    if let Some(at) = super::next_announce_ms() {
        if at < deadline {
            deadline = at;
        }
    }
    // Pi 4 used to clip every sleep to 10 ms so a missed RX IRQ could not
    // wait out the 2 s gratuitous ARP. The line wakes the owner now, and
    // that clip made `drive` run on every timer tick. A quiet poll was
    // most of the tick, so `net_busy_ms` tracked wall clock while
    // `net_idle_ms` only counted the sliver inside `wfi`.
    //
    // Recent traffic keeps the short backup. A real idle streak uses
    // 200 ms. `deadline == now` is the one immediate retry and is left
    // alone unless the receive ring still holds a frame: sleeping then
    // is how the ring overruns when the GENET line does not wake us.
    // A past announce (ARP_NEXT still 0) is not left in the past; storing
    // it would make `work_pending` true on every return.
    #[cfg(feature = "board-pi4")]
    {
        let cap_ms = if quiet { QUIET_POLL_MS } else { ACTIVE_POLL_MS };
        let cap = now.saturating_add(cap_ms);
        if deadline > cap {
            deadline = cap;
        }
        if deadline < now {
            deadline = cap;
        }
    }
    // Virt and Pi 5 report no backlog. On Pi 4 a non-empty ring is polled
    // again immediately, including when the quiet cap would have slept.
    if crate::board::rx_pending() {
        deadline = now;
    }
    DEADLINE_MS.store(deadline, Ordering::Release);
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
        asid: crate::supervisor::asid(crate::percpu::slot()),
        user_ptr: 0,
        user_aux: 0,
        handle_raw: 0,
        listen: [None; TCP_LISTENERS],
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
    notify_owner();
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

/// Mark queued work for this process cancelled. Close jobs stay, so a
/// teardown that already submitted them still runs.
pub fn cancel_task(task: usize) {
    let mut id = 0;
    while id < JOBS_N {
        if JOBS[id].state.load(Ordering::Acquire) == QUEUED {
            unsafe {
                let body = &mut *JOBS[id].body.get();
                if body.task as usize == task
                    && body.op != OP_TCP_CLOSE
                    && body.op != OP_CANCEL
                {
                    body.op = OP_CANCEL;
                }
            }
        }
        id += 1;
    }
}

pub fn task_queued(task: usize) -> bool {
    let mut id = 0;
    while id < JOBS_N {
        if JOBS[id].state.load(Ordering::Acquire) == QUEUED {
            let queued = unsafe { (*JOBS[id].body.get()).task as usize == task };
            if queued {
                return true;
            }
        }
        id += 1;
    }
    false
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
