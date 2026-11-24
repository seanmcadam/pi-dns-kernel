//! Dedicated RAM for the blocker's domain table.
//!
//! The kernel heap is 2 MiB and is shared with every other allocation.
//! Block lists live here instead: a NOLOAD region above the crash slot,
//! mapped with the rest of RAM, and never handed to the global allocator.
//! Pi 4 and Pi 5 get 32 MiB. The virt board's 512 MiB guest gets 16 MiB,
//! placed above its crash slot. A zero word means an empty hash slot, so
//! the blocker zeros the region once before publishing a table.

extern "C" {
    static __block_start: u8;
    static __block_end: u8;
}

pub fn start() -> *mut u8 {
    &__block_start as *const u8 as *mut u8
}

pub fn end() -> *mut u8 {
    &__block_end as *const u8 as *mut u8
}

pub fn size() -> usize {
    (end() as usize).saturating_sub(start() as usize)
}
