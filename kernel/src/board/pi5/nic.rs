//! RP1 Ethernet is not driven.
//!
//! On the Pi 5 the MAC is a Cadence GEM (`rp1_eth` in `bcm2712-rpi-5-b.dts`)
//! on the RP1 chip, behind PCIe controller 2. That controller is the x4
//! link at physical `0x1000120000`. Its outbound window for RP1 peripherals
//! is at `0x1f_00000000`, which this kernel does not map.
//!
//! TODO (RP1 / PCIe bring-up, not implemented):
//! - Take `pcie2` out of reset (`rescal`, bridge reset 44) and train the link.
//! - Program the outbound non-prefetchable window at `0x1f_00000000`.
//! - Map that window, enumerate ECAM, and enable the RP1 BAR.
//! - Bring up the RP1 clocks and the Ethernet PHY reset (RP1 GPIO 32).
//! - Drive the Cadence MACB/GEM. MSI goes through the MIP at `0x1000130000`.
//!
//! Until that exists, `probe` returns `None`. The shared boot path logs
//! "No NIC" and still starts the log task.

use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

pub struct Rp1Nic;

impl Rp1Nic {
    pub fn mac_address(&self) -> [u8; 6] {
        [0, 0, 0, 0, 0, 0]
    }
}

pub fn probe() -> Option<Rp1Nic> {
    log::warn!("RP1 Ethernet is behind PCIe and is not driven");
    None
}

pub struct Rp1Rx;

pub struct Rp1Tx;

impl RxToken for Rp1Rx {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut empty = [];
        f(&mut empty)
    }
}

impl TxToken for Rp1Tx {
    fn consume<R, F>(self, _len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut empty = [];
        f(&mut empty)
    }
}

impl Device for Rp1Nic {
    type RxToken<'a> = Rp1Rx where Self: 'a;
    type TxToken<'a> = Rp1Tx where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        None
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps
    }
}
