use smoltcp::phy::{Device, DeviceCapabilities};

#[cfg(not(feature = "pi4"))]
use super::virtio::VirtioNetDevice;
#[cfg(feature = "pi4")]
use super::genet::GenetDevice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    #[cfg(not(feature = "pi4"))]
    Virtio,
    #[cfg(feature = "pi4")]
    Genet,
}

pub fn detect_device_type() -> DeviceType {
    #[cfg(feature = "pi4")]
    {
        DeviceType::Genet
    }
    #[cfg(not(feature = "pi4"))]
    {
        DeviceType::Virtio
    }
}

pub enum EthernetDevice {
    #[cfg(not(feature = "pi4"))]
    Virtio(VirtioNetDevice),
    #[cfg(feature = "pi4")]
    Genet(GenetDevice),
}

impl EthernetDevice {
    #[cfg(not(feature = "pi4"))]
    pub fn new_virtio() -> Option<Self> {
        VirtioNetDevice::new().map(EthernetDevice::Virtio)
    }

    #[cfg(feature = "pi4")]
    pub fn new_genet() -> Option<Self> {
        GenetDevice::new().map(EthernetDevice::Genet)
    }

    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            #[cfg(not(feature = "pi4"))]
            EthernetDevice::Virtio(dev) => dev.mac_address(),
            #[cfg(feature = "pi4")]
            EthernetDevice::Genet(dev) => dev.mac_address(),
        }
    }
}

impl Device for EthernetDevice {
    type RxToken<'a> = EthernetRxToken<'a> where Self: 'a;
    type TxToken<'a> = EthernetTxToken<'a> where Self: 'a;

    fn receive(&mut self, timestamp: smoltcp::time::Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self {
            #[cfg(not(feature = "pi4"))]
            EthernetDevice::Virtio(dev) => dev.receive(timestamp).map(|(rx, tx)| {
                (EthernetRxToken::Virtio(rx), EthernetTxToken::Virtio(tx))
            }),
            #[cfg(feature = "pi4")]
            EthernetDevice::Genet(dev) => dev.receive(timestamp).map(|(rx, tx)| {
                (EthernetRxToken::Genet(rx), EthernetTxToken::Genet(tx))
            }),
        }
    }

    fn transmit(&mut self, timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        match self {
            #[cfg(not(feature = "pi4"))]
            EthernetDevice::Virtio(dev) => dev.transmit(timestamp).map(EthernetTxToken::Virtio),
            #[cfg(feature = "pi4")]
            EthernetDevice::Genet(dev) => dev.transmit(timestamp).map(EthernetTxToken::Genet),
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        match self {
            #[cfg(not(feature = "pi4"))]
            EthernetDevice::Virtio(dev) => dev.capabilities(),
            #[cfg(feature = "pi4")]
            EthernetDevice::Genet(dev) => dev.capabilities(),
        }
    }
}

pub enum EthernetRxToken<'a> {
    #[cfg(not(feature = "pi4"))]
    Virtio(<VirtioNetDevice as Device>::RxToken<'a>),
    #[cfg(feature = "pi4")]
    Genet(<GenetDevice as Device>::RxToken<'a>),
}

impl<'a> smoltcp::phy::RxToken for EthernetRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            #[cfg(not(feature = "pi4"))]
            EthernetRxToken::Virtio(token) => token.consume(f),
            #[cfg(feature = "pi4")]
            EthernetRxToken::Genet(token) => token.consume(f),
        }
    }
}

pub enum EthernetTxToken<'a> {
    #[cfg(not(feature = "pi4"))]
    Virtio(<VirtioNetDevice as Device>::TxToken<'a>),
    #[cfg(feature = "pi4")]
    Genet(<GenetDevice as Device>::TxToken<'a>),
}

impl<'a> smoltcp::phy::TxToken for EthernetTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match self {
            #[cfg(not(feature = "pi4"))]
            EthernetTxToken::Virtio(token) => token.consume(len, f),
            #[cfg(feature = "pi4")]
            EthernetTxToken::Genet(token) => token.consume(len, f),
        }
    }
}
