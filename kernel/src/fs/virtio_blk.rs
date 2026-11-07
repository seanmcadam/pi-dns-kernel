//! VirtIO block device for QEMU `virtio-blk-device`.
//!
//! One request is in flight at a time. Each request is a three-descriptor
//! chain: a 16-byte header, a 512-byte sector, and a status byte. All three
//! buffers, and the descriptor/avail/used rings, live in the non-cacheable
//! `.dma` section. Sector bytes from the stack are copied into that buffer
//! before the descriptor is published.
//!
//! QEMU 8.2 defaults virtio-mmio `ioeventfd` on, so a queue notify returns
//! before the request finishes and another thread completes it. The wait is
//! therefore a fraction of a second of `cntpct`, not a short spin. A timeout
//! resets the device before the next request: leaving the old one in flight
//! and reusing its descriptors makes every later status read look like
//! `blk io error`.

use core::ptr::{addr_of_mut, read_volatile, write_volatile};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use spin::Mutex;

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

#[link_section = ".dma"]
static mut QUEUE: QueueArea = QueueArea {
    bytes: [0; QUEUE_BYTES],
};
#[link_section = ".dma"]
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
    true
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
        let mut dev = Self {
            base,
            capacity: 0,
            seen: 0,
        };
        if let Err(err) = dev.program_device() {
            log::warn!("VirtIO blk setup failed: {err}");
            return None;
        }

        let lo = read_reg(base, REG_CONFIG) as u64;
        let hi = read_reg(base, REG_CONFIG + 4) as u64;
        let capacity = lo | (hi << 32);
        if capacity == 0 {
            log::warn!("VirtIO blk reported zero capacity");
            return None;
        }
        dev.capacity = capacity;
        Some(dev)
    }

    /// Negotiate features and install a zeroed queue. `seen` matches the
    /// used index, which the reset leaves at 0.
    fn program_device(&mut self) -> Result<(), &'static str> {
        let base = self.base;
        let version = read_reg(base, REG_VERSION);
        let legacy = version < 2;
        write_reg(base, REG_STATUS, 0);
        dsb();

        let mut status = STATUS_ACKNOWLEDGE;
        write_reg(base, REG_STATUS, status);
        status |= STATUS_DRIVER;
        write_reg(base, REG_STATUS, status);

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
                return Err("blk features rejected");
            }
        }

        unsafe {
            core::ptr::write_bytes(addr_of_mut!(QUEUE) as *mut u8, 0, QUEUE_BYTES);
            write_volatile((*addr_of_mut!(IO)).status.as_mut_ptr(), 0);
        }
        if !install_queue(base, addr_of_mut!(QUEUE), legacy) {
            return Err("blk queue rejected");
        }
        status |= STATUS_DRIVER_OK;
        write_reg(base, REG_STATUS, status);
        dsb();
        self.seen = 0;
        Ok(())
    }

    fn read_sector(&mut self, sector: u64, out: &mut [u8; 512]) -> Result<(), &'static str> {
        self.submit(BLK_T_IN, sector, None)?;
        unsafe {
            let data = (*addr_of_mut!(IO)).data.as_ptr();
            // Drop a stale cache line before copying out what the device wrote.
            invalidate_dma(data, 512);
            dsb();
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
        for attempt in 0..2 {
            match self.submit_once(kind, sector, data) {
                Ok(()) => return Ok(()),
                Err(err) if attempt == 0 && (err == "blk timeout" || err == "blk io error") => {
                    log::warn!("VirtIO {err}, resetting queue");
                    self.program_device()?;
                }
                Err(err) => return Err(err),
            }
        }
        Err("blk io error")
    }

    fn submit_once(
        &mut self,
        kind: u32,
        sector: u64,
        data: Option<&[u8; 512]>,
    ) -> Result<(), &'static str> {
        let q = addr_of_mut!(QUEUE);
        let io = addr_of_mut!(IO);
        // A request that completed after we gave up must not be reported as
        // the result of the request we are about to publish.
        self.absorb_used(q);

        unsafe {
            let header = &mut (*io).header;
            header[..4].copy_from_slice(&kind.to_le_bytes());
            header[4..8].fill(0);
            header[8..16].copy_from_slice(&sector.to_le_bytes());
            if let Some(data) = data {
                (*io).data.copy_from_slice(data);
            }
            write_volatile((*io).status.as_mut_ptr(), 0xff);

            let header_addr = crate::addr::virt_to_phys((*io).header.as_ptr() as u64);
            let data_addr = crate::addr::virt_to_phys((*io).data.as_ptr() as u64);
            let status_addr = crate::addr::virt_to_phys((*io).status.as_ptr() as u64);
            let data_flags = if kind == BLK_T_IN {
                VIRTQ_DESC_F_NEXT | VIRTQ_DESC_F_WRITE
            } else {
                VIRTQ_DESC_F_NEXT
            };
            // Descriptors name only the .dma request, never a stack/heap sector.
            write_desc(q, 0, header_addr, 16, VIRTQ_DESC_F_NEXT, 1);
            write_desc(q, 1, data_addr, 512, data_flags, 2);
            write_desc(q, 2, status_addr, 1, VIRTQ_DESC_F_WRITE, 0);

            let avail_i = read_volatile(avail_idx(q));
            let slot = (avail_i as usize) % QSIZE;
            write_volatile(avail_ring(q, slot), 0);
            dsb();
            write_volatile(avail_idx(q), avail_i.wrapping_add(1));
            // Clean what the device reads. Do not clean the used ring: that
            // would write a stale cache line back over the device's update.
            clean_dma(q as *const u8, AVAIL_OFF + 64);
            clean_dma((*io).header.as_ptr(), 16 + 512 + 1);
            dsb();
            write_reg(self.base, REG_QUEUE_NOTIFY, 0);
            dsb();
        }

        self.wait_complete(q, io)
    }

    /// If the device already moved the used index, remember that value.
    fn absorb_used(&mut self, q: *mut QueueArea) {
        invalidate_dma(unsafe { (q as *const u8).add(USED_OFF) }, 16);
        dsb();
        let used = unsafe { read_volatile(used_idx(q)) };
        let advanced = used.wrapping_sub(self.seen);
        if advanced > 0 && advanced <= QSIZE as u16 {
            self.seen = used;
        }
    }

    fn wait_complete(
        &mut self,
        q: *mut QueueArea,
        io: *mut IoBufs,
    ) -> Result<(), &'static str> {
        let start = crate::timer::get_ticks();
        let freq = timer_freq();
        // Half a second of virtual time. ioeventfd completion runs on another
        // host thread; a couple million spins loses that race when the host
        // is busy. A missing counter falls back to a much larger spin cap.
        let budget = freq / 2;
        let mut spins: u32 = 0;
        loop {
            invalidate_dma(unsafe { (q as *const u8).add(USED_OFF) }, 16);
            let status_ptr = unsafe { (*io).status.as_ptr() };
            invalidate_dma(status_ptr, 1);
            dsb();
            let used = unsafe { read_volatile(used_idx(q)) };
            let advanced = used.wrapping_sub(self.seen);
            if advanced > 0 && advanced <= QSIZE as u16 {
                let status = unsafe { read_volatile(status_ptr) };
                // 0xff is the sentinel we stored. The device writes 0, 1, or 2.
                // A moved used index with the sentinel still in place means the
                // status store is not visible yet, or this is a leftover index.
                if status != 0xff {
                    self.seen = used;
                    if status != 0 {
                        return Err("blk io error");
                    }
                    return Ok(());
                }
            }
            spins = spins.wrapping_add(1);
            let timed_out = if budget == 0 {
                spins > 50_000_000
            } else {
                crate::timer::get_ticks().wrapping_sub(start) >= budget
            };
            if timed_out {
                return Err("blk timeout");
            }
            core::hint::spin_loop();
        }
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

fn dsb() {
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

fn clean_dma(addr: *const u8, len: usize) {
    for_each_line(addr, len, |line| unsafe {
        core::arch::asm!(
            "dc cvac, {line}",
            line = in(reg) line,
            options(nostack, preserves_flags)
        );
    });
}

fn invalidate_dma(addr: *const u8, len: usize) {
    for_each_line(addr, len, |line| unsafe {
        core::arch::asm!(
            "dc ivac, {line}",
            line = in(reg) line,
            options(nostack, preserves_flags)
        );
    });
}

fn for_each_line(addr: *const u8, len: usize, mut op: impl FnMut(usize)) {
    let mut line = (addr as usize) & !63;
    let end = (addr as usize).saturating_add(len);
    while line < end {
        op(line);
        line = line.saturating_add(64);
    }
}

fn timer_freq() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {freq}, cntfrq_el0", freq = out(reg) freq);
    }
    freq
}
