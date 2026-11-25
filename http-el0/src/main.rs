//! HTTP dashboard running at EL0. The kernel keeps the sockets, the NIC,
//! storage, and the config file. This task has its own pages and ASID.

#![no_std]
#![no_main]

extern crate alloc;
extern crate version;

mod server;

use core::alloc::{GlobalAlloc, Layout};

core::arch::global_asm!(include_str!("../../user-common/start.s"));

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
const HEADER: usize = 16;

mod thread {
    include!("../../user-common/thread.rs");
}

static HEAP_LOCK: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

#[repr(C)]
struct Node {
    size: usize,
    next: *mut Node,
}

static mut FREE: *mut Node = core::ptr::null_mut();

struct Heap;

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        thread::mutex_lock(&HEAP_LOCK);
        init_heap();
        let ptr = alloc_block(layout);
        thread::mutex_unlock(&HEAP_LOCK);
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        thread::mutex_lock(&HEAP_LOCK);
        free_block(ptr, alloc_size(layout));
        thread::mutex_unlock(&HEAP_LOCK);
    }
}

#[global_allocator]
static ALLOCATOR: Heap = Heap;

fn alloc_size(layout: Layout) -> usize {
    let size = layout.size().max(1);
    align_up(size, 8).max(HEADER)
}

fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

unsafe fn init_heap() {
    if !FREE.is_null() {
        return;
    }
    let node = HEAP_VA as *mut Node;
    (*node).size = ALLOC_SIZE;
    (*node).next = core::ptr::null_mut();
    FREE = node;
}

unsafe fn alloc_block(layout: Layout) -> *mut u8 {
    let align = layout.align().max(HEADER);
    let size = alloc_size(layout);
    let mut prev: *mut Node = core::ptr::null_mut();
    let mut node = FREE;
    while !node.is_null() {
        let start = node as usize;
        let region = (*node).size;
        let aligned = align_up(start + HEADER, align);
        let padding = aligned.saturating_sub(start);
        if padding >= HEADER && padding + size <= region {
            let next = (*node).next;
            if prev.is_null() {
                FREE = next;
            } else {
                (*prev).next = next;
            }
            if padding >= HEADER {
                let pad = start as *mut Node;
                (*pad).size = padding;
                insert_free(pad);
            }
            let remain_at = aligned + size;
            let remain = region - (remain_at - start);
            if remain >= HEADER {
                let rest = remain_at as *mut Node;
                (*rest).size = remain;
                insert_free(rest);
            }
            return aligned as *mut u8;
        }
        prev = node;
        node = (*node).next;
    }
    core::ptr::null_mut()
}

unsafe fn free_block(ptr: *mut u8, size: usize) {
    let node = ptr as *mut Node;
    (*node).size = size;
    insert_free(node);
}

unsafe fn insert_free(node: *mut Node) {
    let addr = node as usize;
    let mut prev: *mut Node = core::ptr::null_mut();
    let mut cur = FREE;
    while !cur.is_null() && (cur as usize) < addr {
        prev = cur;
        cur = (*cur).next;
    }
    if !cur.is_null() && addr + (*node).size == cur as usize {
        (*node).size += (*cur).size;
        (*node).next = (*cur).next;
    } else {
        (*node).next = cur;
    }
    if !prev.is_null() && (prev as usize) + (*prev).size == node as usize {
        (*prev).size += (*node).size;
        (*prev).next = (*node).next;
    } else if prev.is_null() {
        FREE = node;
    } else {
        (*prev).next = node;
    }
}

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    server::run();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    panic_rt::report_panic(b"http", info);
    loop {}
}
