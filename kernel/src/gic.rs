//! GICv2. QEMU `virt` with `-cpu cortex-a72` exposes an `arm,cortex-a15-gic`
//! at `0x08000000` (distributor) and `0x08010000` (CPU interface). The
//! scripts pass `gic-version=2` so a newer QEMU default cannot switch this
//! to GICv3. The Pi 4 GIC-400 lives at `0xFF841000` / `0xFF842000`. That
//! path is compiled in and untested on hardware.
//!
//! The virtual timer is PPI 11, which is INTID 27. A read of `GICC_IAR`
//! that returns 1023 is a spurious interrupt and must not be completed.

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

const GICD_CTLR: usize = 0x000;
const GICD_TYPER: usize = 0x004;
const GICD_IGROUPR: usize = 0x080;
const GICD_ISENABLER: usize = 0x100;
const GICD_ICENABLER: usize = 0x180;
const GICD_IPRIORITYR: usize = 0x400;
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
    // once AckCtl is set below. Disable every other line so a level-triggered
    // device interrupt left at priority 0 cannot outrank the timer. The timer
    // is the only interrupt this kernel services.
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
