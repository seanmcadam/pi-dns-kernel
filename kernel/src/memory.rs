use core::alloc::{GlobalAlloc, Layout};
use kalloc::Heap;
use spin::Mutex;

extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

pub struct LockedAllocator(Mutex<Heap>);

impl LockedAllocator {
    pub const fn new() -> Self {
        Self(Mutex::new(Heap::empty()))
    }

    pub unsafe fn init(&self) {
        self.0
            .lock()
            .init(&__heap_start as *const u8 as *mut u8, &__heap_end as *const u8 as *mut u8);
    }

    /// `(used, size)` in bytes. `used` falls when allocations are freed.
    pub fn usage(&self) -> (usize, usize) {
        let (used, _peak, size) = self.stats();
        (used, size)
    }

    /// `(used, peak, size)`. `peak` is the high water of `used`.
    pub fn stats(&self) -> (usize, usize, usize) {
        let heap = self.0.lock();
        (heap.used(), heap.peak(), heap.size())
    }
}

unsafe impl GlobalAlloc for LockedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = self.0.lock().alloc(layout);
        if ptr.is_null() {
            #[cfg(feature = "board-pi4")]
            crate::fb_con::emergency("LOCKUP heap");
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0.lock().dealloc(ptr, layout);
    }
}

#[global_allocator]
pub static ALLOCATOR: LockedAllocator = LockedAllocator::new();
