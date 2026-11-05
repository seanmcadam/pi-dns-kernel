use alloc::vec::Vec;
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Qemu,  // QEMU virtio or other emulated device
    Genet, // BCM2711 GENET (real Pi 4 hardware)
}

pub fn detect_device_type() -> DeviceType {
    // For now, default to QEMU for testing
    // Real detection would probe hardware registers
    DeviceType::Qemu
}

pub fn create_device(device_type: DeviceType) -> Option<EthernetDevice> {
    match device_type {
        DeviceType::Qemu => Some(EthernetDevice::new_qemu()),
        DeviceType::Genet => {
            log::warn!("GENET driver not yet implemented");
            None
        }
    }
}

#[derive(Clone)]
pub struct EthernetDevice {
    mtu: usize,
    mac: [u8; 6],
}

impl EthernetDevice {
    pub fn new_qemu() -> Self {
        Self {
            mtu: 1500,
            mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
}

impl Device for EthernetDevice {
    type RxToken<'a>
        = DummyRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = DummyTxToken
    where
        Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // TODO: Implement actual packet reception
        None
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // TODO: Implement actual packet transmission
        Some(DummyTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = self.mtu;
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct DummyRxToken;

impl RxToken for DummyRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut [])
    }
}

pub struct DummyTxToken;

impl TxToken for DummyTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = Vec::with_capacity(len);
        buffer.resize(len, 0);
        let result = f(&mut buffer);
        // TODO: Actually send the packet
        result
    }
}
