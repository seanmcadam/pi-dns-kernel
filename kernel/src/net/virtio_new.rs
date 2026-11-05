/// VirtIO network driver using virtio-drivers crate
use alloc::vec::Vec;
use core::ptr::NonNull;
use log;
use smoltcp::phy::{self, Device, DeviceCapabilities};
use smoltcp::time::Instant;
use spin::Mutex;
use virtio_drivers::{
    device::net::{VirtIONet, RxBuffer, TxBuffer},
    transport::mmio::{MmioTransport, VirtIOHeader},
};
use crate::virtio_hal::VirtioHalImpl;

const QUEUE_SIZE: usize = 16;
const BUFFER_SIZE: usize = 2048;

pub struct VirtioNetDriver {
    inner: Mutex<VirtIONet<VirtioHalImpl, MmioTransport, QUEUE_SIZE>>,
    mac: [u8; 6],
}

impl VirtioNetDriver {
    pub fn new(base_addr: usize) -> Result<Self, &'static str> {
        log::info!("VirtIO: Initializing at {:#x}", base_addr);
        
        let header = NonNull::new(base_addr as *mut VirtIOHeader)
            .ok_or("Invalid MMIO address")?;
        
        log::info!("VirtIO: Creating MMIO transport");
        let transport = unsafe { MmioTransport::new(header) }
            .map_err(|e| {
                log::error!("VirtIO: Transport creation failed: {:?}", e);
                "Failed to create MMIO transport"
            })?;
        
        log::info!("VirtIO: Creating network device");
        let net = VirtIONet::<VirtioHalImpl, _, QUEUE_SIZE>::new(transport, BUFFER_SIZE)
            .map_err(|e| {
                log::error!("VirtIO: Device creation failed: {:?}", e);
                "Failed to create VirtIO network device"
            })?;
        
        let mac = net.mac_address();
        log::info!("VirtIO: Initialized! MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        
        Ok(VirtioNetDriver {
            inner: Mutex::new(net),
            mac,
        })
    }
    
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
}

pub struct VirtioRxToken {
    buffer: RxBuffer,
    driver: *const VirtioNetDriver,
}

unsafe impl Send for VirtioRxToken {}

pub struct VirtioTxToken<'a> {
    driver: &'a VirtioNetDriver,
}

impl phy::RxToken for VirtioRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = f(self.buffer.packet_mut());
        
        // Recycle buffer
        unsafe {
            let driver = &*self.driver;
            let mut inner = driver.inner.lock();
            let _ = inner.recycle_rx_buffer(self.buffer);
        }
        
        result
    }
}

impl<'a> phy::TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut inner = self.driver.inner.lock();
        let mut tx_buf = inner.new_tx_buffer(len);
        let result = f(tx_buf.packet_mut());
        
        if let Err(e) = inner.send(tx_buf) {
            log::warn!("TX send failed: {:?}", e);
        }
        
        result
    }
}

impl Device for VirtioNetDriver {
    type RxToken<'a> = VirtioRxToken where Self: 'a;
    type TxToken<'a> = VirtioTxToken<'a> where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut inner = self.inner.lock();
        
        match inner.receive() {
            Ok(rx_buf) => {
                log::debug!("RX: Got packet, {} bytes", rx_buf.packet_len());
                Some((
                    VirtioRxToken { 
                        buffer: rx_buf,
                        driver: self as *const _,
                    },
                    VirtioTxToken { driver: self },
                ))
            }
            Err(_) => None,
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        let inner = self.inner.lock();
        if inner.can_send() {
            drop(inner);
            Some(VirtioTxToken { driver: self })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.max_burst_size = Some(1);
        caps
    }
}
