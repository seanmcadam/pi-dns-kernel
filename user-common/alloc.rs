// First-fit freelist for an EL0 heap.
//
// The including crate defines `HEAP_VA` and `ALLOC_SIZE`. The first
// allocation turns that range into one free block. Neighbours coalesce
// on free. A futex lock covers workers that share the heap.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicU32, Ordering};

const HEADER: usize = 16;

struct Node {
    size: usize,
    next: *mut Node,
}

static LOCK: AtomicU32 = AtomicU32::new(0);
static mut FREE: *mut Node = core::ptr::null_mut();

pub struct Heap;

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        lock();
        let ptr = alloc_block(layout);
        unlock();
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        lock();
        free_block(ptr, alloc_size(layout));
        unlock();
    }
}

fn lock() {
    while LOCK
        .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        let _ = crate::syscall::sys(
            crate::syscall::FUTEX_WAIT,
            &LOCK as *const AtomicU32 as u64,
            1,
            0,
            0,
            0,
            0,
        );
    }
}

fn unlock() {
    LOCK.store(0, Ordering::Release);
    let _ = crate::syscall::sys(
        crate::syscall::FUTEX_WAKE,
        &LOCK as *const AtomicU32 as u64,
        1,
        0,
        0,
        0,
        0,
    );
}

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
    let node = crate::HEAP_VA as *mut Node;
    (*node).size = crate::ALLOC_SIZE;
    (*node).next = core::ptr::null_mut();
    FREE = node;
}

unsafe fn alloc_block(layout: Layout) -> *mut u8 {
    init_heap();
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
