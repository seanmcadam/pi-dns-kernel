//! GICv2. QEMU `virt` with `-cpu cortex-a72` exposes an `arm,cortex-a15-gic`
//! at `0x08000000` (distributor) and `0x08010000` (CPU interface). The
//! scripts pass `gic-version=2` so a newer QEMU default cannot switch this
//! to GICv3. The Pi 4 GIC-400 lives at `0xFF841000` / `0xFF842000`. That
//! path is compiled in and untested on hardware.
//!
//! The virtual timer is PPI 11, which is INTID 27. A read of `GICC_IAR`
//! that returns 1023 is a spurious interrupt and must not be completed.
//!
//! QEMU virt wires virtio-mmio slot `n` to SPI `16 + n` (INTID `48 + n`).
//! The driver computes that from the slot it probed, then routes the NIC
//! line at the network owner. QEMU 8.2 attaches `-device virtio-*-device`
//! at the high end of the 32 slots, so the NIC lands on INTID 79 (SPI 47)
//! and the block device on INTID 78 (SPI 46). SGI 1 is the kick from a core
//! that queued a socket job. On the Pi 4, GENET is SPI 157 and 158 (INTID
//! 189 and 190) and EMMC2 is SPI 126 (INTID 158). Those Pi 4 lines are
//! compiled in and untested on hardware.

#[cfg(not(feature = "pi4"))]
const GICD: usize = crate::addr::phys_to_virt(0x0800_0000) as usize;
#[cfg(not(feature = "pi4"))]
const GICC: usize = crate::addr::phys_to_virt(0x0801_0000) as usize;

// BCM2711 GIC-400. Untested on hardware. CPU accesses use the high alias.
#[cfg(feature = "pi4")]
const GICD: usize = crate::addr::phys_to_virt(0xFF84_1000) as usize;
#[cfg(feature = "pi4")]
const GICC: usize = crate::addr::phys_to_virt(0xFF84_2000) as usize;

/// Virtual timer, PPI 11. `16 + 11`.
pub const TIMER_IRQ: u32 = 27;
pub const SPURIOUS_IRQ: u32 = 1023;

/// Socket-queue kick. Each CPU enables this banked SGI in `init_cpu`.
pub const SGI_NET: u32 = 1;

/// BCM2711. SPI number plus 32. Untested on hardware.
#[cfg(feature = "pi4")]
pub const PI4_GENET_IRQ0: u32 = 32 + 157;
#[cfg(feature = "pi4")]
pub const PI4_GENET_IRQ1: u32 = 32 + 158;
#[cfg(feature = "pi4")]
pub const PI4_EMMC_IRQ: u32 = 32 + 126;

const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_IGROUPR: usize = 0x080;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
const GICD_ITARGETSR: usize = 0x800;
const GICD_SGIR: usize = 0xF00;
const GICC_CTLR: usize = 0x000;
const GICC_PMR: usize = 0x004;
const GICC_IAR: usize = 0x00c;
const GICC_EOIR: usize = 0x010;

