//! Identity-mapped EL1 MMU.
//!
//! TTBR0 maps physical addresses to the same virtual addresses with a 4 KiB
//! granule. The upper half of the 48-bit address space is owned by TTBR1,
//! which points at an empty table, so those addresses fault. A later split
//! can move the kernel into that half without changing the page-table shape.
//! EL0 does not run yet. Kernel `.text` is privileged-executable and
//! unprivileged-execute-never, which is the permission a later user mapping
//! wants.
//!
//! Virtio descriptor rings and packet buffers live in the `.dma` section.
//! Those pages are Normal non-cacheable, so the device and the CPU see the
//! same memory without cache maintenance on each kick. RAM everywhere else
//! is Normal write-back cacheable. MMIO is Device-nGnRE.
//!
//! boot.s drops EL2 to EL1 before this runs. The Pi 4 ranges below are
//! untested on hardware.

use core::ptr::{addr_of, addr_of_mut};

extern "C" {
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __dma_start: u8;
    static __dma_end: u8;
    static __heap_end: u8;
    static __stack_top: u8;
}

const PAGE: u64 = 4096;
const BLOCK: u64 = 2 * 1024 * 1024;
const STACK_BYTES: u64 = 256 * 1024;

const VALID: u64 = 1 << 0;
const TABLE_OR_PAGE: u64 = 1 << 1;
const AF: u64 = 1 << 10;
const AP_RO: u64 = 1 << 7;
const SH_INNER: u64 = 3 << 8;
const SH_OUTER: u64 = 2 << 8;
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;
const ATTR_NORMAL: u64 = 0 << 2;
const ATTR_DEVICE: u64 = 1 << 2;
const ATTR_DMA: u64 = 2 << 2;

#[repr(C, align(4096))]
struct Table {
    entries: [u64; 512],
}

struct Tables {
    l0: Table,
    l1: Table,
    l2: [Table; 4],
    l3: [Table; 8],
    ttbr1: Table,
}

static mut TABLES: Tables = Tables {
    l0: Table { entries: [0; 512] },
    l1: Table { entries: [0; 512] },
    l2: [
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
    ],
    l3: [
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
    ],
    ttbr1: Table { entries: [0; 512] },
};

static mut NEXT_L3: usize = 0;

struct Layout {
    text: (u64, u64),
    rodata: (u64, u64),
    dma: (u64, u64),
    ram: (u64, u64),
    guard: u64,
    fine_start: u64,
    fine_end: u64,
}

/// First byte of the unmapped page directly under the kernel stack.
pub fn guard_page() -> u64 {
    let stack_top = unsafe { addr_of!(__stack_top) as u64 };
    stack_top - STACK_BYTES - PAGE
}

pub fn init() {
    let layout = layout();
    unsafe {
        let l0 = addr_of!(TABLES.l0) as u64;
        let l1 = addr_of!(TABLES.l1) as u64;
        (*addr_of_mut!(TABLES.l0)).entries[0] = l1 | VALID | TABLE_OR_PAGE;

        map_ram_blocks(&layout);
        map_devices();
        map_fine(&layout);

        let ttbr1 = addr_of!(TABLES.ttbr1) as u64;
        enable(l0, ttbr1);
    }
}

fn layout() -> Layout {
    let text = range(addr_of!(__text_start), addr_of!(__text_end));
    let rodata = range(addr_of!(__rodata_start), addr_of!(__rodata_end));
    let dma = range(addr_of!(__dma_start), addr_of!(__dma_end));
    let heap_end = unsafe { addr_of!(__heap_end) as u64 };
    let stack_top = unsafe { addr_of!(__stack_top) as u64 };
    let guard = stack_top - STACK_BYTES - PAGE;
    let ram = ram_range();
    let fine_start = align_down(guard, BLOCK);
    let fine_end = align_up(heap_end, BLOCK);
    Layout {
        text,
        rodata,
        dma,
        ram,
        guard,
        fine_start,
        fine_end,
    }
}

fn range(start: *const u8, end: *const u8) -> (u64, u64) {
    (start as u64, end as u64)
}

fn ram_range() -> (u64, u64) {
    #[cfg(not(feature = "pi4"))]
    {
        (0x4000_0000, 0x4000_0000 + 512 * 1024 * 1024)
    }
    #[cfg(feature = "pi4")]
    {
        // SDRAM up to the peripheral window. Untested on hardware.
        (0x0000_0000, 0xFC00_0000)
    }
}

fn map_devices() {
    let device = attrs(ATTR_DEVICE, SH_OUTER, 0, true);
    #[cfg(not(feature = "pi4"))]
    {
        // GIC at 0x08000000, PL011 UART at 0x09000000, virtio-mmio at 0x0a000000.
        map_blocks(0x0800_0000, 0x0c00_0000, device);
    }
    #[cfg(feature = "pi4")]
    {
        // GENET at 0xFD580000, EMMC2 and PL011 inside 0xFE000000, GIC near 0xFF800000.
        // Untested on hardware.
        map_blocks(0xFC00_0000, 0x1_0000_0000, device);
    }
}

fn map_ram_blocks(layout: &Layout) {
    let flags = attrs(ATTR_NORMAL, SH_INNER, 0, true);
    let mut pa = layout.ram.0;
    while pa < layout.ram.1 {
        let inside_fine = pa >= layout.fine_start && pa < layout.fine_end;
        if !inside_fine {
            map_block(pa, flags);
        }
        pa += BLOCK;
    }
}

