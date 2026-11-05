use smoltcp::phy::{Device, DeviceCapabilities};
use super::virtio_new::VirtioNetDriver;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Virtio,    // QEMU virtio-net (virt machine)
    Genet,     // BCM2711 GENET (real Pi 4 hardware)
}

pub fn detect_device_type() -> DeviceType {
    // For now, always try VirtIO (will be in QEMU virt machine)
    // Real detection would probe device tree or hardware registers
    DeviceType::Virtio
}

pub enum EthernetDevice {
    Virtio(VirtioNetDriver),
    // Genet will go here when implemented
}

impl EthernetDevice {
    pub fn new_virtio(base_addr: usize) -> Result<Self, &'static str> {
        VirtioNetDriver::new(base_addr).map(EthernetDevice::Virtio)
    }
    
    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            EthernetDevice::Virtio(dev) => dev.mac_address(),
        }
    }
}

impl Device for EthernetDevice {
    type RxToken<'a> = EthernetRxToken<'a> where Self: 'a;
    type TxToken<'a> = EthernetTxToken<'a> where Self: 'a;
    
    fn receive(&mut self, timestamp: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self {
            EthernetDevice::Virtio(dev) => {
                dev.receive(timestamp).map(|(rx, tx)| {
                    (EthernetRxToken::Virtio(rx), EthernetTxToken::Virtio(tx))
                })
            }
        }
    }
    
    fn transmit(&mut self, timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        match self {
            EthernetDevice::Virtio(dev) => {
                dev.transmit(timestamp).map(EthernetTxToken::Virtio)
            }
        }
    }
    
    fn capabilities(&self) -> DeviceCapabilities {
        match self {
            EthernetDevice::Virtio(dev) => dev.capabilities(),
        }
    }
}

pub enum EthernetRxToken<'a> {
    Virtio(<VirtioNetDriver as Device>::RxToken<'a>),
}

impl<'a> smoltcp::phy::RxToken for EthernetRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            EthernetRxToken::Virtio(token) => token.consume(f),
        }
    }
}

pub enum EthernetTxToken<'a> {
    Virtio(<VirtioNetDriver as Device>::TxToken<'a>),
}

impl<'a> smoltcp::phy::TxToken for EthernetTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            EthernetTxToken::Virtio(token) => token.consume(len, f),
        }
    }
}
