//! One board per kernel image.
//!
//! Cargo enables `board-virt` by default, `board-pi4` for the Pi 4 image, or
//! `board-pi5` for the Pi 5 image. Shared code calls the functions below. It
//! does not name a board feature.

#[cfg(any(
    all(feature = "board-virt", feature = "board-pi4"),
    all(feature = "board-virt", feature = "board-pi5"),
    all(feature = "board-pi4", feature = "board-pi5"),
))]
compile_error!("enable only one of board-virt, board-pi4, and board-pi5");

#[cfg(not(any(feature = "board-virt", feature = "board-pi4", feature = "board-pi5")))]
compile_error!("select board-virt, board-pi4, or board-pi5");

#[cfg(feature = "board-virt")]
mod virt;
#[cfg(feature = "board-virt")]
use virt::Virt as Active;

#[cfg(feature = "board-pi4")]
mod pi4;
#[cfg(feature = "board-pi4")]
use pi4::Pi4 as Active;

#[cfg(feature = "board-pi5")]
mod pi5;
#[cfg(feature = "board-pi5")]
use pi5::Pi5 as Active;

use core::ptr::addr_of_mut;

use smoltcp::phy::Device;

/// HDMI user image. Only the Pi boards fill one in.
pub struct HdmiImage {
    pub code: u64,
    pub bin_len: usize,
    pub data: u64,
    pub heap: u64,
    pub stack: u64,
}

/// Code window reserved for the HDMI program. The binary has to fit.
pub const HDMI_CODE_BYTES: usize = 128 * 1024;
pub const HDMI_DATA_BYTES: usize = 4096;
pub const HDMI_HEAP_BYTES: usize = 16 * 1024;
pub const HDMI_STACK_BYTES: usize = 64 * 1024;
/// 32 code pages, one data page, four heap pages, four stack pages.
/// Virt does not call `hdmi_pages`, so this constant is unused in that image.
#[allow(dead_code)]
pub const HDMI_PAGES: u32 = 41;

/// Fault-injection stub for a board with no HDMI image.
static mut HDMI_STUB: [u8; 4096] = [0; 4096];

/// What a device interrupt was. The shared handler counts it and wakes the owner.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrqHit {
    None,
    /// The Pi 5 image has no NIC, so this variant is constructed only on
    /// boards that drive one.
    #[allow(dead_code)]
    Net,
    Block,
}

pub trait Board {
    const NAME: &'static str;
    const GICD_PHYS: u64;
    const GICC_PHYS: u64;
    const UART_PHYS: u64;
    /// ARM virtual timer PPI. INTID 27 on both of these GICv2 boards.
    const TIMER_IRQ: u32;
    const RAM_START: u64;
    const RAM_END: u64;

    type Nic: Device;
    type Disk: embedded_sdmmc::BlockDevice<Error = &'static str>;

    fn device_ranges() -> &'static [(u64, u64)];
    fn early_init();
    fn start_secondary(cpu: usize, entry: u64, entry_el: u64);
    fn el2_blocks_secondaries() -> bool;
    fn default_network() -> crate::net::NetworkMode;
    fn probe_nic() -> Option<Self::Nic>;
    fn nic_name() -> &'static str;
    fn packet_counts() -> (u32, u32);
    fn packet_bytes() -> (u64, u64) {
        (0, 0)
    }
    /// The receive ring still holds a frame the stack has not taken.
    fn rx_pending() -> bool {
        false
    }
    /// Descriptor drops, ring overruns, and frames still sitting in the ring.
    fn rx_load() -> (u64, u64, u64) {
        (0, 0, 0)
    }
    /// IRQ re-arms, transmits refused because the ring was full, and
    /// transmits whose DMA did not retire inside the short budget.
    fn nic_faults() -> (u64, u64, u64) {
        (0, 0, 0)
    }
    fn retarget_nic(cpu: usize);
    fn probe_disk() -> bool;
    fn disk() -> Self::Disk;
    fn handle_irq(id: u32) -> IrqHit;

    /// Boards with a PHY refresh link state here. Others do nothing.
    fn service_nic() {}

    /// Re-enable a NIC IRQ the handler masked. Called after the poll has
    /// drained the ring. Virtio acknowledges in the handler, so the default
    /// is a no-op.
    fn arm_nic() {}

    /// `false` holds a DHCP timer until the link comes up. Boards without
    /// that wait report the link as up.
    fn nic_link() -> bool {
        true
    }

