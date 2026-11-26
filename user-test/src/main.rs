#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("user.s"));

mod syscall {
    #![allow(dead_code)]
    include!("../../user-common/syscall.rs");
}

mod thread {
    include!("../../user-common/thread.rs");
}

const LOCK: u64 = 0x1_000A_0000;
const COUNTER: u64 = 0x1_000A_0008;
const ADDS: u64 = 1000;

#[no_mangle]
pub extern "C" fn futex_test() {
    unsafe {
        core::ptr::write_volatile(LOCK as *mut u32, 0);
        core::ptr::write_volatile(COUNTER as *mut u32, 0);
    }
    let word = unsafe { core::sync::atomic::AtomicU32::from_ptr(LOCK as *mut u32) };
    let mut id = 0u64;
    let mut started = [0i64; 4];
    while id < 4 {
        let top = 0x1_000A_2000 + id * 0x1000;
        started[id as usize] = thread::spawn(worker, top, ADDS);
        id += 1;
    }
    let mut n = 0;
    while n < 4 {
        if started[n] > 0 {
            let _ = thread::join(started[n]);
        }
        n += 1;
    }
    let total = unsafe { core::ptr::read_volatile(COUNTER as *const u32) };
    let mut msg = [0u8; 32];
    let prefix = b"futex counter ";
    let mut len = prefix.len();
    msg[..len].copy_from_slice(prefix);
    len += write_dec(&mut msg[len..], total as u64);
    let _ = log(&msg[..len]);
    let _ = word.load(core::sync::atomic::Ordering::Relaxed);
    exit(0);
}

extern "C" fn worker(adds: u64) {
    let word = unsafe { core::sync::atomic::AtomicU32::from_ptr(LOCK as *mut u32) };
    let mut n = 0;
    while n < adds {
        thread::mutex_lock(word);
        unsafe {
            let value = core::ptr::read_volatile(COUNTER as *const u32);
            let mut spin = 0;
            while spin < 32 {
                spin += 1;
            }
            core::ptr::write_volatile(COUNTER as *mut u32, value.wrapping_add(1));
        }
        thread::mutex_unlock(word);
        n += 1;
    }
    thread::exit(0);
}

fn write_dec(buf: &mut [u8], mut value: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if value == 0 {
        buf[0] = b'0';
        return 1;
    }
    while value > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    let digits = &tmp[i..];
    buf[..digits.len()].copy_from_slice(digits);
    digits.len()
}

fn log(bytes: &[u8]) -> i64 {
    syscall::sys(syscall::LOG, bytes.as_ptr() as u64, bytes.len() as u64, 0, 0, 0, 0)
}

fn exit(code: u64) -> ! {
    let _ = syscall::sys(syscall::EXIT, code, 0, 0, 0, 0, 0);
    loop {}
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    panic_rt::report_panic(b"boot", info);
    loop {}
}
