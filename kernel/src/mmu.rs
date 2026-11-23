//! Upper-half EL1 MMU, plus a small EL0 window in TTBR0.
//!
//! `boot.s` turns the MMU on with a 1 GiB block map, jumps to the high link
//! address, and leaves TTBR0 empty. This module replaces that coarse TTBR1
//! map with 2 MiB blocks and 4 KiB pages. TTBR0 is not used for the kernel.
//! EL0 cannot walk TTBR1, so a user load or store of a kernel address faults.
//!
//! Page-table entries store physical addresses. The CPU writes the tables
//! through their high virtual addresses. Bits 47:0 of a kernel virtual
//! address equal the physical address, so the level-1/2/3 indices match.
//!
//! EL0 lives at `USER_CODE_VA` (`0x1_0000_0000`) in its own TTBR0 root, not
//! in the kernel tables. User leaves have nG set. TCR_EL1.AS is 0 and A1
//! is 0, so the 8-bit ASID is TTBR0[55:48] and bits [63:56] are ignored.
//! Each slot uses ASID slot+1. Kernel pages stay global and EL1-only.
//!
//! Virtio descriptor rings and packet buffers live in the `.dma` section.
//! Those pages are Normal non-cacheable. The device is programmed with the
//! physical address; the CPU uses the high alias. Other RAM is Normal
//! write-back. MMIO is Device-nGnRE.
//!
//! boot.s drops EL2 to EL1 before this runs. The Pi 4 and Pi 5 ranges are
//! untested on hardware. Addresses below 4 GiB use the four level-2 tables.
//! A small extra pool covers the BCM2712 device window above 4 GiB and the
//! Pi 4 HDMI alias at `0x4_7EE00000`. Virt never allocates from that pool.
//! The Pi 4 HDMI console uses one of the low tables for its framebuffer.

use crate::addr::{phys_to_virt, virt_to_phys};

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

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
const NG: u64 = 1 << 11;
const AP_EL0_RW: u64 = 1 << 6;
const AP_EL0_RO: u64 = (1 << 6) | (1 << 7);
const PXN: u64 = 1 << 53;
const UXN: u64 = 1 << 54;

/// EL0 window. Not an identity map: the physical pages are kernel RAM.
/// Code ends at `USER_CODE_VA + USER_CODE_BYTES`. The next page is unmapped
/// so a pointer just past the code is a fault, not a heap address.
pub const USER_CODE_VA: u64 = 0x1_0000_0000;
pub const USER_CODE_BYTES: u64 = 512 * 1024;
pub const USER_DATA_VA: u64 = 0x1_0009_0000;
pub const USER_DATA_BYTES: u64 = 64 * 1024;
pub const USER_HEAP_VA: u64 = crate::fault_log::USER_HEAP_VA;
pub const USER_HEAP_BYTES: u64 = crate::fault_log::USER_HEAP_BYTES;
pub const USER_STACK_VA: u64 = crate::fault_log::USER_STACK_VA;
pub const USER_STACK_BYTES: u64 = crate::fault_log::USER_STACK_BYTES;
/// Firmware framebuffer, mapped only into the HDMI task. It sits in the gap
/// after the user stack so a 640x480 surface still fits in the two level-3
/// tables. Level-2 index 1.
pub const USER_FB_VA: u64 = 0x1_0024_0000;
pub const USER_FB_MAX: u64 = 1280 * 1024;
const ATTR_NORMAL: u64 = 0 << 2;
const ATTR_DEVICE: u64 = 1 << 2;
const ATTR_DMA: u64 = 2 << 2;

/// Stage-1 leaf for an EL0 page. Bit 11 is nG, so the TLB entry is tagged
/// with the current ASID and does not match a different process. Kernel
/// leaves never set this bit.
const fn user_leaf(exec: bool) -> u64 {
    let common = ATTR_NORMAL | SH_INNER | AF | NG | PXN;
    if exec {
        common | AP_EL0_RO
    } else {
        common | AP_EL0_RW | UXN
    }
}

const fn user_fb_leaf() -> u64 {
    ATTR_DMA | SH_INNER | AP_EL0_RW | AF | NG | PXN | UXN
}