fn read(base: usize, offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

fn write(base: usize, offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

fn dsb() {
    unsafe {
        core::arch::asm!("dsb sy", options(nomem, nostack, preserves_flags));
    }
}

pub fn init() {
    init_distributor();
    init_cpu();
}

/// Distributor, once, from core 0. Shared SPI registers are not banked.
pub fn init_distributor() {
    write(GICD, GICD_CTLR, 0);

    // Group 1 is what Non-secure EL1 can take, and what Secure EL1 can take
    // once AckCtl is set below. Disable every line here. The timer, the
    // virtio SPIs, and SGI 1 are enabled later, each at a priority under the
    // CPU mask. A level line left at priority 0 would outrank those.
    let typer = read(GICD, GICD_TYPER);
    let lines = ((typer & 0x1f) + 1) * 32;
    let lines = lines.min(1024);
    let mut id = 0u32;
    while id < lines {
        let off = (id / 32) as usize * 4;
        write(GICD, GICD_IGROUPR + off, 0xffff_ffff);
        write(GICD, GICD_ICENABLER + off, 0xffff_ffff);
        id += 32;
    }

    // Group 1 and the distributor enable are shared. CPU interfaces are not.
    write(GICD, GICD_CTLR, 0x3);
    dsb();
}

/// CPU interface and the banked virtual-timer PPI. Every core calls this.
pub fn init_cpu() {
    write(GICC, GICC_CTLR, 0);
    write(GICC, GICC_PMR, 0xff);
    let bit = 1u32 << (TIMER_IRQ % 32);
    // IGROUPR for a PPI is banked. Core 0's distributor init does not
    // cover the other cores, so each CPU marks its timer Group 1.
    let word = ((TIMER_IRQ / 32) * 4) as usize;
    // Priority bytes for PPIs are banked per CPU. 0xa0 sits under the mask.
    let byte = GICD_IPRIORITYR + TIMER_IRQ as usize;
    let aligned = byte & !3;
    let shift = (byte & 3) * 8;
    let mut priority = read(GICD, aligned);
    priority &= !(0xff << shift);
    priority |= 0xa0 << shift;
    write(GICD, aligned, priority);
    let group = read(GICD, GICD_IGROUPR + word);
    write(GICD, GICD_IGROUPR + word, group | bit);
    write(GICD, GICD_ISENABLER + word, bit);

    // SGI 1 is banked, so the distributor init on core 0 does not enable it
    // on the other cores. Group 1, priority 0x80, same as the device SPIs.
    set_priority(SGI_NET, 0x80);
    set_group1(SGI_NET);
    enable_id(SGI_NET);

    // QEMU's GICv2 treats this CPU as Secure. A Secure ack of a Group 1
    // interrupt returns 1022 unless GICC_CTLR.AckCtl is set, so the timer
    // never reaches the vector. AckCtl (bit 2) plus both enable bits lets
    // Secure EL1 take the Group 1 timer as an IRQ. From Non-secure EL1 the
    // same addresses alias to EnableGrp1, which is what a Pi 4 firmware
    // handover needs. FIQEn stays clear, so Group 0 would also arrive as IRQ.
    // EOImode stays 0: one EOIR write drops priority and deactivates.
    write(GICC, GICC_CTLR, 0x7);
    dsb();
}

/// Interrupt id from `GICC_IAR`, or 1023 when there is nothing to service.
pub fn ack() -> u32 {
    read(GICC, GICC_IAR) & 0x3ff
}

pub fn eoi(id: u32) {
    write(GICC, GICC_EOIR, id);
    dsb();
}

fn set_priority(intid: u32, priority: u32) {
    let byte = GICD_IPRIORITYR + intid as usize;
    let aligned = byte & !3;
    let shift = (byte & 3) * 8;
    let mut word = read(GICD, aligned);
    word &= !(0xff << shift);
    word |= (priority & 0xff) << shift;
    write(GICD, aligned, word);
}

fn set_group1(intid: u32) {
    let word = ((intid / 32) * 4) as usize;
    let bit = 1u32 << (intid % 32);
    let group = read(GICD, GICD_IGROUPR + word);
    write(GICD, GICD_IGROUPR + word, group | bit);
}

pub fn enable_id(intid: u32) {
    let word = ((intid / 32) * 4) as usize;
    let bit = 1u32 << (intid % 32);
    write(GICD, GICD_ISENABLER + word, bit);
    dsb();
}

#[cfg_attr(not(feature = "pi4"), allow(dead_code))]
pub fn disable_id(intid: u32) {
    let word = ((intid / 32) * 4) as usize;
    let bit = 1u32 << (intid % 32);
    write(GICD, GICD_ICENABLER + word, bit);
    dsb();
}

/// SPI CPU target. Bit 0 of `cpu_mask` is CPU 0. SGI and PPI targets are
/// banked and read-only, so this ignores ids below 32.
pub fn route_spi(intid: u32, cpu_mask: u8) {
    if intid < 32 {
        return;
    }
    let byte = GICD_ITARGETSR + intid as usize;
    let aligned = byte & !3;
    let shift = (byte & 3) * 8;
    let mut word = read(GICD, aligned);
    word &= !(0xff << shift);
    word |= (cpu_mask as u32) << shift;
    write(GICD, aligned, word);
    dsb();
}

/// Group 1, priority, target, then enable. `cpu_mask` bit 0 is CPU 0.
pub fn arm_spi(intid: u32, priority: u32, cpu_mask: u8) {
    set_group1(intid);
    set_priority(intid, priority);
    route_spi(intid, cpu_mask);
    enable_id(intid);
}

/// One bit per online CPU, starting at CPU 0. At least CPU 0.
pub fn online_cpu_mask() -> u8 {
    let n = crate::smp::online().clamp(1, 8);
    (1u8 << n) - 1
}

/// Software-generated interrupt `sgi` to one CPU. TargetListFilter 0 uses
/// the CPU target bitmask in bits 23:16. Bit 0 of that mask is CPU 0.
pub fn send_sgi(sgi: u32, cpu: usize) {
    let list = 1u32 << (cpu & 7);
    let value = (list << 16) | (sgi & 0xf);
    write(GICD, GICD_SGIR, value);
    dsb();
}
