//! Kernel virtual addresses.
//!
//! The kernel is linked with bit 63:48 set. Bits 47:0 of a kernel virtual
//! address are the physical address, so a page-table index taken from either
//! form names the same slot. Devices still see physical addresses. The CPU
//! uses the virtual form. EL0 cannot walk TTBR1, so a user load of a kernel
//! address faults.

pub const VIRT_OFFSET: u64 = 0xFFFF_0000_0000_0000;

pub const fn phys_to_virt(pa: u64) -> u64 {
    pa | VIRT_OFFSET
}

pub const fn virt_to_phys(va: u64) -> u64 {
    va & !VIRT_OFFSET
}
