//! Guest shutdown and reboot.
//!
//! The HTTP task only stores the request. The poll loop applies it after a
//! short hardware-counter delay, so the response can leave the NIC first.
//! The board call stops or resets this guest. It does not target the host.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// 1 shutdown, 2 reboot. Zero means nothing is waiting.
static CMD: AtomicU8 = AtomicU8::new(0);
static FAIL: AtomicU8 = AtomicU8::new(0);
static AT: AtomicU64 = AtomicU64::new(0);

/// `0` idle, `1` shutdown pending, `2` reboot pending, `3` shutdown refused,
/// `4` reboot refused. Action `1` or `2` queues that request.
/// Only the HTTP task may call this. `1` means a request is already waiting.
pub fn request(action: u8) -> i64 {
    if crate::percpu::slot() != crate::task::HTTP_SLOT {
        return -3;
    }
    if action == 0 {
        return status() as i64;
    }
    if action != 1 && action != 2 {
        return -4;
    }
    if CMD
        .compare_exchange(0, action, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return 1;
    }
    FAIL.store(0, Ordering::Release);
    AT.store(
        crate::timer::millis().saturating_add(300),
        Ordering::Release,
    );
    0
}

pub fn status() -> u8 {
    match CMD.load(Ordering::Acquire) {
        1 => 1,
        2 => 2,
        _ => match FAIL.load(Ordering::Acquire) {
            1 => 3,
            2 => 4,
            _ => 0,
        },
    }
}

pub fn poll() {
    let cmd = CMD.load(Ordering::Acquire);
    let at = AT.load(Ordering::Acquire);
    if cmd == 0 || at == 0 || crate::timer::millis() < at {
        return;
    }
    if CMD.swap(0, Ordering::AcqRel) == 0 {
        return;
    }
    let reset = cmd == 2;
    crate::console::write_str(if reset {
        "power: reboot\r\n"
    } else {
        "power: shutdown\r\n"
    });
    if crate::board::power(reset) {
        FAIL.store(cmd, Ordering::Release);
        crate::console::write_str("power: refused\r\n");
    }
}
