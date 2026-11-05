/// HAL implementation for virtio-drivers crate
use core::ptr::NonNull;
use virtio_drivers::{BufferDirection, Hal, PhysAddr};
use alloc::alloc::{alloc_zeroed, dealloc, Layout};

pub struct VirtioHalImpl;

unsafe impl Hal for VirtioHalImpl {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (PhysAddr, NonNull<u8>) {
        let size = pages * 4096;
        let layout = Layout::from_size_align(size, 4096).unwrap();
        let vaddr = unsafe { alloc_zeroed(layout) };
        
        if vaddr.is_null() {
            panic!("DMA allocation failed");
        }
        
        let paddr = vaddr as usize;
        let ptr = NonNull::new(vaddr).unwrap();
        
        log::info!("DMA alloc: {} pages at vaddr={:#x} paddr={:#x}", pages, vaddr as usize, paddr);
        (paddr, ptr)
    }

    unsafe fn dma_dealloc(paddr: PhysAddr, vaddr: NonNull<u8>, pages: usize) -> i32 {
        let size = pages * 4096;
        let layout = Layout::from_size_align(size, 4096).unwrap();
        dealloc(vaddr.as_ptr(), layout);
        log::info!("DMA dealloc: {} pages at paddr={:#x}", pages, paddr);
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: PhysAddr, _size: usize) -> NonNull<u8> {
        // Identity mapping in our setup
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, _direction: BufferDirection) -> PhysAddr {
        let ptr = buffer.as_ptr() as *mut u8;
        ptr as usize
    }

    unsafe fn unshare(_paddr: PhysAddr, _buffer: NonNull<[u8]>, _direction: BufferDirection) {
        // Nothing needed
    }
}
