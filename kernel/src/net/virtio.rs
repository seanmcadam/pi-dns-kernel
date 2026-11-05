/// VirtIO Network Device Driver for QEMU
/// 
/// Implements VirtIO 1.0 MMIO transport for network device.
/// Based on VirtIO specification and QEMU virt machine layout.

use core::ptr::{read_volatile, write_volatile};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use alloc::{vec::Vec, collections::VecDeque};
use spin::Mutex;

// VirtIO MMIO registers (virt machine: 0x0a000000 base)
const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;
const VIRTIO_MMIO_MAGIC_OFFSET: usize = 0x000;
const VIRTIO_MMIO_VERSION_OFFSET: usize = 0x004;
const VIRTIO_MMIO_DEVICE_ID_OFFSET: usize = 0x008;
const VIRTIO_MMIO_VENDOR_ID_OFFSET: usize = 0x00c;
const VIRTIO_MMIO_DEVICE_FEATURES_OFFSET: usize = 0x010;
const VIRTIO_MMIO_DRIVER_FEATURES_OFFSET: usize = 0x020;
const VIRTIO_MMIO_QUEUE_SEL_OFFSET: usize = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX_OFFSET: usize = 0x034;
const VIRTIO_MMIO_QUEUE_NUM_OFFSET: usize = 0x038;
const VIRTIO_MMIO_QUEUE_READY_OFFSET: usize = 0x044;
const VIRTIO_MMIO_QUEUE_NOTIFY_OFFSET: usize = 0x050;
const VIRTIO_MMIO_STATUS_OFFSET: usize = 0x070;
const VIRTIO_MMIO_QUEUE_DESC_LOW_OFFSET: usize = 0x080;
const VIRTIO_MMIO_QUEUE_DESC_HIGH_OFFSET: usize = 0x084;
const VIRTIO_MMIO_QUEUE_AVAIL_LOW_OFFSET: usize = 0x090;
const VIRTIO_MMIO_QUEUE_AVAIL_HIGH_OFFSET: usize = 0x094;
const VIRTIO_MMIO_QUEUE_USED_LOW_OFFSET: usize = 0x0a0;
const VIRTIO_MMIO_QUEUE_USED_HIGH_OFFSET: usize = 0x0a4;
const VIRTIO_MMIO_CONFIG_OFFSET: usize = 0x100;

// VirtIO device types
const VIRTIO_DEVICE_TYPE_NETWORK: u32 = 1;

// VirtIO status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u32 = 1;
const VIRTIO_STATUS_DRIVER: u32 = 2;
const VIRTIO_STATUS_DRIVER_OK: u32 = 4;
const VIRTIO_STATUS_FEATURES_OK: u32 = 8;
const VIRTIO_STATUS_FAILED: u32 = 128;

// VirtIO network features
const VIRTIO_NET_F_MAC: u32 = 1 << 5;

// Queue sizes
const QUEUE_SIZE: usize = 256;
const BUFFER_SIZE: usize = 1526; // MTU + header

#[repr(C, align(16))]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C, align(2))]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

#[repr(C, align(4))]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C, align(4))]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; QUEUE_SIZE],
    avail_event: u16,
}

#[repr(C)]
struct VirtioNetHdr {
    flags: u8,
    gso_type: u8,
    hdr_len: u16,
    gso_size: u16,
    csum_start: u16,
    csum_offset: u16,
    num_buffers: u16,
}

impl VirtioNetHdr {
    fn new() -> Self {
        Self {
            flags: 0,
            gso_type: 0,
            hdr_len: 0,
            gso_size: 0,
            csum_start: 0,
            csum_offset: 0,
            num_buffers: 0,
        }
    }
}

struct Virtqueue {
    desc: &'static mut [VirtqDesc; QUEUE_SIZE],
    avail: &'static mut VirtqAvail,
    used: &'static mut VirtqUsed,
    free_head: usize,
    last_used_idx: u16,
}

impl Virtqueue {
    fn new() -> Self {
        // Allocate queue structures
        let desc_ptr = alloc_aligned(
            core::mem::size_of::<[VirtqDesc; QUEUE_SIZE]>(),
            16,
        ) as *mut [VirtqDesc; QUEUE_SIZE];
        
        let avail_ptr = alloc_aligned(
            core::mem::size_of::<VirtqAvail>(),
            2,
        ) as *mut VirtqAvail;
        
        let used_ptr = alloc_aligned(
            core::mem::size_of::<VirtqUsed>(),
            4,
        ) as *mut VirtqUsed;
        
        let desc = unsafe { &mut *desc_ptr };
        let avail = unsafe { &mut *avail_ptr };
        let used = unsafe { &mut *used_ptr };
        
        // Initialize descriptor table with free list
        for i in 0..QUEUE_SIZE {
            desc[i].addr = 0;
            desc[i].len = 0;
            desc[i].flags = 0;
            desc[i].next = if i + 1 < QUEUE_SIZE { (i + 1) as u16 } else { 0 };
        }
        
        // Clear avail and used rings
        avail.flags = 0;
        avail.idx = 0;
        used.flags = 0;
        used.idx = 0;
        
        Self {
            desc,
            avail,
            used,
            free_head: 0,
            last_used_idx: 0,
        }
    }
    
