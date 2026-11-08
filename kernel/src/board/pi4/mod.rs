//! Raspberry Pi 4 (BCM2711). QEMU `raspi4b` (QEMU 9 or newer) can boot this
//! image far enough to exercise the UART, the GIC-400, the spin table, and
//! EMMC2. GENET is not in that machine. The image has not been run on hardware.
//!
//! The link script is `link.ld` in this directory. The firmware loads the raw
//! image at physical `0x80000`. The link address is `0xFFFF000000080000`.

mod emmc;
mod genet;
mod hdmi_task {
    use core::ptr::{addr_of, addr_of_mut};
    include!("../hdmi_task.rs");
}
mod mbox {
    include!("../mailbox.rs");
}
mod smp;
mod uart;

/// HDMI0 at child `0x7ef00700`. The bcm2711 soc ranges add `0xFE000000 - 0x7E000000`.
/// `HDMI_HOTPLUG` is offset `0x1c`, bit 0. Neither the address nor the bit has
/// been confirmed on hardware. HDMI1 is the same block at `0x7ef05700`.
const HDMI0_HOTPLUG: u64 = 0xFEF0_071C;
const HDMI1_HOTPLUG: u64 = 0xFEF0_571C;
/// BCM2711 mailbox at `0x7E00B880`, same translation. Channel 8 is the property channel.
const MBOX_BASE: u64 = 0xFE00_B880;

use crate::board::{Board, IrqHit};
use crate::net::NetworkMode;

pub struct Pi4;

const DEVICE_RANGES: &[(u64, u64)] = &[(0xFC00_0000, 0x1_0000_0000)];

/// BCM2711 GIC-400 SPI 157 and 158. Untested on hardware.
const GENET_IRQ0: u32 = 32 + 157;
const GENET_IRQ1: u32 = 32 + 158;
/// BCM2711 EMMC2, SPI 126. QEMU raspi4b raises this line. Untested on hardware.
const EMMC_IRQ: u32 = 32 + 126;

impl Board for Pi4 {
    const NAME: &'static str = "pi4";
    const GICD_PHYS: u64 = 0xFF84_1000;
    const GICC_PHYS: u64 = 0xFF84_2000;
    const UART_PHYS: u64 = 0xFE20_1000;
    const TIMER_IRQ: u32 = 27;
    const RAM_START: u64 = 0;
    /// Stop at the peripheral window. Untested on hardware.
    const RAM_END: u64 = 0xFC00_0000;

    type Nic = genet::GenetDevice;
    type Disk = emmc::DiskDev;

    fn device_ranges() -> &'static [(u64, u64)] {
        DEVICE_RANGES
    }

    fn early_init() {
        uart::init_pi4();
    }

    fn start_secondary(cpu: usize, entry: u64, entry_el: u64) {
        smp::start_secondary(cpu, entry, entry_el);
    }

    fn el2_blocks_secondaries() -> bool {
        false
    }

    fn default_network() -> NetworkMode {
        default_network()
    }

    fn probe_nic() -> Option<Self::Nic> {
        genet::GenetDevice::new()
    }

    fn nic_name() -> &'static str {
        "GENET"
    }

    fn packet_counts() -> (u32, u32) {
        genet::packet_counts()
    }

    fn retarget_nic(_cpu: usize) {}

    fn probe_disk() -> bool {
        emmc::probe()
    }

    fn disk() -> Self::Disk {
        emmc::DiskDev
    }

    fn handle_irq(id: u32) -> IrqHit {
        // GENET's INTRL2 mask is not programmed. If a line is ever enabled
        // and sticks high, drop it after one count so it cannot spin the core.
        if id == GENET_IRQ0 || id == GENET_IRQ1 {
            crate::gic::disable_id(id);
            return IrqHit::Net;
        }
        if id == EMMC_IRQ {
            crate::gic::disable_id(id);
            return IrqHit::Block;
        }
        IrqHit::None
    }

    fn hdmi_present() -> bool {
        true
    }

    fn hdmi_pages() -> u32 {
        crate::board::HDMI_PAGES
    }

    fn hdmi_hpd() -> i64 {
        read_hotplug()
    }

    fn hdmi_mailbox(words: &mut [u32]) -> i64 {
        mbox::run(MBOX_BASE, words)
    }

    fn hdmi_image() -> Option<crate::board::HdmiImage> {
        Some(hdmi_task::image())
    }

    fn hdmi_code_ptr() -> *mut u8 {
        hdmi_task::code_ptr()
    }

    fn hdmi_code_bytes() -> usize {
        crate::board::HDMI_CODE_BYTES
    }
}

fn read_hotplug() -> i64 {
    let a = crate::exception::read_u32_catching(HDMI0_HOTPLUG as *const u32);
    let b = crate::exception::read_u32_catching(HDMI1_HOTPLUG as *const u32);
    if a.is_none() && b.is_none() {
        return -1;
    }
    let connected = a.unwrap_or(0) & 1 != 0 || b.unwrap_or(0) & 1 != 0;
    if connected {
        1
    } else {
        0
    }
}

fn default_network() -> NetworkMode {
    #[cfg(feature = "dhcp-default")]
    {
        NetworkMode::Dhcp {
            fallback_ip: [10, 0, 2, 15],
            fallback_netmask: [255, 255, 255, 0],
            fallback_gateway: [10, 0, 2, 2],
            timeout_ms: 3_000,
        }
    }
    #[cfg(not(feature = "dhcp-default"))]
    {
        NetworkMode::Static {
            ip: [10, 0, 2, 15],
            netmask: [255, 255, 255, 0],
            gateway: [10, 0, 2, 2],
        }
    }
}
