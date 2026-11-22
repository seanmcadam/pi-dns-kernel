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

#[cfg(test)]
mod tests {
    use super::*;

    fn is_kernel_ptr(addr: u64) -> bool {
        (addr & VIRT_OFFSET) == VIRT_OFFSET
    }

    /// Peripheral physical addresses used after the MMU is on. `0xFE100020`
    /// is PM_RSTS, the load that halted Pi 4 tryboot.
    const POST_MMU_PHYS: &[u64] = &[
        0xFE10_001C, // PM_RSTC
        0xFE10_0020, // PM_RSTS
        0xFE10_0024, // PM_WDOG
        0xFEF0_071C, // HDMI0 hotplug, legacy offset
        0xFEF0_08A8, // HDMI0 HDMI_HOTPLUG
        0xFEF0_0000, // DVP clock
        0xFE40_0000, // HVS
        0xFE20_A000, // pixelvalve2
        0x4_7EF0_08A8, // HDMI0 hotplug, 35-bit alias
        0xFE00_B880, // Pi 4 mailbox
        0xFE20_1000, // Pi 4 UART
        0xFE34_0000, // EMMC2
        0xFD58_0000, // GENET
        0xFF84_1000, // GIC
        0x0900_0000, // virt PL011
        0x0800_0000, // virt GICD
        0x1_07D0_01000, // Pi 5 UART
        0x10_7E00_B880, // Pi 5 mailbox candidate
    ];

    #[test]
    fn peripheral_physical_addresses_translate_into_the_kernel_window() {
        assert_eq!(phys_to_virt(0xFE10_0020), 0xFFFF_0000_FE10_0020);
        assert_eq!(phys_to_virt(0xFE10_001C), 0xFFFF_0000_FE10_001C);
        assert_eq!(phys_to_virt(0xFE10_0024), 0xFFFF_0000_FE10_0024);
        assert_eq!(phys_to_virt(0x4_7EF0_08A8), 0xFFFF_0004_7EF0_08A8);
        for &pa in POST_MMU_PHYS {
            let va = phys_to_virt(pa);
            assert!(!is_kernel_ptr(pa), "{pa:#x} already looked virtual");
            assert!(is_kernel_ptr(va), "{va:#x}");
            assert_eq!(va, pa | VIRT_OFFSET);
            assert_ne!(va, pa);
            assert_eq!(virt_to_phys(va), pa);
        }
    }
}