    fn desc_addr(&self) -> u64 {
        self.desc.as_ptr() as u64
    }
    
    fn avail_addr(&self) -> u64 {
        self.avail as *const _ as u64
    }
    
    fn used_addr(&self) -> u64 {
        self.used as *const _ as u64
    }
}

pub struct VirtioNetDevice {
    base_addr: usize,
    mac: [u8; 6],
    rx_queue: Virtqueue,
    tx_queue: Virtqueue,
    rx_buffers: Vec<Vec<u8>>,
    tx_buffers: Vec<Vec<u8>>,
    rx_packets: Mutex<VecDeque<Vec<u8>>>,
}

impl VirtioNetDevice {
    pub fn new() -> Option<Self> {
        let base = VIRTIO_MMIO_BASE;
        
        // Check magic value
        let magic = read_reg(base, VIRTIO_MMIO_MAGIC_OFFSET);
        if magic != 0x74726976 { // "virt"
            log::warn!("VirtIO magic mismatch: 0x{:x}", magic);
            return None;
        }
        
        let version = read_reg(base, VIRTIO_MMIO_VERSION_OFFSET);
        log::info!("VirtIO version: {}", version);
        
        let device_id = read_reg(base, VIRTIO_MMIO_DEVICE_ID_OFFSET);
        if device_id != VIRTIO_DEVICE_TYPE_NETWORK {
            log::warn!("Not a network device: {}", device_id);
            return None;
        }
        
        log::info!("Found VirtIO network device");
        
        // Reset device
        write_reg(base, VIRTIO_MMIO_STATUS_OFFSET, 0);
        
        // Set ACKNOWLEDGE status bit
        write_reg(base, VIRTIO_MMIO_STATUS_OFFSET, VIRTIO_STATUS_ACKNOWLEDGE);
        
        // Set DRIVER status bit
        write_reg(
            base,
            VIRTIO_MMIO_STATUS_OFFSET,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER,
        );
        
        // Read and negotiate features
        let device_features = read_reg(base, VIRTIO_MMIO_DEVICE_FEATURES_OFFSET);
        log::info!("Device features: 0x{:x}", device_features);
        
        let driver_features = VIRTIO_NET_F_MAC;
        write_reg(base, VIRTIO_MMIO_DRIVER_FEATURES_OFFSET, driver_features);
        
        // Set FEATURES_OK
        write_reg(
            base,
            VIRTIO_MMIO_STATUS_OFFSET,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        
        // Verify FEATURES_OK
        let status = read_reg(base, VIRTIO_MMIO_STATUS_OFFSET);
        if (status & VIRTIO_STATUS_FEATURES_OK) == 0 {
            log::error!("Device did not accept features");
            return None;
        }
        
        // Read MAC address
        let mut mac = [0u8; 6];
        for i in 0..6 {
            mac[i] = read_reg(base, VIRTIO_MMIO_CONFIG_OFFSET + i * 4) as u8;
        }
        log::info!("MAC address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
        
        // Initialize virtqueues
        let rx_queue = Virtqueue::new();
        let tx_queue = Virtqueue::new();
        
        // Setup RX queue (0)
        write_reg(base, VIRTIO_MMIO_QUEUE_SEL_OFFSET, 0);
        write_reg(base, VIRTIO_MMIO_QUEUE_NUM_OFFSET, QUEUE_SIZE as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_DESC_LOW_OFFSET, rx_queue.desc_addr() as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_DESC_HIGH_OFFSET, (rx_queue.desc_addr() >> 32) as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_AVAIL_LOW_OFFSET, rx_queue.avail_addr() as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH_OFFSET, (rx_queue.avail_addr() >> 32) as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_USED_LOW_OFFSET, rx_queue.used_addr() as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_USED_HIGH_OFFSET, (rx_queue.used_addr() >> 32) as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_READY_OFFSET, 1);
        
        // Setup TX queue (1)
        write_reg(base, VIRTIO_MMIO_QUEUE_SEL_OFFSET, 1);
        write_reg(base, VIRTIO_MMIO_QUEUE_NUM_OFFSET, QUEUE_SIZE as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_DESC_LOW_OFFSET, tx_queue.desc_addr() as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_DESC_HIGH_OFFSET, (tx_queue.desc_addr() >> 32) as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_AVAIL_LOW_OFFSET, tx_queue.avail_addr() as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_AVAIL_HIGH_OFFSET, (tx_queue.avail_addr() >> 32) as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_USED_LOW_OFFSET, tx_queue.used_addr() as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_USED_HIGH_OFFSET, (tx_queue.used_addr() >> 32) as u32);
        write_reg(base, VIRTIO_MMIO_QUEUE_READY_OFFSET, 1);
        
        // Set DRIVER_OK
        write_reg(
            base,
            VIRTIO_MMIO_STATUS_OFFSET,
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | 
            VIRTIO_STATUS_FEATURES_OK | VIRTIO_STATUS_DRIVER_OK,
        );
        
        let mut device = Self {
            base_addr: base,
            mac,
            rx_queue,
            tx_queue,
            rx_buffers: Vec::new(),
            tx_buffers: Vec::new(),
            rx_packets: Mutex::new(VecDeque::new()),
        };
        
        // Allocate and provide RX buffers
        device.refill_rx();
        
        log::info!("VirtIO network device initialized");
        Some(device)
    }
    
    fn refill_rx(&mut self) {
        // Provide RX buffers to device
        for _ in 0..16 {
            let mut buffer = Vec::with_capacity(BUFFER_SIZE);
            buffer.resize(BUFFER_SIZE, 0);
            
            // TODO: Actually add to RX queue
            self.rx_buffers.push(buffer);
        }
    }
    
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }
    
    fn poll_rx(&mut self) {
        // Check for received packets
        let used_idx = unsafe { read_volatile(&self.rx_queue.used.idx) };
        
        while self.rx_queue.last_used_idx != used_idx {
            let idx = (self.rx_queue.last_used_idx % QUEUE_SIZE as u16) as usize;
            let used_elem = unsafe { read_volatile(&self.rx_queue.used.ring[idx]) };
            
            // Extract packet from buffer
            if let Some(buffer) = self.rx_buffers.get(used_elem.id as usize) {
                let packet_len = used_elem.len as usize;
                if packet_len > core::mem::size_of::<VirtioNetHdr>() {
                    let data_len = packet_len - core::mem::size_of::<VirtioNetHdr>();
                    let packet = buffer[core::mem::size_of::<VirtioNetHdr>()..core::mem::size_of::<VirtioNetHdr>() + data_len].to_vec();
                    self.rx_packets.lock().push_back(packet);
                }
            }
            
            self.rx_queue.last_used_idx = self.rx_queue.last_used_idx.wrapping_add(1);
        }
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken where Self: 'a;
    type TxToken<'a> = VirtioTxToken<'a> where Self: 'a;
    
    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.poll_rx();
        
        let packet = {
            let mut rx_packets = self.rx_packets.lock();
            rx_packets.pop_front()
        };
        
        if let Some(packet) = packet {
            Some((
                VirtioRxToken { packet },
                VirtioTxToken { device: self },
            ))
        } else {
            None
        }
    }
    
    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtioTxToken { device: self })
    }
    
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct VirtioRxToken {
    packet: Vec<u8>,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut packet = self.packet;
        f(&mut packet)
    }
}