fn map_fine(layout: &Layout) {
    let mut pa = layout.fine_start;
    while pa < layout.fine_end {
        if pa == layout.guard {
            map_page(pa, 0);
        } else if let Some(flags) = flags_for(layout, pa) {
            map_page(pa, flags);
        }
        pa += PAGE;
    }
}

fn flags_for(layout: &Layout, pa: u64) -> Option<u64> {
    if pa < layout.ram.0 || pa >= layout.ram.1 {
        return None;
    }
    if pa >= layout.text.0 && pa < layout.text.1 {
        return Some(attrs(ATTR_NORMAL, SH_INNER, AP_RO, false));
    }
    if pa >= layout.rodata.0 && pa < layout.rodata.1 {
        return Some(attrs(ATTR_NORMAL, SH_INNER, AP_RO, true));
    }
    if pa >= layout.dma.0 && pa < layout.dma.1 {
        return Some(attrs(ATTR_DMA, SH_INNER, 0, true));
    }
    Some(attrs(ATTR_NORMAL, SH_INNER, 0, true))
}

fn attrs(attr: u64, share: u64, ap: u64, execute_never: bool) -> u64 {
    let mut flags = attr | share | ap | AF;
    if execute_never {
        flags |= PXN | UXN;
    } else {
        // EL1 may execute. EL0 may not. There is no user mapping yet.
        flags |= UXN;
    }
    flags
}

fn map_blocks(start: u64, end: u64, flags: u64) {
    let mut pa = start;
    while pa < end {
        map_block(pa, flags);
        pa += BLOCK;
    }
}

fn map_block(pa: u64, flags: u64) {
    unsafe {
        let l2 = l2_for(pa);
        let index = ((pa >> 21) & 511) as usize;
        (*l2).entries[index] = (pa & !((1 << 21) - 1)) | flags | VALID;
    }
}

fn map_page(pa: u64, flags: u64) {
    unsafe {
        let l3 = l3_for(pa);
        let index = ((pa >> 12) & 511) as usize;
        (*l3).entries[index] = if flags == 0 {
            0
        } else {
            (pa & !((1 << 12) - 1)) | flags | VALID | TABLE_OR_PAGE
        };
    }
}

unsafe fn l2_for(pa: u64) -> *mut Table {
    let gb = ((pa >> 30) & 511) as usize;
    if gb >= 4 {
        give_up("physical address outside the 4 GiB table budget");
    }
    let l1 = addr_of_mut!(TABLES.l1);
    let l2 = addr_of_mut!(TABLES.l2[gb]);
    if (*l1).entries[gb] & VALID == 0 {
        (*l1).entries[gb] = (l2 as u64) | VALID | TABLE_OR_PAGE;
    }
    l2
}

unsafe fn l3_for(pa: u64) -> *mut Table {
    let l2 = l2_for(pa);
    let index = ((pa >> 21) & 511) as usize;
    let entry = (*l2).entries[index];
    if entry & VALID != 0 && entry & TABLE_OR_PAGE != 0 {
        return (entry & 0x0000_ffff_ffff_f000) as *mut Table;
    }
    let slot = NEXT_L3;
    if slot >= 8 {
        give_up("ran out of level-3 page tables");
    }
    NEXT_L3 = slot + 1;
    let l3 = addr_of_mut!(TABLES.l3[slot]);
    (*l2).entries[index] = (l3 as u64) | VALID | TABLE_OR_PAGE;
    l3
}

fn give_up(message: &str) -> ! {
    crate::console::write_str("MMU setup failed: ");
    crate::console::write_str(message);
    crate::console::write_str("\r\n");
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

unsafe fn enable(ttbr0: u64, ttbr1: u64) {
    // 48-bit VA on both halves. TG1's 4 KiB encoding is 0b10, not 0b00.
    // IPS is 32-bit physical: every address this kernel uses fits.
    let tcr: u64 = 16
        | (1 << 8)
        | (1 << 10)
        | (3 << 12)
        | (16 << 16)
        | (1 << 24)
        | (1 << 26)
        | (3 << 28)
        | (0b10 << 30);
    // Attr0 Normal write-back, Attr1 Device-nGnRE, Attr2 Normal non-cacheable.
    let mair: u64 = 0x0000_0000_0044_04ff;
    let mut sctlr: u64 = 0;
    core::arch::asm!(
        "msr mair_el1, {mair}",
        "msr tcr_el1, {tcr}",
        "msr ttbr0_el1, {ttbr0}",
        "msr ttbr1_el1, {ttbr1}",
        "dsb sy",
        "isb",
        "ic iallu",
        "dsb sy",
        "isb",
        "mrs {sctlr}, sctlr_el1",
        "orr {sctlr}, {sctlr}, #1",
        "orr {sctlr}, {sctlr}, #4",
        "orr {sctlr}, {sctlr}, #0x1000",
        "orr {sctlr}, {sctlr}, #0x80000",
        "msr sctlr_el1, {sctlr}",
        "isb",
        mair = in(reg) mair,
        tcr = in(reg) tcr,
        ttbr0 = in(reg) ttbr0,
        ttbr1 = in(reg) ttbr1,
        sctlr = inout(reg) sctlr,
    );
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
