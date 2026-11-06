//! VirtIO block device for QEMU `virtio-blk-device`.
//!
//! One request is in flight at a time. Each request is a three-descriptor
//! chain: a 16-byte header, a 512-byte sector, and a status byte.

use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use spin::Mutex;

const VIRTIO_MMIO_BASE: usize = 0x0a00_0000;
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
const VIRTIO_F_VERSION_1: u32 = 1;

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;
const BLK_T_IN: u32 = 0;
const BLK_T_OUT: u32 = 1;

const QSIZE: usize = 8;
const QUEUE_BYTES: usize = 8192;
const AVAIL_OFF: usize = 16 * QSIZE;
const USED_OFF: usize = 4096;
const MAGIC: u32 = 0x7472_6976;

#[repr(C, align(4096))]
struct QueueArea {
    bytes: [u8; QUEUE_BYTES],
}

#[repr(C, align(16))]
struct IoBufs {
    header: [u8; 16],
    data: [u8; 512],
    status: [u8; 1],
}

#[repr(C)]
struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

static mut QUEUE: QueueArea = QueueArea {
    bytes: [0; QUEUE_BYTES],
};
static mut IO: IoBufs = IoBufs {
    header: [0; 16],
    data: [0; 512],
    status: [0],
};

struct VirtioBlk {
    base: usize,
    capacity: u64,
    seen: u16,
}

static DISK: Mutex<Option<VirtioBlk>> = Mutex::new(None);
static READY: AtomicBool = AtomicBool::new(false);

pub struct DiskDev;

pub fn probe() -> bool {
    let Some(mut dev) = VirtioBlk::new() else {
        return false;
    };
    let mut sector = [0u8; 512];
    if dev.read_sector(0, &mut sector).is_ok()
        && sector[510] == 0x55
        && sector[511] == 0xAA
    {
        log::info!("VirtIO blk MBR ok, {} sectors", dev.capacity);
    } else {
        log::info!("VirtIO blk ready, {} sectors", dev.capacity);
    }
    *DISK.lock() = Some(dev);
    READY.store(true, Ordering::Relaxed);
    true
}

pub fn is_ready() -> bool {
    READY.load(Ordering::Relaxed)
}

impl BlockDevice for DiskDev {
    type Error = &'static str;

    fn read(
        &self,
        blocks: &mut [Block],
        start_block_idx: BlockIdx,
        _reason: &str,
    ) -> Result<(), Self::Error> {
        let mut disk = DISK.lock();
        let disk = disk.as_mut().ok_or("no disk")?;
        for (index, block) in blocks.iter_mut().enumerate() {
            let sector = start_block_idx.0 as u64 + index as u64;
            disk.read_sector(sector, &mut block.contents)?;
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        let mut disk = DISK.lock();
        let disk = disk.as_mut().ok_or("no disk")?;
        for (index, block) in blocks.iter().enumerate() {
            let sector = start_block_idx.0 as u64 + index as u64;
            disk.write_sector(sector, &block.contents)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let disk = DISK.lock();
        let disk = disk.as_ref().ok_or("no disk")?;
        Ok(BlockCount(disk.capacity.min(u32::MAX as u64) as u32))
    }
}

impl VirtioBlk {
    fn new() -> Option<Self> {
        let base = find_blk_device()?;
        let version = read_reg(base, REG_VERSION);
        log::info!("VirtIO blk version {}", version);
        write_reg(base, REG_STATUS, 0);

        let mut status = STATUS_ACKNOWLEDGE;
        write_reg(base, REG_STATUS, status);
        status |= STATUS_DRIVER;
        write_reg(base, REG_STATUS, status);

        let _dev_lo = device_features(base, 0);
        let dev_hi = device_features(base, 1);
        set_driver_features(base, 0, 0);
        let drv_hi = if version >= 2 {
            dev_hi & VIRTIO_F_VERSION_1
        } else {
            0
        };
        set_driver_features(base, 1, drv_hi);

        if version >= 2 {
            status |= STATUS_FEATURES_OK;
            write_reg(base, REG_STATUS, status);
            if read_reg(base, REG_STATUS) & STATUS_FEATURES_OK == 0 {
                log::warn!("VirtIO blk rejected features");
                write_reg(base, REG_STATUS, 128);
                return None;
            }
        }

        unsafe {
            core::ptr::write_bytes(addr_of_mut!(QUEUE) as *mut u8, 0, QUEUE_BYTES);
        }
        let legacy = version < 2;
        if !install_queue(base, addr_of_mut!(QUEUE), legacy) {
            log::warn!("VirtIO blk queue rejected");
            return None;
        }

        status |= STATUS_DRIVER_OK;
        write_reg(base, REG_STATUS, status);

        let lo = read_reg(base, REG_CONFIG) as u64;
        let hi = read_reg(base, REG_CONFIG + 4) as u64;
        let capacity = lo | (hi << 32);
        if capacity == 0 {
            log::warn!("VirtIO blk reported zero capacity");
            return None;
        }
        Some(Self {
            base,
            capacity,
            seen: 0,
        })
    }

    fn read_sector(&mut self, sector: u64, out: &mut [u8; 512]) -> Result<(), &'static str> {
        self.submit(BLK_T_IN, sector, None)?;
        unsafe {
            out.copy_from_slice(&(*addr_of_mut!(IO)).data);
        }
        Ok(())
    }

    fn write_sector(&mut self, sector: u64, data: &[u8; 512]) -> Result<(), &'static str> {
        self.submit(BLK_T_OUT, sector, Some(data))
    }

