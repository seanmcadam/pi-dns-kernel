//! First-fit heap that returns memory on free.
//!
//! Blocks are 16-byte aligned. Each block starts with a 16-byte header
//! (total size, free-list link). `used` is the sum of the live blocks,
//! headers included, and `peak` is the high water of `used`. Freeing
//! coalesces neighbours, so a burst of short-lived allocations (a status
//! page, a config apply) comes back to the baseline.

#![cfg_attr(not(test), no_std)]

use core::alloc::Layout;
use core::ptr;

const ALIGN: usize = 16;
const HEADER: usize = 16;

pub struct Heap {
    start: usize,
    end: usize,
    free: usize,
    used: usize,
    peak: usize,
}

impl Heap {
    pub const fn empty() -> Self {
        Self {
            start: 0,
            end: 0,
            free: 0,
            used: 0,
            peak: 0,
        }
    }

    /// Bind the heap to `[start, end)`. The range is trimmed to 16 bytes.
    ///
    /// # Safety
    ///
    /// The range must be writable and not used for anything else.
    pub unsafe fn init(&mut self, start: *mut u8, end: *mut u8) {
        let start = align_up(start as usize, ALIGN);
        let end = (end as usize) & !(ALIGN - 1);
        self.used = 0;
        self.peak = 0;
        self.free = 0;
        if end.saturating_sub(start) < HEADER + ALIGN {
            self.start = 0;
            self.end = 0;
            return;
        }
        self.start = start;
        self.end = end;
        write_hdr(start, end - start, 0);
        self.free = start;
    }

    pub fn used(&self) -> usize {
        self.used
    }

    pub fn peak(&self) -> usize {
        self.peak
    }

    pub fn size(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// # Safety
    ///
    /// Only called while this heap exclusively owns the region.
    pub unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {
        if layout.align() > ALIGN || self.free == 0 {
            return ptr::null_mut();
        }
        let need = HEADER + align_up(layout.size().max(1), ALIGN);
        let mut prev = 0usize;
        let mut cur = self.free;
        while cur != 0 {
            let (bsize, next) = read_hdr(cur);
            if bsize >= need {
                let leftover = bsize - need;
                let taken = if leftover >= HEADER + ALIGN {
                    let rest = cur + need;
                    write_hdr(rest, leftover, next);
                    rest
                } else {
                    next
                };
                if leftover >= HEADER + ALIGN {
                    write_hdr(cur, need, 0);
                }
                if prev == 0 {
                    self.free = taken;
                } else {
                    let (ps, _) = read_hdr(prev);
                    write_hdr(prev, ps, taken);
                }
                let got = if leftover >= HEADER + ALIGN {
                    need
                } else {
                    bsize
                };
                self.used += got;
                if self.used > self.peak {
                    self.peak = self.used;
                }
                return (cur + HEADER) as *mut u8;
            }
            prev = cur;
            cur = next;
        }
        ptr::null_mut()
    }

    /// # Safety
    ///
    /// `ptr` came from `alloc` on this heap and has not been freed.
    pub unsafe fn dealloc(&mut self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        let block = (ptr as usize) - HEADER;
        if block < self.start || block >= self.end {
            return;
        }
        let (bsize, _) = read_hdr(block);
        if bsize < HEADER || block + bsize > self.end {
            return;
        }
        self.used = self.used.saturating_sub(bsize);
        let mut prev = 0usize;
        let mut cur = self.free;
        while cur != 0 && cur < block {
            prev = cur;
            cur = read_hdr(cur).1;
        }
        write_hdr(block, bsize, cur);
        if prev == 0 {
            self.free = block;
        } else {
            let (ps, _) = read_hdr(prev);
            write_hdr(prev, ps, block);
        }
        self.coalesce(prev, block);
    }

    unsafe fn coalesce(&mut self, prev: usize, block: usize) {
        let mut node = block;
        if prev != 0 {
            let (ps, pn) = read_hdr(prev);
            if prev + ps == block {
                let (bs, bn) = read_hdr(block);
                write_hdr(prev, ps + bs, bn);
                node = prev;
            } else {
                let _ = pn;
            }
        }
        let (ns, next) = read_hdr(node);
        if next != 0 && node + ns == next {
            let (rs, rn) = read_hdr(next);
            write_hdr(node, ns + rs, rn);
        }
    }
}

unsafe fn read_hdr(block: usize) -> (usize, usize) {
    let p = block as *const usize;
    (*p, *p.add(1))
}

unsafe fn write_hdr(block: usize, size: usize, next: usize) {
    let p = block as *mut usize;
    *p = size;
    *p.add(1) = next;
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;
    use std::vec::Vec;

    fn heap(bytes: usize) -> (Vec<u8>, Heap) {
        let mut buf = vec![0u8; bytes];
        let mut heap = Heap::empty();
        unsafe {
            heap.init(buf.as_mut_ptr(), buf.as_mut_ptr().add(buf.len()));
        }
        (buf, heap)
    }

    #[test]
    fn status_sized_allocs_return_to_baseline() {
        let (_buf, mut heap) = heap(256 * 1024);
        let base = heap.used();
        assert_eq!(base, 0);
        for round in 0..400 {
            let mut ptrs = [ptr::null_mut(); 6];
            for (i, slot) in ptrs.iter_mut().enumerate() {
                let n = 16 + ((i + round) % 3) * 4;
                let layout = Layout::from_size_align(n, 8).unwrap();
                *slot = unsafe { heap.alloc(layout) };
                assert!(!(*slot).is_null());
            }
            let big = Layout::from_size_align(20 * 1024, 8).unwrap();
            let p = unsafe { heap.alloc(big) };
            assert!(!p.is_null());
            unsafe { heap.dealloc(p, big) };
            for (i, slot) in ptrs.iter().enumerate() {
                let n = 16 + ((i + round) % 3) * 4;
                let layout = Layout::from_size_align(n, 8).unwrap();
                unsafe { heap.dealloc(*slot, layout) };
            }
            assert_eq!(heap.used(), base, "round {round}");
        }
        assert!(heap.peak() > 20 * 1024);
        assert!(heap.peak() <= heap.size());
        assert_eq!(heap.size() & 15, 0);
    }

    #[test]
    fn coalesces_so_a_large_block_fits_again() {
        let (_buf, mut heap) = heap(64 * 1024);
        let layout = Layout::from_size_align(1000, 8).unwrap();
        let mut ptrs = [ptr::null_mut(); 20];
        for slot in &mut ptrs {
            *slot = unsafe { heap.alloc(layout) };
            assert!(!(*slot).is_null());
        }
        for slot in &ptrs {
            unsafe { heap.dealloc(*slot, layout) };
        }
        assert_eq!(heap.used(), 0);
        let big = Layout::from_size_align(32 * 1024, 8).unwrap();
        let p = unsafe { heap.alloc(big) };
        assert!(!p.is_null());
        unsafe { heap.dealloc(p, big) };
        assert_eq!(heap.used(), 0);
    }

    #[test]
    fn refuses_when_full() {
        let (_buf, mut heap) = heap(4096);
        let layout = Layout::from_size_align(3000, 8).unwrap();
        let p = unsafe { heap.alloc(layout) };
        assert!(!p.is_null());
        let q = unsafe { heap.alloc(layout) };
        assert!(q.is_null());
        assert_eq!(heap.used(), heap.peak());
        unsafe { heap.dealloc(p, layout) };
        assert_eq!(heap.used(), 0);
        let q = unsafe { heap.alloc(layout) };
        assert!(!q.is_null());
    }
}
