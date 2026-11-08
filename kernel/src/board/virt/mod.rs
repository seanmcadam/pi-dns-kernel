//! QEMU `virt` with GICv2, PL011, virtio-net, and virtio-blk.
//!
//! The link script is `link.ld` in this directory. The raw image is loaded
//! at physical `0x40080000` and linked at `0xFFFF000040080000`.

mod smp;
mod virtio_blk;
mod virtio_net;

use crate::board::{Board, IrqHit};
use crate::net::NetworkMode;

pub struct Virt;

const DEVICE_RANGES: &[(u64, u64)] = &[(0x0800_0000, 0x0c00_0000)];

impl Board for Virt {
    const NAME: &'static str = "virt";
    const GICD_PHYS: u64 = 0x0800_0000;
    const GICC_PHYS: u64 = 0x0801_0000;
    const UART_PHYS: u64 = 0x0900_0000;
    const TIMER_IRQ: u32 = 27;
    const RAM_START: u64 = 0x4000_0000;
    const RAM_END: u64 = 0x4000_0000 + 512 * 1024 * 1024;

    type Nic = virtio_net::VirtioNetDevice;
    type Disk = virtio_blk::DiskDev;

    fn device_ranges() -> &'static [(u64, u64)] {
        DEVICE_RANGES
    }

    fn early_init() {}

    fn start_secondary(cpu: usize, entry: u64, entry_el: u64) {
        smp::start_secondary(cpu, entry, entry_el);
    }

    fn el2_blocks_secondaries() -> bool {
        smp::el2_blocks_psci()
    }

    fn default_network() -> NetworkMode {
        default_network()
    }

    fn probe_nic() -> Option<Self::Nic> {
        virtio_net::VirtioNetDevice::new()
    }

    fn nic_name() -> &'static str {
        "Virtio"
    }

    fn packet_counts() -> (u32, u32) {
        virtio_net::packet_counts()
    }

    fn packet_bytes() -> (u64, u64) {
        virtio_net::packet_bytes()
    }

    fn retarget_nic(cpu: usize) {
        virtio_net::retarget(cpu);
    }

    fn probe_disk() -> bool {
        virtio_blk::probe()
    }

    fn disk() -> Self::Disk {
        virtio_blk::DiskDev
    }

    fn handle_irq(id: u32) -> IrqHit {
        if virtio_net::take_irq(id) {
            return IrqHit::Net;
        }
        if virtio_blk::take_irq(id) {
            return IrqHit::Block;
        }
        IrqHit::None
    }

    fn power(reset: bool) -> bool {
        smp::system_power(reset)
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
