//! Raspberry Pi 5 (BCM2712, Cortex-A76).
//!
//! QEMU has no Pi 5 machine. This image is built and its link address is
//! checked. It has not been booted, and it has not been run on hardware.
//!
//! The link script is `link.ld` in this directory. The firmware loads
//! `kernel_2712.img` at physical `0x200000`. The link address is
//! `0xFFFF000000200000`.
//!
//! Addresses come from the Raspberry Pi downstream device tree
//! `arch/arm64/boot/dts/broadcom/bcm2712.dtsi` (rpi-6.12.y). The SoC bus
//! `ranges` adds `0x10_0000_0000` to each child address:
//!
//! - debug UART (`uart10`): child `0x7d001000` → `0x107d001000`
//! - GIC-400 distributor: child `0x7fff9000` → `0x107fff9000`
//! - GIC-400 CPU interface: child `0x7fffa000` → `0x107fffa000`
//! - SDHCI host (`sdio1`): child `0x00fff000` → `0x1000fff000`
//!
//! The board file `bcm2712-rpi-5-b.dts` enables `uart10` as the 3-pin debug
//! UART and `sdio1` as the SD card. Ethernet is `rp1_eth` behind `pcie2`,
//! which this image does not drive.

mod hdmi_task {
    use core::ptr::{addr_of, addr_of_mut};
    include!("../hdmi_task.rs");
}
mod mbox {
    include!("../mailbox.rs");
}
mod nic;
mod sdhci;
mod smp;
mod uart;

/// Unverified mailbox bases. The first is the Pi 4 child address `0x7E00B880`
/// plus the bcm2712 ranges offset `0x10_0000_0000`. The second is a legacy
/// VideoCore window some Pi 5 notes call `0x107C013880`. A status read that
/// faults is skipped. RP1 HDMI is not programmed, and hotplug stays disconnected,
/// so neither base is written unless a later change reports a display.
const MBOX_CANDIDATES: [u64; 2] = [0x10_7E00_B880, 0x107C_013880];

use crate::board::{Board, IrqHit};
use crate::net::NetworkMode;

pub struct Pi5;

/// Two gigabytes of the BCM2712 SoC window. PCIe outbound windows, where an
/// RP1 BAR would appear, are outside this range and are not mapped.
const DEVICE_RANGES: &[(u64, u64)] = &[(0x10_0000_0000, 0x10_8000_0000)];

/// `sdio1` interrupt is `GIC_SPI 273`, so the INTID is 32 + 273.
const SD_IRQ: u32 = 32 + 273;

impl Board for Pi5 {
    const NAME: &'static str = "pi5";
    const GICD_PHYS: u64 = 0x1_07ff_f9000;
    const GICC_PHYS: u64 = 0x1_07ff_fa000;
    const UART_PHYS: u64 = 0x1_07d0_01000;
    /// Virtual timer PPI 11. The device tree lists PPI 13, 14, 11, 10.
    const TIMER_IRQ: u32 = 27;
    const RAM_START: u64 = 0;
    /// First 4 GiB only. An 8 GiB or 16 GiB board keeps the rest unmapped.
    /// The peripheral window starts at 64 GiB, so this does not cover MMIO.
    const RAM_END: u64 = 0x1_0000_0000;

    type Nic = nic::Rp1Nic;
    type Disk = sdhci::DiskDev;

    fn device_ranges() -> &'static [(u64, u64)] {
        DEVICE_RANGES
    }

    fn early_init() {
        uart::init_pi5();
    }

    fn start_secondary(cpu: usize, entry: u64, entry_el: u64) {
        smp::start_secondary(cpu, entry, entry_el);
    }

    fn el2_blocks_secondaries() -> bool {
        // PSCI on this SoC is SMC. SMC from EL1 goes to EL3, not to EL2.
        false
    }

    fn default_network() -> NetworkMode {
        default_network()
    }

    fn probe_nic() -> Option<Self::Nic> {
        nic::probe()
    }

    fn nic_name() -> &'static str {
        "RP1"
    }

    fn packet_counts() -> (u32, u32) {
        (0, 0)
    }

    fn retarget_nic(_cpu: usize) {}

    fn probe_disk() -> bool {
        sdhci::probe()
    }

    fn disk() -> Self::Disk {
        sdhci::DiskDev
    }

    fn handle_irq(id: u32) -> IrqHit {
        if id == SD_IRQ {
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

    /// RP1 hotplug is not wired. The process stays idle.
    fn hdmi_hpd() -> i64 {
        0
    }

    fn hdmi_mailbox(words: &mut [u32]) -> i64 {
        let mut i = 0;
        while i < MBOX_CANDIDATES.len() {
            let rc = mbox::run(MBOX_CANDIDATES[i], words);
            if rc != -1 {
                return rc;
            }
            i += 1;
        }
        -1
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
