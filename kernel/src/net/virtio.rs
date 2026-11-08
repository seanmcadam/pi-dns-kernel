/// VirtIO-net over the virtio-mmio transport (QEMU `virtio-net-device`).
///
/// Supports both the modern (MMIO version 2) and legacy (version 1) layouts.
/// RX and TX use a single descriptor per packet: a 10-byte virtio-net header
/// followed by the Ethernet frame. Buffers live in BSS so they are in guest RAM
/// and stay put for the life of the kernel.

use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

const VIRTIO_MMIO_BASE: usize = crate::addr::phys_to_virt(0x0a00_0000) as usize;
const VIRTIO_MMIO_STRIDE: usize = 0x200;
const VIRTIO_MMIO_SLOTS: usize = 32;

const REG_MAGIC: usize = 0x000;
const REG_VERSION: usize = 0x004;
const REG_DEVICE_ID: usize = 0x008;
const REG_DEVICE_FEATURES: usize = 0x010;
const REG_DEVICE_FEATURES_SEL: usize = 0x014;
const REG_DRIVER_FEATURES: usize = 0x020;
const REG_DRIVER_FEATURES_SEL: usize = 0x024;
const REG_GUEST_PAGE_SIZE: usize = 0x028;
const REG_QUEUE_SEL: usize = 0x030;
const REG_QUEUE_NUM_MAX: usize = 0x034;
const REG_QUEUE_NUM: usize = 0x038;
const REG_QUEUE_ALIGN: usize = 0x03c;
const REG_QUEUE_PFN: usize = 0x040;
const REG_QUEUE_READY: usize = 0x044;
const REG_QUEUE_NOTIFY: usize = 0x050;
const REG_STATUS: usize = 0x070;
const REG_QUEUE_DESC_LOW: usize = 0x080;
const REG_QUEUE_DESC_HIGH: usize = 0x084;
const REG_QUEUE_AVAIL_LOW: usize = 0x090;
const REG_QUEUE_AVAIL_HIGH: usize = 0x094;
const REG_QUEUE_USED_LOW: usize = 0x0a0;
const REG_QUEUE_USED_HIGH: usize = 0x0a4;
const REG_CONFIG: usize = 0x100;

const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

const VIRTIO_NET_F_MAC: u32 = 1 << 5;
const VIRTIO_F_VERSION_1: u32 = 1; // bit 32, feature word 1

const VIRTQ_DESC_F_WRITE: u16 = 2;

const QSIZE: usize = 16;
const BUF_SIZE: usize = 2048;
const HDR_LEN: usize = 10;
const QUEUE_BYTES: usize = 8192;
const AVAIL_OFF: usize = 16 * QSIZE; // 256
const USED_OFF: usize = 4096;

const MAGIC: u32 = 0x7472_6976;

static RX_COUNT: AtomicU32 = AtomicU32::new(0);
static TX_COUNT: AtomicU32 = AtomicU32::new(0);

#[repr(C, align(4096))]
struct QueueArea {
    bytes: [u8; QUEUE_BYTES],
}