/// TCR_EL1.AS is 0. The live ASID is TTBR0[55:48]. [63:56] reads back as
/// written and is ignored for allocation and matching.
pub const fn ttbr0_for(base: u64, asid: u64) -> u64 {
    base | ((asid & 0xff) << 48)
}

/// TLBI ASIDE1 takes the ASID in Xt[63:48]. With an 8-bit ASID the value
/// sits in [55:48] and [63:56] is 0.
pub const fn tlbi_asid_arg(asid: u64) -> u64 {
    (asid & 0xff) << 48
}

const _: () = {
    let code = user_leaf(true);
    let data = user_leaf(false);
    let fb = user_fb_leaf();
    assert!(code & NG != 0 && data & NG != 0 && fb & NG != 0);
    assert!(code & UXN == 0);
    assert!(data & UXN != 0 && fb & UXN != 0);
    assert!(code & AP_EL0_RO == AP_EL0_RO);
    // Kernel RAM leaves are built without nG. A global entry would match
    // every ASID, which is what user leaves must not do.
    assert!((ATTR_NORMAL | SH_INNER | AF | UXN) & NG == 0);
    assert!(ttbr0_for(0x1000, 2) == 0x1000 | (2 << 48));
    assert!(ttbr0_for(0x1000, 2) & (0xff << 56) == 0);
    assert!(tlbi_asid_arg(2) == 2 << 48);
    assert!(tlbi_asid_arg(2) & (0xff << 56) == 0);
};

#[repr(C, align(4096))]
struct Table {
    entries: [u64; 512],
}

struct Tables {
    l0: Table,
    l1: Table,
    l2: [Table; 4],
    l3: [Table; 8],
    // One TTBR0 root per EL0 task. The tasks use the same virtual window
    // and different physical pages, so each root has its own tables.
    // Level-1 index 4 is the 4 GiB point. Two level-3 tables cover the window.
    // Slot 2 is the one-page spinner, which only fills the first entry of each.
    l2_user: [Table; USER_SLOTS],
    l3_user: [[Table; 2]; USER_SLOTS],
    user_l0: [Table; USER_SLOTS],
    user_l1: [Table; USER_SLOTS],
}

pub const USER_SLOTS: usize = 7;

/// Level-2 tables for physical addresses at 4 GiB and above. The BCM2712
/// device window needs two of these (gigabytes 0x40 and 0x41). Pi 4 uses
/// one for the 35-bit HDMI alias (gigabyte 17). Virt uses none.
const HIGH_L2: usize = 4;

struct HighL2 {
    tables: [Table; HIGH_L2],
    gb: [u16; HIGH_L2],
    used: usize,
}

static mut HIGH: HighL2 = HighL2 {
    tables: [
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
        Table { entries: [0; 512] },
    ],
    gb: [0xffff; HIGH_L2],
    used: 0,
};

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
    l2_user: [const { Table { entries: [0; 512] } }; USER_SLOTS],
    l3_user: [const { [const { Table { entries: [0; 512] } }; 2] }; USER_SLOTS],
    user_l0: [const { Table { entries: [0; 512] } }; USER_SLOTS],
    user_l1: [const { Table { entries: [0; 512] } }; USER_SLOTS],
};

static FB_BYTES: AtomicU64 = AtomicU64::new(0);

static mut NEXT_L3: usize = 0;

/// Page tables for the Pi 4 HDMI window. Eight level-3 tables cover 16 MiB.
/// They are not the identity-map pool, so a framebuffer cannot exhaust it.
#[cfg(feature = "board-pi4")]
struct FbTables {
    l3: [Table; 8],
}

#[cfg(feature = "board-pi4")]
static mut FB_TABLES: FbTables = FbTables {
    l3: [const { Table { entries: [0; 512] } }; 8],
};

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
    let stack_top = addr_of!(__stack_top) as u64;
    stack_top - STACK_BYTES - PAGE
}