pub struct VirtioTxToken<'a> {
    device: &'a mut VirtioNetDevice,
}

impl<'a> TxToken for VirtioTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buffer = Vec::with_capacity(len + core::mem::size_of::<VirtioNetHdr>());
        
        // Add VirtIO net header
        let hdr = VirtioNetHdr::new();
        let hdr_bytes = unsafe {
            core::slice::from_raw_parts(
                &hdr as *const _ as *const u8,
                core::mem::size_of::<VirtioNetHdr>(),
            )
        };
        buffer.extend_from_slice(hdr_bytes);
        
        // Add packet data
        buffer.resize(len + core::mem::size_of::<VirtioNetHdr>(), 0);
        let result = f(&mut buffer[core::mem::size_of::<VirtioNetHdr>()..]);
        
        // TODO: Actually transmit via TX queue
        log::debug!("TX packet: {} bytes", len);
        
        // For now, just notify
        write_reg(self.device.base_addr, VIRTIO_MMIO_QUEUE_NOTIFY_OFFSET, 1);
        
        result
    }
}

fn read_reg(base: usize, offset: usize) -> u32 {
    unsafe { read_volatile((base + offset) as *const u32) }
}

fn write_reg(base: usize, offset: usize, value: u32) {
    unsafe { write_volatile((base + offset) as *mut u32, value) }
}

fn alloc_aligned(size: usize, align: usize) -> usize {
    use alloc::alloc::{alloc, Layout};
    
    let layout = Layout::from_size_align(size, align).unwrap();
    let ptr = unsafe { alloc(layout) };
    
    // Zero the memory
    unsafe {
        core::ptr::write_bytes(ptr, 0, size);
    }
    
    ptr as usize
}