    /// Short NIC text for the HDMI status row. The default is a placeholder.
    fn write_nic_status(out: &mut [u8]) -> usize {
        let text = b"nic";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        n
    }

    /// Second HDMI row: DMA and MAC counters. Empty on boards without them.
    fn write_nic_diag(out: &mut [u8]) -> usize {
        let _ = out;
        0
    }

    /// This image has an HDMI process.
    fn hdmi_present() -> bool {
        false
    }

    fn hdmi_pages() -> u32 {
        0
    }

    fn hdmi_hpd() -> i64 {
        -1
    }

    fn hdmi_mailbox(_words: &mut [u32]) -> i64 {
        -1
    }

    fn hdmi_image() -> Option<HdmiImage> {
        None
    }

    fn hdmi_code_ptr() -> *mut u8 {
        addr_of_mut!(HDMI_STUB).cast()
    }

    fn hdmi_code_bytes() -> usize {
        4096
    }

    /// Framebuffer console: state, width, height, recoveries.
    /// State is 0 none, 1 up, 2 fallback, 3 released. Virt and Pi 5 stay at 0.
    fn hdmi_console() -> (u32, u32, u32, u32) {
        (0, 0, 0, 0)
    }

    /// Latest HDMI mailbox summary. Virt and Pi 5 report `none`.
    fn hdmi_diag(out: &mut [u8]) -> usize {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        n
    }

    /// Shutdown record from the previous reboot. Virt and Pi 5 report `none`.
    fn hdmi_last_shut(out: &mut [u8]) -> usize {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        n
    }

    /// This boot's read-only HDMI register dump. Virt and Pi 5 report `none`.
    fn hdmi_regs(out: &mut [u8]) -> usize {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        n
    }

    /// Connected HDMI port: `hdmi0`, `hdmi1`, `hdmi0+hdmi1`, or `none`.
    /// Virt and Pi 5 report `none`.
    fn hdmi_port(out: &mut [u8]) -> usize {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        n
    }

    /// Firmware `vclog` message text. Virt and Pi 5 report `none`.
    fn hdmi_vclog(out: &mut [u8]) -> usize {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        n
    }

    /// Shut down or reboot this guest. `reset` selects reboot.
    /// A successful call does not return. `true` means the platform refused
    /// and this guest is still running.
    fn power(reset: bool) -> bool {
        let _ = reset;
        true
    }

    /// Reboot through the Pi firmware one-shot tryboot flag.
    /// `true` means this board cannot do that, and the guest is still running.
    fn power_tryboot() -> bool {
        true
    }

    /// Mailbox `GET_REBOOT_FLAGS` (`0x00030064`), or nothing on boards
    /// that do not have that call. Bit 0 is the tryboot flag. The Pi
    /// bootloader usually clears it before the kernel runs.
    fn tryboot_flag() -> Option<u32> {
        None
    }
}

pub const NAME: &str = <Active as Board>::NAME;
pub const GICD_PHYS: u64 = <Active as Board>::GICD_PHYS;
pub const GICC_PHYS: u64 = <Active as Board>::GICC_PHYS;
pub const UART_PHYS: u64 = <Active as Board>::UART_PHYS;
pub const TIMER_IRQ: u32 = <Active as Board>::TIMER_IRQ;
pub const RAM_START: u64 = <Active as Board>::RAM_START;
pub const RAM_END: u64 = <Active as Board>::RAM_END;

pub type Nic = <Active as Board>::Nic;
pub type DiskDev = <Active as Board>::Disk;

pub fn device_ranges() -> &'static [(u64, u64)] {
    <Active as Board>::device_ranges()
}

pub fn early_init() {
    <Active as Board>::early_init();
}

pub fn start_secondary(cpu: usize, entry: u64, entry_el: u64) {
    <Active as Board>::start_secondary(cpu, entry, entry_el);
}

pub fn el2_blocks_secondaries() -> bool {
    <Active as Board>::el2_blocks_secondaries()
}

pub fn default_network() -> crate::net::NetworkMode {
    <Active as Board>::default_network()
}

/// Cap PHY advertisement before the NIC is probed. `0` leaves autonegotiation
/// alone. Only the Pi 4 GENET driver looks at this.
pub fn set_phy_speed(cap: u32) {
    #[cfg(feature = "board-pi4")]
    pi4::set_speed_cap(cap);
    #[cfg(not(feature = "board-pi4"))]
    let _ = cap;
}