pub fn init() {
    let layout = layout();
    unsafe {
        let l1 = virt_to_phys(addr_of!(TABLES.l1) as u64);
        (&mut *addr_of_mut!(TABLES.l0)).entries[0] = l1 | VALID | TABLE_OR_PAGE;

        map_ram_blocks(&layout);
        map_devices();
        map_fine(&layout);

        let ttbr1 = virt_to_phys(addr_of!(TABLES.l0) as u64);
        install_high_map(ttbr1);
        // `.dma` was zeroed while the early map was write-back. Those dirty
        // lines still hit after the pages become Normal non-cacheable, so
        // the CPU would read the zeros and the MAC would see whatever was
        // in RAM. Drop the lines. Later TX cleans its buffer; RX invalidates
        // before the CPU looks.
        discard_dma_lines(&layout);
        if pan_supported() {
            // SPAN clear: an exception into EL1 sets PAN, so a user fault
            // cannot leave EL1 able to read the user window by accident.
            let mut sctlr: u64;
            core::arch::asm!(
                "mrs {sctlr}, sctlr_el1",
                "bic {sctlr}, {sctlr}, #0x800000",
                "msr sctlr_el1, {sctlr}",
                "isb",
                ".inst 0xd500419f",
                sctlr = out(reg) sctlr,
                options(nostack),
            );
            let _ = sctlr;
            PAN = true;
        }
    }
}

static mut PAN: bool = false;

pub fn pan_enabled() -> bool {
    unsafe { PAN }
}

pub fn set_pan(on: bool) {
    if !pan_enabled() {
        return;
    }
    unsafe {
        // Not `nomem`. LLVM may otherwise sink the user load or store past
        // both the clear and the set, and Cortex-A76 then takes a permission
        // fault. QEMU's Cortex-A72 does not enforce PAN, so that reorder
        // stays invisible there.
        if on {
            core::arch::asm!(".inst 0xd500419f", options(nostack));
        } else {
            core::arch::asm!(".inst 0xd500409f", options(nostack));
        }
    }
}

fn pan_supported() -> bool {
    let mmfr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {mmfr}, id_aa64mmfr1_el1",
            mmfr = out(reg) mmfr,
            options(nomem, nostack)
        );
    }
    (mmfr >> 4) & 0xf != 0
}

/// Map one task's EL0 code, data, heap, and stack.
///
/// `slot` is 0 through 6. The arguments are kernel virtual addresses of that
/// task's backing pages. Descriptors store physical addresses. TTBR0 is
/// pointed at this slot with ASID slot+1, which is the id `claim_asid`
/// prefers. The old ASID is invalidated before the new root is used.
pub fn map_user_window(slot: usize, code_va: u64, data_va: u64, heap_va: u64, stack_va: u64) {
    if slot >= USER_SLOTS {
        give_up("user slot out of range");
    }
    prepare_user_root(slot);
    let code = user_leaf(true);
    let data = user_leaf(false);
    map_user_span(slot, USER_CODE_VA, virt_to_phys(code_va), USER_CODE_BYTES, code);
    map_user_span(slot, USER_DATA_VA, virt_to_phys(data_va), USER_DATA_BYTES, data);
    map_user_span(slot, USER_HEAP_VA, virt_to_phys(heap_va), USER_HEAP_BYTES, data);
    map_user_span(slot, USER_STACK_VA, virt_to_phys(stack_va), USER_STACK_BYTES, data);
    // Stacks grow down. Leave the page under each stack unmapped, and the
    // page at each initial SP unmapped. The frames stay in the heap image.
    unmap_user_guards(slot);
    install_user_asid(slot);
    flush_tlb();
}

/// Map a task that does not need the full DNS/HTTP windows.
pub fn map_user_sized(
    slot: usize,
    code_va: u64,
    code_len: u64,
    data_va: u64,
    data_len: u64,
    stack_va: u64,
    stack_len: u64,
    heap_va: u64,
    heap_len: u64,
) {
    if slot >= USER_SLOTS {
        give_up("user slot out of range");
    }
    prepare_user_root(slot);
    let code = user_leaf(true);
    let data = user_leaf(false);
    map_user_span(slot, USER_CODE_VA, virt_to_phys(code_va), code_len, code);
    if data_len > 0 {
        map_user_span(slot, USER_DATA_VA, virt_to_phys(data_va), data_len, data);
    }
    if heap_len > 0 {
        map_user_span(slot, USER_HEAP_VA, virt_to_phys(heap_va), heap_len, data);
    }
    map_user_span(slot, USER_STACK_VA, virt_to_phys(stack_va), stack_len, data);
    unmap_user_guards(slot);
    install_user_asid(slot);
    flush_tlb();
}

