//! DNS resolver running at EL0. The kernel keeps the sockets, the NIC,
//! and storage. The web UI is a second EL0 task. This program asks for
//! sockets and config through syscalls.

#![no_std]
#![no_main]

extern crate alloc;

mod failover;
mod health;
mod server;

core::arch::global_asm!(include_str!("../../user-common/start.s"));

mod syscall {
    #![allow(dead_code)]
    include!("../../user-common/syscall.rs");
}

pub(crate) const HEAP_VA: usize = 0x1_000A_0000;
pub(crate) const HEAP_SIZE: usize = 1536 * 1024;
/// `udp_recv` alone takes about 1.5 KiB. Stacks grow down, so each worker
/// has 64 KiB and the kernel leaves the page under that 64 KiB unmapped.
/// The page above worker 0 is the main stack's overflow guard. The
/// allocator stops at the lowest guard.
pub(crate) const STACK_BYTES: usize = 64 * 1024;
pub(crate) const PAGE: usize = 4096;
pub(crate) const WORKERS: usize = 5;
pub(crate) const ALLOC_SIZE: usize = HEAP_SIZE - PAGE - WORKERS * (STACK_BYTES + PAGE);

mod thread {
    include!("../../user-common/thread.rs");
}

mod heap {
    include!("../../user-common/alloc.rs");
}

#[global_allocator]
static ALLOCATOR: heap::Heap = heap::Heap;

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    server::run();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    panic_rt::report_panic(b"dns", info);
    loop {}
}
