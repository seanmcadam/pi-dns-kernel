//! One board per kernel image.
//!
//! Cargo enables `board-virt` by default or `board-pi4` for the Pi 4 image.
//! `board-pi5` is reserved and refuses to build; a Pi 5 board is a later change.
//! Shared code calls the functions below. It does not name a board feature.

#[cfg(feature = "board-pi5")]
compile_error!("board-pi5 is reserved; a Pi 5 board is a later change");

#[cfg(all(feature = "board-virt", feature = "board-pi4"))]
compile_error!("enable only one of board-virt and board-pi4");

#[cfg(not(any(feature = "board-virt", feature = "board-pi4")))]
compile_error!("select board-virt or board-pi4");

#[cfg(feature = "board-virt")]
mod virt;
#[cfg(feature = "board-virt")]
use virt::Virt as Active;

#[cfg(feature = "board-pi4")]
mod pi4;
#[cfg(feature = "board-pi4")]
use pi4::Pi4 as Active;

use smoltcp::phy::Device;

/// What a device interrupt was. The shared handler counts it and wakes the owner.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IrqHit {
    None,
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
    fn retarget_nic(cpu: usize);
    fn probe_disk() -> bool;
    fn disk() -> Self::Disk;
    fn handle_irq(id: u32) -> IrqHit;
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

pub fn probe_nic() -> Option<Nic> {
    <Active as Board>::probe_nic()
}

pub fn nic_name() -> &'static str {
    <Active as Board>::nic_name()
}

pub fn packet_counts() -> (u32, u32) {
    <Active as Board>::packet_counts()
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