/// Map one code page and one stack page. The spinner has no heap.
pub fn map_user_stub(slot: usize, code_va: u64, stack_va: u64) {
    if slot >= USER_SLOTS {
        give_up("user slot out of range");
    }
    prepare_user_root(slot);
    let code = user_leaf(true);
    let data = user_leaf(false);
    map_user_span(slot, USER_CODE_VA, virt_to_phys(code_va), PAGE, code);
    map_user_span(slot, USER_STACK_VA, virt_to_phys(stack_va), PAGE, data);
    unmap_user_guards(slot);
    install_user_asid(slot);
    flush_tlb();
}

fn prepare_user_root(slot: usize) {
    unsafe {
        let l0 = addr_of_mut!(TABLES.user_l0[slot]);
        let l1 = addr_of_mut!(TABLES.user_l1[slot]);
        let l2 = addr_of_mut!(TABLES.l2_user[slot]);
        (&mut *l0).entries = [0; 512];
        (&mut *l1).entries = [0; 512];
        // USER_CODE_VA is 0x1_0000_0000: level 0 index 0, level 1 index 4.
        (&mut *l0).entries[0] = virt_to_phys(l1 as u64) | VALID | TABLE_OR_PAGE;
        (&mut *l1).entries[4] = virt_to_phys(l2 as u64) | VALID | TABLE_OR_PAGE;
        for index in 0..2 {
            let l3 = addr_of_mut!(TABLES.l3_user[slot][index]);
            (&mut *l3).entries = [0; 512];
            (&mut *l2).entries[index] = virt_to_phys(l3 as u64) | VALID | TABLE_OR_PAGE;
        }
    }
}

/// Map a firmware framebuffer into the HDMI task. Pages are non-cacheable
/// so a store is visible to scanout without a cache clean. `pa` is already
/// a physical address; it is not run through `virt_to_phys`.
pub fn map_user_fb(slot: usize, pa: u64, bytes: u64) -> i64 {
    if slot >= USER_SLOTS || bytes == 0 || bytes > USER_FB_MAX || pa % PAGE != 0 || bytes % PAGE != 0
    {
        return -4;
    }
    let end = match pa.checked_add(bytes) {
        Some(end) => end,
        None => return -4,
    };
    let _ = end;
    let last = USER_FB_VA + bytes - PAGE;
    if fb_l2(USER_FB_VA).is_none() || fb_l2(last).is_none() {
        return -4;
    }
    unmap_user_fb(slot);
    let flags = user_fb_leaf();
    map_user_span(slot, USER_FB_VA, pa, bytes, flags);
    FB_BYTES.store(bytes, Ordering::Release);
    tlbi_asid((slot as u64) + 1);
    flush_tlb();
    USER_FB_VA as i64
}

pub fn unmap_user_fb(slot: usize) {
    if slot >= USER_SLOTS {
        return;
    }
    let bytes = FB_BYTES.swap(0, Ordering::AcqRel);
    if bytes == 0 {
        return;
    }
    let mut off = 0;
    while off < bytes {
        let page = USER_FB_VA + off;
        let Some(l2_index) = fb_l2(page) else {
            break;
        };
        let l3_index = ((page >> 12) & 511) as usize;
        unsafe {
            let l3 = addr_of_mut!(TABLES.l3_user[slot][l2_index]);
            (&mut *l3).entries[l3_index] = 0;
        }
        off += PAGE;
    }
    tlbi_asid((slot as u64) + 1);
    flush_tlb();
}

fn fb_l2(page: u64) -> Option<usize> {
    let l2_index = ((page >> 21) & 511) as usize;
    if l2_index >= 2 {
        None
    } else {
        Some(l2_index)
    }
}

fn unmap_user_guards(slot: usize) {
    let mut index = 0;
    while let Some(page) = crate::fault_log::user_guard_page(slot, index) {
        unmap_user_page(slot, page);
        index += 1;
    }
}

fn unmap_user_page(slot: usize, va: u64) {
    let l2_index = ((va >> 21) & 511) as usize;
    let l3_index = ((va >> 12) & 511) as usize;
    if l2_index >= 2 {
        return;
    }
    unsafe {
        let l3 = addr_of_mut!(TABLES.l3_user[slot][l2_index]);
        (&mut *l3).entries[l3_index] = 0;
    }
}