#[repr(C, align(16))]
struct PacketBufs {
    bufs: [[u8; BUF_SIZE]; QSIZE],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

#[link_section = ".dma"]
static mut RXQ: QueueArea = QueueArea {
    bytes: [0; QUEUE_BYTES],
};
#[link_section = ".dma"]
static mut TXQ: QueueArea = QueueArea {
    bytes: [0; QUEUE_BYTES],
};
#[link_section = ".dma"]
static mut RX_BUFS: PacketBufs = PacketBufs {
    bufs: [[0; BUF_SIZE]; QSIZE],
};
#[link_section = ".dma"]
static mut TX_BUFS: PacketBufs = PacketBufs {
    bufs: [[0; BUF_SIZE]; QSIZE],
};

pub struct VirtioNetDevice {
    base: usize,
    mac: [u8; 6],
    rx_seen: u16,
    tx_seen: u16,
    tx_busy: [bool; QSIZE],
    pending: [Option<(u16, u16)>; QSIZE],
}

pub fn packet_counts() -> (u32, u32) {
    (
        RX_COUNT.load(Ordering::Relaxed),
        TX_COUNT.load(Ordering::Relaxed),
    )
}

impl VirtioNetDevice {
    pub fn new() -> Option<Self> {
        let base = find_net_device()?;
        let version = read_reg(base, REG_VERSION);
        log_num("VirtIO net version ", version as u64);

        write_reg(base, REG_STATUS, 0);

        let mut status = STATUS_ACKNOWLEDGE;
        write_reg(base, REG_STATUS, status);
        status |= STATUS_DRIVER;
        write_reg(base, REG_STATUS, status);

        let dev_lo = device_features(base, 0);
        let dev_hi = device_features(base, 1);
        log_hex("VirtIO features ", ((dev_hi as u64) << 32) | dev_lo as u64);

        let drv_lo = dev_lo & VIRTIO_NET_F_MAC;
        let drv_hi = if version >= 2 {
            dev_hi & VIRTIO_F_VERSION_1
        } else {
            0
        };
        set_driver_features(base, 0, drv_lo);
        set_driver_features(base, 1, drv_hi);

        if version >= 2 {
            status |= STATUS_FEATURES_OK;
            write_reg(base, REG_STATUS, status);
            let got = read_reg(base, REG_STATUS);
            if got & STATUS_FEATURES_OK == 0 {
                uart_log("VirtIO rejected features\r\n");
                write_reg(base, REG_STATUS, 128);
                return None;
            }
        }

        let mac = read_mac(base);
        log_mac(&mac);

        init_queue(addr_of_mut!(RXQ));
        init_queue(addr_of_mut!(TXQ));

        let legacy = version < 2;
        if !install_queue(base, 0, addr_of_mut!(RXQ), legacy) {
            uart_log("VirtIO RX queue rejected\r\n");
            return None;
        }
        if !install_queue(base, 1, addr_of_mut!(TXQ), legacy) {
            uart_log("VirtIO TX queue rejected\r\n");
            return None;
        }

        let mut dev = Self {
            base,
            mac,
            rx_seen: 0,
            tx_seen: 0,
            tx_busy: [false; QSIZE],
            pending: [None; QSIZE],
        };

        for i in 0..QSIZE {
            dev.post_rx(i as u16);
        }
        dev.kick(0);

        status |= STATUS_DRIVER_OK;
        write_reg(base, REG_STATUS, status);
        // Kick again now that the device is allowed to use the queues.
        dev.kick(0);

        uart_log("VirtIO net ready\r\n");
        Some(dev)
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn kick(&self, queue: u32) {
        barrier();
        write_reg(self.base, REG_QUEUE_NOTIFY, queue);
    }

    fn post_rx(&mut self, idx: u16) {
        unsafe {
            let q = addr_of_mut!(RXQ);
            let desc = desc_ptr(q, idx as usize);
            let addr = buf_addr(addr_of_mut!(RX_BUFS), idx as usize);
            write_volatile(
                desc,
                Desc {
                    addr,
                    len: BUF_SIZE as u32,
                    flags: VIRTQ_DESC_F_WRITE,
                    next: 0,
                },
            );
            let avail_i = read_volatile(avail_idx(q));
            let slot = (avail_i as usize) % QSIZE;
            write_volatile(avail_ring(q, slot), idx);
            barrier();
            write_volatile(avail_idx(q), avail_i.wrapping_add(1));
        }
    }

    fn harvest_rx(&mut self) {
        unsafe {
            let q = addr_of_mut!(RXQ);
            let hw = read_volatile(used_idx(q));
            while self.rx_seen != hw {
                let slot = (self.rx_seen as usize) % QSIZE;
                let (id, len) = used_elem(q, slot);
                self.rx_seen = self.rx_seen.wrapping_add(1);
                if (id as usize) >= QSIZE || (len as usize) <= HDR_LEN {
                    if (id as usize) < QSIZE {
                        self.post_rx(id as u16);
                    }
                    continue;
                }
                let frame_len = (len as usize - HDR_LEN) as u16;
                if let Some(slot) = self.pending.iter_mut().find(|s| s.is_none()) {
                    *slot = Some((id as u16, frame_len));
                    RX_COUNT.fetch_add(1, Ordering::Relaxed);
                } else if (id as usize) < QSIZE {
                    self.post_rx(id as u16);
                }
            }
        }
    }

    fn reclaim_tx(&mut self) {
        unsafe {
            let q = addr_of_mut!(TXQ);
            let hw = read_volatile(used_idx(q));
            while self.tx_seen != hw {
                let slot = (self.tx_seen as usize) % QSIZE;
                let (id, _) = used_elem(q, slot);
                self.tx_seen = self.tx_seen.wrapping_add(1);
                if (id as usize) < QSIZE {
                    self.tx_busy[id as usize] = false;
                }
            }
        }
    }

    fn alloc_tx(&self) -> Option<usize> {
        self.tx_busy.iter().position(|b| !*b)
    }

    fn submit_tx(&mut self, idx: usize, total_len: u32) {
        unsafe {
            let q = addr_of_mut!(TXQ);
            let desc = desc_ptr(q, idx);
            let addr = buf_addr(addr_of_mut!(TX_BUFS), idx);
            write_volatile(
                desc,
                Desc {
                    addr,
                    len: total_len,
                    flags: 0,
                    next: 0,
                },
            );
            let avail_i = read_volatile(avail_idx(q));
            let slot = (avail_i as usize) % QSIZE;
            write_volatile(avail_ring(q, slot), idx as u16);
            barrier();
            write_volatile(avail_idx(q), avail_i.wrapping_add(1));
        }
        self.tx_busy[idx] = true;
        self.kick(1);
        TX_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

impl Device for VirtioNetDevice {
    type RxToken<'a> = VirtioRxToken where Self: 'a;
    type TxToken<'a> = VirtioTxToken<'a> where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.reclaim_tx();
        self.harvest_rx();
        if self.alloc_tx().is_none() {
            return None;
        }
        let pos = self.pending.iter().position(|s| s.is_some())?;
        let (idx, len) = self.pending[pos].take()?;
        Some((
            VirtioRxToken { idx, len },
            VirtioTxToken { device: self },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        self.reclaim_tx();
        if self.alloc_tx().is_some() {
            Some(VirtioTxToken { device: self })
        } else {
            None
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(QSIZE);
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct VirtioRxToken {
    idx: u16,
    len: u16,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = unsafe {
            let bufs = &mut *addr_of_mut!(RX_BUFS);
            let frame = bufs.bufs[self.idx as usize].as_mut_ptr().add(HDR_LEN);
            let slice = core::slice::from_raw_parts_mut(frame, self.len as usize);
            f(slice)
        };
        // Hand the buffer back. `repost` needs a device, so do it through the
        // static queue directly and kick queue 0 at the known MMIO base.
        repost_rx_static(self.idx);
        result
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
        let idx = self
            .device
            .alloc_tx()
            .expect("TX token handed out with no free slot");
        let result = unsafe {
            let bufs = &mut *addr_of_mut!(TX_BUFS);
            let buf = &mut bufs.bufs[idx];
            buf[..HDR_LEN].fill(0);
            let frame = &mut buf[HDR_LEN..HDR_LEN + len];
            let result = f(frame);
            result
        };
        self.device.submit_tx(idx, (HDR_LEN + len) as u32);
        result
    }
}

fn repost_rx_static(idx: u16) {
    unsafe {
        let q = addr_of_mut!(RXQ);
        let desc = desc_ptr(q, idx as usize);
        let addr = buf_addr(addr_of_mut!(RX_BUFS), idx as usize);
        write_volatile(
            desc,
            Desc {
                addr,
                len: BUF_SIZE as u32,
                flags: VIRTQ_DESC_F_WRITE,
                next: 0,
            },
        );
        let avail_i = read_volatile(avail_idx(q));
        let slot = (avail_i as usize) % QSIZE;
        write_volatile(avail_ring(q, slot), idx);
        barrier();
        write_volatile(avail_idx(q), avail_i.wrapping_add(1));
    }
    // The device base is the slot we programmed. Re-read it from the first
    // matching register block; the queue notify register is per-device.
    if let Some(base) = cached_base() {
        barrier();
        write_reg(base, REG_QUEUE_NOTIFY, 0);
    }
}

fn find_net_device() -> Option<usize> {
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let base = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
        if read_reg(base, REG_MAGIC) != MAGIC {
            continue;
        }
        let device_id = read_reg(base, REG_DEVICE_ID);
        if device_id == 1 {
            set_cached_base(base);
            return Some(base);
        }
    }
    uart_log("VirtIO net device not found\r\n");
    None
}

static CACHED_BASE: AtomicUsize = AtomicUsize::new(0);

fn set_cached_base(base: usize) {
    CACHED_BASE.store(base, Ordering::Relaxed);
}

fn cached_base() -> Option<usize> {
    let v = CACHED_BASE.load(Ordering::Relaxed);
    if v == 0 {
        None
    } else {
        Some(v)
    }
}

fn install_queue(base: usize, index: u32, q: *mut QueueArea, legacy: bool) -> bool {
    write_reg(base, REG_QUEUE_SEL, index);
    let max = read_reg(base, REG_QUEUE_NUM_MAX);
    if max < QSIZE as u32 {
        log_num("VirtIO queue too small ", max as u64);
        return false;
    }
    write_reg(base, REG_QUEUE_NUM, QSIZE as u32);

    let q_addr = crate::addr::virt_to_phys(q as u64);
    if legacy {
        write_reg(base, REG_GUEST_PAGE_SIZE, 4096);
        write_reg(base, REG_QUEUE_ALIGN, 4096);
        write_reg(base, REG_QUEUE_PFN, (q_addr / 4096) as u32);
        read_reg(base, REG_QUEUE_PFN) != 0
    } else {
        write_reg(base, REG_QUEUE_DESC_LOW, q_addr as u32);
        write_reg(base, REG_QUEUE_DESC_HIGH, (q_addr >> 32) as u32);
        let avail = q_addr + AVAIL_OFF as u64;
        write_reg(base, REG_QUEUE_AVAIL_LOW, avail as u32);
        write_reg(base, REG_QUEUE_AVAIL_HIGH, (avail >> 32) as u32);
        let used = q_addr + USED_OFF as u64;
        write_reg(base, REG_QUEUE_USED_LOW, used as u32);
        write_reg(base, REG_QUEUE_USED_HIGH, (used >> 32) as u32);
        write_reg(base, REG_QUEUE_READY, 1);
        read_reg(base, REG_QUEUE_READY) == 1
    }
}

fn init_queue(q: *mut QueueArea) {
    unsafe {
        core::ptr::write_bytes(q as *mut u8, 0, QUEUE_BYTES);
    }
}

fn desc_ptr(q: *mut QueueArea, index: usize) -> *mut Desc {
    unsafe { (q as *mut u8).add(index * 16) as *mut Desc }
}

fn avail_idx(q: *mut QueueArea) -> *mut u16 {
    unsafe { (q as *mut u8).add(AVAIL_OFF + 2) as *mut u16 }
}

fn avail_ring(q: *mut QueueArea, index: usize) -> *mut u16 {
    unsafe { (q as *mut u8).add(AVAIL_OFF + 4 + index * 2) as *mut u16 }
}

fn used_idx(q: *mut QueueArea) -> *const u16 {
    unsafe { (q as *mut u8).add(USED_OFF + 2) as *const u16 }
}

fn used_elem(q: *mut QueueArea, index: usize) -> (u32, u32) {
    unsafe {
        let p = (q as *mut u8).add(USED_OFF + 4 + index * 8);
        let id = read_volatile(p as *const u32);
        let len = read_volatile(p.add(4) as *const u32);
        (id, len)
    }
}

fn buf_addr(bufs: *mut PacketBufs, index: usize) -> u64 {
    unsafe { crate::addr::virt_to_phys((&mut *bufs).bufs[index].as_mut_ptr() as u64) }
}

fn device_features(base: usize, sel: u32) -> u32 {
    write_reg(base, REG_DEVICE_FEATURES_SEL, sel);
    read_reg(base, REG_DEVICE_FEATURES)
}

fn set_driver_features(base: usize, sel: u32, value: u32) {
    write_reg(base, REG_DRIVER_FEATURES_SEL, sel);
    write_reg(base, REG_DRIVER_FEATURES, value)
}

fn read_mac(base: usize) -> [u8; 6] {
    let w0 = read_reg(base, REG_CONFIG);
    let w1 = read_reg(base, REG_CONFIG + 4);
    [
        w0 as u8,
        (w0 >> 8) as u8,
        (w0 >> 16) as u8,
        (w0 >> 24) as u8,
        w1 as u8,
        (w1 >> 8) as u8,
    ]
}

fn read_reg(base: usize, offset: usize) -> u32 {
    unsafe { read_volatile((base + offset) as *const u32) }
}

fn write_reg(base: usize, offset: usize, value: u32) {
    unsafe { write_volatile((base + offset) as *mut u32, value) }
}

fn barrier() {
    unsafe {
        core::arch::asm!("dmb sy", options(nostack, preserves_flags));
    }
}

fn uart_log(s: &str) {
    unsafe {
        let uart = crate::addr::phys_to_virt(0x0900_0000) as *mut u32;
        for byte in s.bytes() {
            write_volatile(uart, byte as u32);
        }
    }
}

fn log_num(prefix: &str, n: u64) {
    uart_log(prefix);
    uart_dec(n);
    uart_log("\r\n");
}

fn log_hex(prefix: &str, n: u64) {
    uart_log(prefix);
    uart_log("0x");
    uart_hex(n);
    uart_log("\r\n");
}

fn log_mac(mac: &[u8; 6]) {
    uart_log("MAC ");
    for (i, byte) in mac.iter().enumerate() {
        if i != 0 {
            uart_log(":");
        }
        uart_hex8(*byte);
    }
    uart_log("\r\n");
}

fn uart_dec(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 {
        uart_log("0");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    uart_log(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

fn uart_hex(n: u64) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut started = false;
    let mut tmp = [0u8; 1];
    for shift in (0..16).rev() {
        let nib = ((n >> (shift * 4)) & 0xf) as usize;
        if nib != 0 || started || shift == 0 {
            started = true;
            tmp[0] = HEX[nib];
            uart_log(core::str::from_utf8(&tmp).unwrap_or("?"));
        }
    }
}

fn uart_hex8(b: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let tmp = [HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]];
    uart_log(core::str::from_utf8(&tmp).unwrap_or("??"));
}
