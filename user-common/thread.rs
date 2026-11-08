// User threads and a futex mutex. Included by the EL0 programs.

use core::sync::atomic::{AtomicU32, Ordering};

pub const THREAD_CREATE: u64 = 34;
pub const THREAD_EXIT: u64 = 35;
pub const THREAD_JOIN: u64 = 36;
pub const THREAD_TRY_JOIN: u64 = 44;
pub const FUTEX_WAIT: u64 = 37;
pub const FUTEX_WAKE: u64 = 38;

fn sys(num: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "svc #0",
            in("x8") num,
            inout("x0") a0 => ret,
            in("x1") a1,
            in("x2") a2,
            in("x3") a3,
            in("x4") a4,
            in("x5") a5,
        );
    }
    ret as i64
}

pub fn spawn(entry: extern "C" fn(u64), stack: u64, arg: u64) -> i64 {
    sys(THREAD_CREATE, entry as u64, stack, arg, 0, 0, 0)
}

pub fn exit(code: u64) -> ! {
    let _ = sys(THREAD_EXIT, code, 0, 0, 0, 0, 0);
    loop {}
}

pub fn join(tid: i64) -> i64 {
    sys(THREAD_JOIN, tid as u64, 0, 0, 0, 0, 0)
}

pub fn try_join(tid: i64) -> i64 {
    sys(THREAD_TRY_JOIN, tid as u64, 0, 0, 0, 0, 0)
}

pub fn futex_wait(word: &AtomicU32, expected: u32) -> i64 {
    sys(
        FUTEX_WAIT,
        word as *const AtomicU32 as u64,
        expected as u64,
        0,
        0,
        0,
        0,
    )
}

pub fn futex_wake(word: &AtomicU32, n: u32) -> i64 {
    sys(
        FUTEX_WAKE,
        word as *const AtomicU32 as u64,
        n as u64,
        0,
        0,
        0,
        0,
    )
}

pub fn mutex_lock(word: &AtomicU32) {
    while word
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        let _ = futex_wait(word, 1);
    }
}

pub fn mutex_unlock(word: &AtomicU32) {
    word.store(0, Ordering::Release);
    let _ = futex_wake(word, 1);
}