fn map_user_span(slot: usize, va: u64, pa: u64, bytes: u64, flags: u64) {
    let mut off = 0;
    while off < bytes {
        let page = va + off;
        let l2_index = ((page >> 21) & 511) as usize;
        let l3_index = ((page >> 12) & 511) as usize;
        if l2_index >= 2 {
            return;
        }
        unsafe {
            let l3 = addr_of_mut!(TABLES.l3_user[slot][l2_index]);
            (&mut *l3).entries[l3_index] = (pa + off) | flags | VALID | TABLE_OR_PAGE;
        }
        off += PAGE;
    }
}

fn in_window(ptr: u64, end: u64, start: u64, bytes: u64) -> bool {
    ptr >= start && end <= start + bytes
}

pub fn user_slice_ok(ptr: u64, len: u64) -> bool {
    let Some(end) = checked_end(ptr, len) else {
        return false;
    };
    let inside = in_window(ptr, end, USER_CODE_VA, USER_CODE_BYTES)
        || in_window(ptr, end, USER_DATA_VA, USER_DATA_BYTES)
        || in_window(ptr, end, USER_HEAP_VA, USER_HEAP_BYTES)
        || in_window(ptr, end, USER_STACK_VA, USER_STACK_BYTES);
    inside && !crate::fault_log::user_range_hits_guard(crate::percpu::slot(), ptr, end)
}

/// Pages EL0 may store to. Code is read-only, so a recv buffer there is rejected
/// instead of taking an EL1 permission fault.
pub fn user_write_ok(ptr: u64, len: u64) -> bool {
    let Some(end) = checked_end(ptr, len) else {
        return false;
    };
    let inside = in_window(ptr, end, USER_DATA_VA, USER_DATA_BYTES)
        || in_window(ptr, end, USER_HEAP_VA, USER_HEAP_BYTES)
        || in_window(ptr, end, USER_STACK_VA, USER_STACK_BYTES);
    inside && !crate::fault_log::user_range_hits_guard(crate::percpu::slot(), ptr, end)
}

fn checked_end(ptr: u64, len: u64) -> Option<u64> {
    if len == 0 {
        return None;
    }
    ptr.checked_add(len)
}

/// Preferred ASID for a slot: dns 1, http 2, spin 3, log 4, telnet 5,
/// vnc 6, hdmi 7. ASID 0 is not used for a user root. Kernel translations
/// are global. User translations are non-global, so an entry tagged with
/// one of these ids does not match another process at the same VA.
pub fn slot_asid(slot: usize) -> u64 {
    (slot as u64) + 1
}

fn install_user_asid(slot: usize) {
    let asid = slot_asid(slot);
    use_user_slot(slot, asid);
    tlbi_asid(asid);
    invalidate_icache();
}

/// Boot self-test on slot 0. `active` installs ASID 1. Clearing it drops
/// that ASID and the I-cache before DNS reuses the same pages and the same
/// id. TTBR0 is then zero, so a later walk does not refill ASID 0.
pub fn set_user_asid(active: bool) {
    if active {
        use_user_slot(0, 1);
        return;
    }
    tlbi_asid(1);
    invalidate_icache();
    unsafe {
        core::arch::asm!("msr ttbr0_el1, xzr", "isb", options(nostack));
    }
}

/// Point TTBR0 at this slot. Does not invalidate. Callers that publish a
/// new root use `install_user_asid` or `tlbi_asid`. A switch between two
/// live ids needs no invalidate: nG entries miss when the ASID differs.
pub fn use_user_slot(slot: usize, asid: u64) {
    if slot >= USER_SLOTS {
        return;
    }
    let base = unsafe { virt_to_phys(addr_of!(TABLES.user_l0[slot]) as u64) };
    let ttbr = ttbr0_for(base, asid);
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {ttbr}",
            "isb",
            ttbr = in(reg) ttbr,
            options(nostack),
        );
    }
}

pub fn flush_tlb() {
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "tlbi vmalle1is",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
}

/// Drop this task's user root. The next map rebuilds it. Call `tlbi_asid`
/// as well so other cores drop translations tagged with the old ASID.
pub fn release_user_root(slot: usize) {
    if slot >= USER_SLOTS {
        return;
    }
    unsafe {
        let l0 = addr_of_mut!(TABLES.user_l0[slot]);
        (&mut *l0).entries[0] = 0;
    }
}

