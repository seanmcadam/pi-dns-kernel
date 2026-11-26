// User threads and a futex mutex. Included by the EL0 programs.
// The crate root includes `syscall.rs` as `mod syscall`.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::syscall::{
    sys, FUTEX_WAIT, FUTEX_WAKE, THREAD_CREATE, THREAD_EXIT, THREAD_JOIN, THREAD_TRY_JOIN,
};

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