    fn submit(
        &mut self,
        kind: u32,
        sector: u64,
        data: Option<&[u8; 512]>,
    ) -> Result<(), &'static str> {
        unsafe {
            let io = addr_of_mut!(IO);
            let header = &mut (*io).header;
            header[..4].copy_from_slice(&kind.to_le_bytes());
            header[4..8].fill(0);
            header[8..16].copy_from_slice(&sector.to_le_bytes());
            if let Some(data) = data {
                (*io).data.copy_from_slice(data);
            }
            (*io).status[0] = 0xff;

            let q = addr_of_mut!(QUEUE);
            let data_flags = if kind == BLK_T_IN {
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE
            } else {
                VIRTQ_DESC_F_NEXT
            };
            write_desc(q, 0, (*io).header.as_ptr() as u64, 16, VIRTQ_DESC_F_NEXT, 1);
            write_desc(q, 1, (*io).data.as_ptr() as u64, 512, data_flags, 2);
            write_desc(
                q,
                2,
                (*io).status.as_ptr() as u64,
                1,
                VIRTQ_DESC_F_WRITE,
                0,
            );

            let avail_i = read_volatile(avail_idx(q));
            let slot = (avail_i as usize) % QSIZE;
            write_volatile(avail_ring(q, slot), 0);
            barrier();
            write_volatile(avail_idx(q), avail_i.wrapping_add(1));
            barrier();
            write_reg(self.base, REG_QUEUE_NOTIFY, 0);

            let mut spins = 0;
            while read_volatile(used_idx(q)) == self.seen {
                spins += 1;
                if spins > 2_000_000 {
                    return Err("blk timeout");
                }
                core::hint::spin_loop();
            }
            self.seen = self.seen.wrapping_add(1);
            barrier();
            if (*io).status[0] != 0 {
                return Err("blk io error");
            }
        }
        Ok(())
    }
}

fn find_blk_device() -> Option<usize> {
    for slot in 0..VIRTIO_MMIO_SLOTS {
        let base = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_STRIDE;
        if read_reg(base, REG_MAGIC) != MAGIC {
            continue;
        }
        if read_reg(base, REG_DEVICE_ID) == 2 {
            return Some(base);
        }
    }
    None
}

fn install_queue(base: usize, q: *mut QueueArea, legacy: bool) -> bool {
    write_reg(base, REG_QUEUE_SEL, 0);
    let max = read_reg(base, REG_QUEUE_NUM_MAX);
    if max < QSIZE as u32 {
        return false;
    }
    write_reg(base, REG_QUEUE_NUM, QSIZE as u32);
    let q_addr = q as u64;
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

fn write_desc(q: *mut QueueArea, index: usize, addr: u64, len: u32, flags: u16, next: u16) {
    unsafe {
        let desc = (q as *mut u8).add(index * 16) as *mut Desc;
        write_volatile(
            desc,
            Desc {
                addr,
                len,
                flags,
                next,
            },
        );
    }
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

fn device_features(base: usize, sel: u32) -> u32 {
    write_reg(base, REG_DEVICE_FEATURES_SEL, sel);
    read_reg(base, REG_DEVICE_FEATURES)
}

fn set_driver_features(base: usize, sel: u32, value: u32) {
    write_reg(base, REG_DRIVER_FEATURES_SEL, sel);
    write_reg(base, REG_DRIVER_FEATURES, value);
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
