use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use spin::Mutex;

extern "C" {
    static __heap_start: u8;
    static __heap_end: u8;
}

struct BumpAllocator {
    heap_start: usize,
    heap_end: usize,
    next: usize,
}

impl BumpAllocator {
    pub const fn new() -> Self {
        Self {
            heap_start: 0,
            heap_end: 0,
            next: 0,
        }
    }

    pub unsafe fn init(&mut self) {
        self.heap_start = &__heap_start as *const u8 as usize;
        self.heap_end = &__heap_end as *const u8 as usize;
        self.next = self.heap_start;
    }

    fn alloc(&mut self, layout: Layout) -> *mut u8 {
        let alloc_start = align_up(self.next, layout.align());
        let alloc_end = match alloc_start.checked_add(layout.size()) {
            Some(end) => end,
            None => return null_mut(),
        };

        if alloc_end > self.heap_end {
            null_mut()
        } else {
            self.next = alloc_end;
            alloc_start as *mut u8
        }
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

pub struct LockedAllocator(Mutex<BumpAllocator>);

impl LockedAllocator {
    pub const fn new() -> Self {
        Self(Mutex::new(BumpAllocator::new()))
    }

    pub unsafe fn init(&self) {
        self.0.lock().init();
    }
}

unsafe impl GlobalAlloc for LockedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0.lock().alloc(layout)
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't support deallocation
    }
}

#[global_allocator]
pub static ALLOCATOR: LockedAllocator = LockedAllocator::new();
