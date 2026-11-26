//! HTTP dashboard running at EL0. The kernel keeps the sockets, the NIC,
//! storage, and the config file. This task has its own pages and ASID.

#![no_std]
#![no_main]

extern crate alloc;
extern crate version;

mod server;

core::arch::global_asm!(include_str!("../../user-common/start.s"));

mod syscall {
    #![allow(dead_code)]
    include!("../../user-common/syscall.rs");
}

pub(crate) const HEAP_VA: usize = 0x1_000A_0000;
pub(crate) const HEAP_SIZE: usize = 1536 * 1024;
/// `serve` takes about 3 KiB and `read_config_file` puts another 8 KiB on
/// the same stack. Stacks grow down, so each worker has 64 KiB and the
/// kernel leaves the page under that 64 KiB unmapped. One more unmapped
/// page sits above worker 0, which is the main stack's overflow guard.
/// The allocator stops at the lowest guard and never receives those pages.
pub(crate) const STACK_BYTES: usize = 64 * 1024;
pub(crate) const PAGE: usize = 4096;
pub(crate) const WORKERS: usize = 4;
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
    panic_rt::report_panic(b"http", info);
    loop {}
}