pub fn probe_nic() -> Option<Nic> {
    <Active as Board>::probe_nic()
}

pub fn nic_name() -> &'static str {
    <Active as Board>::nic_name()
}

pub fn packet_counts() -> (u32, u32) {
    <Active as Board>::packet_counts()
}

pub fn packet_bytes() -> (u64, u64) {
    <Active as Board>::packet_bytes()
}

pub fn rx_pending() -> bool {
    <Active as Board>::rx_pending()
}

pub fn rx_load() -> (u64, u64, u64) {
    <Active as Board>::rx_load()
}

pub fn nic_faults() -> (u64, u64, u64) {
    <Active as Board>::nic_faults()
}

pub fn retarget_nic(cpu: usize) {
    <Active as Board>::retarget_nic(cpu);
}

pub fn probe_disk() -> bool {
    <Active as Board>::probe_disk()
}

pub fn disk() -> DiskDev {
    <Active as Board>::disk()
}

pub fn handle_irq(id: u32) -> IrqHit {
    <Active as Board>::handle_irq(id)
}

pub fn service_nic() {
    <Active as Board>::service_nic()
}

pub fn arm_nic() {
    <Active as Board>::arm_nic()
}

pub fn nic_link() -> bool {
    <Active as Board>::nic_link()
}

pub fn write_nic_status(out: &mut [u8]) -> usize {
    <Active as Board>::write_nic_status(out)
}

pub fn write_nic_diag(out: &mut [u8]) -> usize {
    <Active as Board>::write_nic_diag(out)
}

pub fn hdmi_present() -> bool {
    <Active as Board>::hdmi_present()
}

pub fn hdmi_pages() -> u32 {
    <Active as Board>::hdmi_pages()
}

pub fn hdmi_hpd() -> i64 {
    <Active as Board>::hdmi_hpd()
}

pub fn hdmi_mailbox(words: &mut [u32]) -> i64 {
    <Active as Board>::hdmi_mailbox(words)
}

pub fn hdmi_image() -> Option<HdmiImage> {
    <Active as Board>::hdmi_image()
}

pub fn hdmi_code_ptr() -> *mut u8 {
    <Active as Board>::hdmi_code_ptr()
}

pub fn hdmi_code_bytes() -> usize {
    <Active as Board>::hdmi_code_bytes()
}

pub fn hdmi_console() -> (u32, u32, u32, u32) {
    <Active as Board>::hdmi_console()
}

pub fn hdmi_diag(out: &mut [u8]) -> usize {
    <Active as Board>::hdmi_diag(out)
}

pub fn hdmi_last_shut(out: &mut [u8]) -> usize {
    <Active as Board>::hdmi_last_shut(out)
}

pub fn hdmi_regs(out: &mut [u8]) -> usize {
    <Active as Board>::hdmi_regs(out)
}

pub fn hdmi_port(out: &mut [u8]) -> usize {
    <Active as Board>::hdmi_port(out)
}

pub fn hdmi_vclog(out: &mut [u8]) -> usize {
    <Active as Board>::hdmi_vclog(out)
}

pub fn power(reset: bool) -> bool {
    <Active as Board>::power(reset)
}

pub fn power_tryboot() -> bool {
    <Active as Board>::power_tryboot()
}

pub fn tryboot_flag() -> Option<u32> {
    <Active as Board>::tryboot_flag()
}

/// VideoCore bus address to ARM physical address.
///
/// The Pi 4 firmware returns an alias in the top bits. `bus & 0x3FFF_FFFF`
/// is that alias with the cache bits cleared. Addresses below 16 MiB are
/// rejected so a bad reply cannot remap the firmware or the kernel image.
/// Pi 5 uses the same translation; it is unverified and unused while hotplug
/// reports disconnected.
pub fn fb_bus_to_phys(bus: u64, bytes: u64) -> Option<(u64, u64)> {
    if bytes == 0 || bytes > crate::mmu::USER_FB_MAX {
        return None;
    }
    let phys = bus & 0x3FFF_FFFF;
    let bytes = (bytes + 4095) & !4095;
    if bytes == 0 || bytes > crate::mmu::USER_FB_MAX || phys < 16 * 1024 * 1024 || phys % 4096 != 0 {
        return None;
    }
    let end = phys.checked_add(bytes)?;
    if phys < RAM_START || end > RAM_END {
        return None;
    }
    Some((phys, bytes))
}