/// Invalidate EL0/EL1 stage-1 entries for one ASID on every core.
/// Inner shareable. The argument uses the 8-bit ASID in bits [55:48].
pub fn tlbi_asid(asid: u64) {
    let arg = tlbi_asid_arg(asid);
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "tlbi aside1is, {arg}",
            "dsb ish",
            "isb",
            arg = in(reg) arg,
            options(nostack),
        );
    }
}

/// Drop fetched instructions on every core and every ASID. A core accepts
/// the broadcast only after CPUECTLR_EL1.SMPEN is set. The issuing core
/// still executes it.
pub fn invalidate_icache() {
    unsafe {
        core::arch::asm!(
            "dsb ish",
            "ic ialluis",
            "dsb ish",
            "isb",
            options(nostack),
        );
    }
}

fn layout() -> Layout {
    let text = phys_range(addr_of!(__text_start), addr_of!(__text_end));
    let rodata = phys_range(addr_of!(__rodata_start), addr_of!(__rodata_end));
    let dma = phys_range(addr_of!(__dma_start), addr_of!(__dma_end));
    let heap_end = virt_to_phys(addr_of!(__heap_end) as u64);
    let stack_top = virt_to_phys(addr_of!(__stack_top) as u64);
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

fn phys_range(start: *const u8, end: *const u8) -> (u64, u64) {
    (virt_to_phys(start as u64), virt_to_phys(end as u64))
}

fn ram_range() -> (u64, u64) {
    (crate::board::RAM_START, crate::board::RAM_END)
}

fn map_devices() {
    let device = attrs(ATTR_DEVICE, SH_OUTER, 0, true);
    for &(start, end) in crate::board::device_ranges() {
        map_blocks(start, end, device);
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
        if pa == layout.guard
            || crate::task::is_kstack_guard_pa(pa)
            || crate::smp::is_stack_guard_pa(pa)
        {
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
        (&mut *l2).entries[index] = (pa & !((1 << 21) - 1)) | flags | VALID;
    }
}

fn map_page(pa: u64, flags: u64) {
    unsafe {
        let l3 = l3_for(pa);
        let index = ((pa >> 12) & 511) as usize;
        (&mut *l3).entries[index] = if flags == 0 {
            0
        } else {
            (pa & !((1 << 12) - 1)) | flags | VALID | TABLE_OR_PAGE
        };
    }
}

unsafe fn l2_for(pa: u64) -> *mut Table {
    let gb = ((pa >> 30) & 511) as usize;
    if gb >= 4 {
        return l2_high(gb);
    }
    let l1 = addr_of_mut!(TABLES.l1);
    let l2 = addr_of_mut!(TABLES.l2[gb]);
    if (&mut *l1).entries[gb] & VALID == 0 {
        (&mut *l1).entries[gb] = virt_to_phys(l2 as u64) | VALID | TABLE_OR_PAGE;
    }
    l2
}

unsafe fn l2_high(gb: usize) -> *mut Table {
    let high = addr_of_mut!(HIGH);
    let used = core::ptr::read(addr_of!((*high).used));
    let tags = addr_of_mut!((*high).gb) as *mut u16;
    let tables = addr_of_mut!((*high).tables) as *mut Table;
    let mut i = 0;
    while i < used {
        if core::ptr::read(tags.add(i)) as usize == gb {
            return tables.add(i);
        }
        i += 1;
    }
    if used >= HIGH_L2 {
        give_up("ran out of level-2 page tables");
    }
    core::ptr::write(tags.add(used), gb as u16);
    core::ptr::write(addr_of_mut!((*high).used), used + 1);
    let l2 = tables.add(used);
    let l1 = addr_of_mut!(TABLES.l1);
    (&mut *l1).entries[gb] = virt_to_phys(l2 as u64) | VALID | TABLE_OR_PAGE;
    l2
}

unsafe fn l3_for(pa: u64) -> *mut Table {
    let l2 = l2_for(pa);
    let index = ((pa >> 21) & 511) as usize;
    let entry = (&mut *l2).entries[index];
    if entry & VALID != 0 && entry & TABLE_OR_PAGE != 0 {
        return phys_to_virt(entry & 0x0000_ffff_ffff_f000) as *mut Table;
    }
    let slot = NEXT_L3;
    if slot >= 8 {
        give_up("ran out of level-3 page tables");
    }
    NEXT_L3 = slot + 1;
    let l3 = addr_of_mut!(TABLES.l3[slot]);
    (&mut *l2).entries[index] = virt_to_phys(l3 as u64) | VALID | TABLE_OR_PAGE;
    l3
}

/// Level-3 tables for one firmware-log window. Two tables cover 4 MiB,
/// which is enough for the 1 MiB cap in [`fwupd::vclog_window`]. They are
/// not the kernel fine-page pool.
#[cfg(feature = "board-pi4")]
struct VcTables {
    l3: [Table; 2],
}

#[cfg(feature = "board-pi4")]
static mut VC_TABLES: VcTables = VcTables {
    l3: [const { Table { entries: [0; 512] } }; 2],
};

/// Map `[start, end)` read-only Device-nGnRE, splitting the 2 MiB blocks
/// that cover it. Other pages in those blocks keep the attributes they
/// already had, so a framebuffer block stays non-cacheable. Fails closed
/// when the range is empty, outside RAM, overlaps the kernel, crosses more
/// than two blocks, or a block is already a page table. The caller must
/// not read the range when this returns false.
#[cfg(feature = "board-pi4")]
pub fn map_vc_ro(start: u64, end: u64) -> bool {
    if start >= end {
        return false;
    }
    let heap_end = virt_to_phys(addr_of!(__heap_end) as u64);
    if start < heap_end || end > crate::board::RAM_END {
        return false;
    }
    let page_lo = align_down(start, PAGE);
    let page_hi = align_up(end, PAGE);
    let block_lo = align_down(page_lo, BLOCK);
    let block_hi = align_up(page_hi, BLOCK);
    let blocks = (block_hi - block_lo) / BLOCK;
    if blocks == 0 || blocks > 2 {
        return false;
    }
    unsafe {
        let mut n = 0u64;
        while n < blocks {
            let block = block_lo + n * BLOCK;
            let l2 = l2_for(block);
            let index = ((block >> 21) & 511) as usize;
            let entry = (&*l2).entries[index];
            if entry & VALID == 0 || entry & TABLE_OR_PAGE != 0 {
                return false;
            }
            n += 1;
        }
        let va = phys_to_virt(page_lo);
        let va_end = phys_to_virt(page_hi);
        let mut line = va & !63;
        while line < va_end {
            core::arch::asm!("dc civac, {line}", line = in(reg) line, options(nostack));
            line = line.saturating_add(64);
        }
        core::arch::asm!("dsb sy", options(nostack));
        let device = attrs(ATTR_DEVICE, SH_OUTER, AP_RO, true);
        n = 0;
        while n < blocks {
            let block = block_lo + n * BLOCK;
            let l2 = l2_for(block);
            let index = ((block >> 21) & 511) as usize;
            let entry = (&*l2).entries[index];
            let l3 = addr_of_mut!(VC_TABLES.l3[n as usize]);
            (&mut *l3).entries = [0; 512];
            let keep = block_leaf_attrs(entry);
            let mut page = 0u64;
            while page < 512 {
                let pa = block + page * PAGE;
                let flags = if pa + PAGE > start && pa < end {
                    device
                } else {
                    keep
                };
                (&mut *l3).entries[page as usize] =
                    (pa & !((1 << 12) - 1)) | flags | VALID | TABLE_OR_PAGE;
                page += 1;
            }
            (&mut *l2).entries[index] = virt_to_phys(l3 as u64) | VALID | TABLE_OR_PAGE;
            n += 1;
        }
    }
    flush_tlb();
    true
}

/// Attribute bits shared by a block and a page. Bits [20:12] of a block
/// are not an address, but they are part of a page's output address, so
/// they are not copied.
#[cfg(feature = "board-pi4")]
fn block_leaf_attrs(entry: u64) -> u64 {
    const KEEP: u64 = (0b111 << 2) | (0b11 << 6) | (0b11 << 8) | AF | NG | PXN | UXN;
    entry & KEEP
}

/// Map a Pi 4 framebuffer and return the virtual address to store through.
///
/// The window is Normal non-cacheable, EL1-only, and not an identity map.
/// Identity blocks that cover the same physical pages are switched to the
/// same attribute when they sit above the kernel heap, so those pages are
/// not also write-back. A framebuffer that overlaps the kernel is refused.
#[cfg(feature = "board-pi4")]
pub fn map_framebuffer(pa: u64, bytes: u64) -> Option<u64> {
    const WINDOW: u64 = 0x2_0000_0000;
    const MAX_BYTES: u64 = 16 * 1024 * 1024;
    if bytes == 0 || bytes > MAX_BYTES || pa % PAGE != 0 {
        return None;
    }
    let Some(end) = pa.checked_add(bytes) else {
        return None;
    };
    let heap_end = virt_to_phys(addr_of!(__heap_end) as u64);
    if pa < heap_end || end > crate::board::RAM_END {
        return None;
    }
    let pages = align_up(bytes, PAGE) / PAGE;
    if pages > 8 * 512 {
        return None;
    }
    let flags = attrs(ATTR_DMA, SH_INNER, 0, true);
    unsafe {
        let mut page_i = 0u64;
        while page_i < pages {
            let va = WINDOW + page_i * PAGE;
            let l2 = l2_for(va);
            let l2_index = ((va >> 21) & 511) as usize;
            let slot = (page_i >> 9) as usize;
            let l3 = addr_of_mut!(FB_TABLES.l3[slot]);
            if (&*l2).entries[l2_index] & VALID == 0 {
                (&mut *l3).entries = [0; 512];
                (&mut *l2).entries[l2_index] = virt_to_phys(l3 as u64) | VALID | TABLE_OR_PAGE;
            }
            let l3_index = ((va >> 12) & 511) as usize;
            (&mut *l3).entries[l3_index] =
                (pa + page_i * PAGE) | flags | VALID | TABLE_OR_PAGE;
            page_i += 1;
        }
        let mut block = pa & !(BLOCK - 1);
        let block_end = align_up(end, BLOCK);
        while block < block_end {
            if block >= heap_end {
                let l2 = l2_for(block);
                let index = ((block >> 21) & 511) as usize;
                (&mut *l2).entries[index] = block | flags | VALID;
            }
            block += BLOCK;
        }
        core::arch::asm!("dsb ish", "isb", options(nostack));
    }
    flush_tlb();
    Some(phys_to_virt(WINDOW))
}

fn give_up(message: &str) -> ! {
    crate::console::write_str("MMU setup failed: ");
    crate::console::write_str(message);
    crate::console::write_str("\r\n");
    let mut raw = [0u8; 80];
    let prefix = b"mmu failed ";
    let mut n = prefix.len().min(raw.len());
    raw[..n].copy_from_slice(&prefix[..n]);
    let msg = message.as_bytes();
    let take = msg.len().min(raw.len() - n);
    raw[n..n + take].copy_from_slice(&msg[..take]);
    n += take;
    if let Ok(line) = core::str::from_utf8(&raw[..n]) {
        let _ = crate::logger::try_queue_fault(line);
    }
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Switch TTBR1 from the coarse blocks to the strict map and set WXN.
///
/// The MMU is already on. TTBR0 is left alone: it is the empty table from
/// boot until the user window is installed. Text pages in the new map are
/// read-only, so WXN does not make them unexecutable. The TLB is invalidated
/// before WXN is set, so a stale writable translation of text cannot be
/// fetched under the new bit.
fn discard_dma_lines(layout: &Layout) {
    let start = phys_to_virt(layout.dma.0) as usize & !63;
    let end = phys_to_virt(layout.dma.1) as usize;
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc ivac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}

unsafe fn install_high_map(ttbr1: u64) {
    let mut sctlr: u64;
    core::arch::asm!(
        "msr ttbr1_el1, {ttbr1}",
        "dsb ish",
        "tlbi vmalle1is",
        "dsb ish",
        "isb",
        "mrs {sctlr}, sctlr_el1",
        "orr {sctlr}, {sctlr}, #0x80000",
        "msr sctlr_el1, {sctlr}",
        "isb",
        ttbr1 = in(reg) ttbr1,
        sctlr = out(reg) sctlr,
        options(nostack),
    );
    let _ = sctlr;
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn align_up(value: u64, align: u64) -> u64 {
    (value + align - 1) & !(align - 1)
}
