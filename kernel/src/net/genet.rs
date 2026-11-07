//! BCM2711 GENET v5 Ethernet (Pi 4, MMIO 0xFD580000).
//!
//! One RX ring and one TX ring (queue 16, the default priority queue) share
//! 32 descriptors each. Frames are placed in static RAM buffers. The 2-byte
//! RX offset matches the controller's IP-header alignment.
//!
//! This driver is compiled into the Pi 4 image and has not been run on hardware.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU32, Ordering};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

const BASE: usize = crate::addr::phys_to_virt(0xFD58_0000) as usize;

const SYS_REV_CTRL: usize = 0x00;
const SYS_RBUF_FLUSH_CTRL: usize = 0x08;
const EXT_RGMII_OOB_CTRL: usize = 0x8c;
const RBUF_CTRL: usize = 0x300;
const RBUF_TBUF_SIZE_CTRL: usize = 0x3b4;
const UMAC_CMD: usize = 0x808;
const UMAC_MAC0: usize = 0x80c;
const UMAC_MAC1: usize = 0x810;
const UMAC_MAX_FRAME_LEN: usize = 0x814;
const UMAC_TX_FLUSH: usize = 0xb34;
const UMAC_MIB_CTRL: usize = 0xd80;
const MDIO_CMD: usize = 0xe14;

const GENET_RX_OFF: usize = 0x2000;
const GENET_TX_OFF: usize = 0x4000;
const DMA_DESC_SIZE: usize = 12;
const TOTAL_DESCS: usize = 256;
const GENET_RDMA_REG_OFF: usize = GENET_RX_OFF + TOTAL_DESCS * DMA_DESC_SIZE;
const GENET_TDMA_REG_OFF: usize = GENET_TX_OFF + TOTAL_DESCS * DMA_DESC_SIZE;
const DEFAULT_Q: usize = 0x10;
const DMA_RING_SIZE: usize = 0x40;
const DMA_RINGS_SIZE: usize = DMA_RING_SIZE * (DEFAULT_Q + 1);
const RDMA_REG_BASE: usize = GENET_RDMA_REG_OFF + DMA_RINGS_SIZE;
const TDMA_REG_BASE: usize = GENET_TDMA_REG_OFF + DMA_RINGS_SIZE;
const RDMA_RING: usize = GENET_RDMA_REG_OFF + DMA_RING_SIZE * DEFAULT_Q;
const TDMA_RING: usize = GENET_TDMA_REG_OFF + DMA_RING_SIZE * DEFAULT_Q;

const DMA_RING_CFG: usize = 0x00;
const DMA_CTRL: usize = 0x04;
const DMA_SCB_BURST_SIZE: usize = 0x0c;
const DMA_EN: u32 = 1;
const DMA_RING_BUF_EN_SHIFT: u32 = 1;

const DMA_PROD_INDEX: usize = 0x08;
const DMA_CONS_INDEX: usize = 0x0c;
const DMA_RING_BUF_SIZE_REG: usize = 0x10;
const DMA_START_ADDR: usize = 0x14;
const DMA_END_ADDR: usize = 0x1c;
const DMA_MBUF_DONE_THRESH: usize = 0x24;
const DMA_FLOW_PERIOD: usize = 0x28;
const DMA_READ_PTR: usize = 0x00;
const DMA_WRITE_PTR: usize = 0x2c;

const DMA_OWN: u32 = 0x8000;
const DMA_EOP: u32 = 0x4000;
const DMA_SOP: u32 = 0x2000;
const DMA_TX_APPEND_CRC: u32 = 0x0040;
const DMA_BUFLENGTH_SHIFT: u32 = 16;
const DMA_BUFLENGTH_MASK: u32 = 0x0fff;
const DMA_MAX_BURST: u32 = 0x8;

const CMD_TX_EN: u32 = 1 << 0;
const CMD_RX_EN: u32 = 1 << 1;
const CMD_SPEED_SHIFT: u32 = 2;
const UMAC_SPEED_1000: u32 = 2;
const CMD_SW_RESET: u32 = 1 << 13;
const CMD_LCL_LOOP_EN: u32 = 1 << 15;
const RGMII_LINK: u32 = 1 << 4;
const OOB_DISABLE: u32 = 1 << 5;
const RGMII_MODE_EN: u32 = 1 << 6;
const ID_MODE_DIS: u32 = 1 << 16;
const RBUF_ALIGN_2B: u32 = 1 << 1;

const DESC_COUNT: usize = 32;
const BUF_LEN: usize = 2048;
const RX_ALIGN: usize = 2;

const DESC_LENGTH_STATUS: usize = 0x00;
const DESC_ADDRESS_LO: usize = 0x04;
const DESC_ADDRESS_HI: usize = 0x08;

static RX_COUNT: AtomicU32 = AtomicU32::new(0);
static TX_COUNT: AtomicU32 = AtomicU32::new(0);

static mut RX_BUFS: [[u8; BUF_LEN]; DESC_COUNT] = [[0; BUF_LEN]; DESC_COUNT];
static mut TX_BUFS: [[u8; BUF_LEN]; DESC_COUNT] = [[0; BUF_LEN]; DESC_COUNT];

static RX_INDEX: AtomicU32 = AtomicU32::new(0);
static RX_CONS: AtomicU32 = AtomicU32::new(0);

pub struct GenetDevice {
    mac: [u8; 6],
    tx_index: u16,
}

pub fn packet_counts() -> (u32, u32) {
    (
        RX_COUNT.load(Ordering::Relaxed),
        TX_COUNT.load(Ordering::Relaxed),
    )
}

impl GenetDevice {
    pub fn new() -> Option<Self> {
        let rev = read(SYS_REV_CTRL);
        if rev == 0 || rev == 0xffff_ffff {
            log::warn!("GENET revision register is not present");
            return None;
        }
        log::info!("GENET revision {:#x}", rev);

        umac_reset();
        let mac = board_mac();
        write_mac(&mac);
        disable_dma();
        init_rx_descriptors();
        init_rx_ring();
        init_tx_ring();
        enable_dma();
        bring_up_link();
        log::info!(
            "GENET MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );

        let rx_cons = read(RDMA_RING + DMA_PROD_INDEX) & 0xffff;
        write(RDMA_RING + DMA_CONS_INDEX, rx_cons);
        RX_CONS.store(rx_cons, Ordering::Relaxed);
        RX_INDEX.store(rx_cons % DESC_COUNT as u32, Ordering::Relaxed);
        let tx_index = (read(TDMA_RING + DMA_CONS_INDEX) & 0xff) as u16;
        write(TDMA_RING + DMA_PROD_INDEX, tx_index as u32);

        Some(Self {
            mac,
            tx_index: tx_index % DESC_COUNT as u16,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn transmit_frame(&mut self, frame: &[u8]) {
        let index = self.tx_index as usize % DESC_COUNT;
        unsafe {
            let buf = &mut TX_BUFS[index];
            let n = frame.len().min(BUF_LEN);
            buf[..n].copy_from_slice(&frame[..n]);
            let addr = crate::addr::virt_to_phys(buf.as_ptr() as u64);
            let desc = GENET_TX_OFF + index * DMA_DESC_SIZE;
            write(desc + DESC_ADDRESS_LO, addr as u32);
            write(desc + DESC_ADDRESS_HI, (addr >> 32) as u32);
            let status = ((n as u32) << DMA_BUFLENGTH_SHIFT)
                | DMA_SOP
                | DMA_EOP
                | DMA_TX_APPEND_CRC
                | (0x3f << 7);
            barrier();
            write(desc + DESC_LENGTH_STATUS, status);
        }
        self.tx_index = self.tx_index.wrapping_add(1) % DESC_COUNT as u16;
        let prod = (read(TDMA_RING + DMA_PROD_INDEX).wrapping_add(1)) & 0xffff;
        barrier();
        write(TDMA_RING + DMA_PROD_INDEX, prod);
        TX_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

fn umac_reset() {
    let mut flush = read(SYS_RBUF_FLUSH_CTRL);
    flush |= 1 << 1;
    write(SYS_RBUF_FLUSH_CTRL, flush);
    spin(1000);
    write(SYS_RBUF_FLUSH_CTRL, 0);
    write(UMAC_CMD, 0);
    write(UMAC_CMD, CMD_SW_RESET | CMD_LCL_LOOP_EN);
    spin(100);
    write(UMAC_CMD, 0);
    write(UMAC_MIB_CTRL, (1 << 0) | (1 << 1) | (1 << 2));
    write(UMAC_MIB_CTRL, 0);
    write(UMAC_MAX_FRAME_LEN, 1536);
    let rbuf = read(RBUF_CTRL) | RBUF_ALIGN_2B;
    write(RBUF_CTRL, rbuf);
    write(RBUF_TBUF_SIZE_CTRL, 1);
}

fn disable_dma() {
    let tx = read(TDMA_REG_BASE + DMA_CTRL) & !DMA_EN;
    write(TDMA_REG_BASE + DMA_CTRL, tx);
    let rx = read(RDMA_REG_BASE + DMA_CTRL) & !DMA_EN;
    write(RDMA_REG_BASE + DMA_CTRL, rx);
    write(UMAC_TX_FLUSH, 1);
    spin(1000);
    write(UMAC_TX_FLUSH, 0);
}

fn enable_dma() {
    let mask = (1 << (DEFAULT_Q as u32 + DMA_RING_BUF_EN_SHIFT)) | DMA_EN;
    write(TDMA_REG_BASE + DMA_CTRL, mask);
    let rx = read(RDMA_REG_BASE + DMA_CTRL) | mask;
    write(RDMA_REG_BASE + DMA_CTRL, rx);
}

fn init_rx_descriptors() {
    let status = (BUF_LEN as u32) << DMA_BUFLENGTH_SHIFT | DMA_OWN;
    for index in 0..DESC_COUNT {
        program_rx_desc(index);
        let desc = GENET_RX_OFF + index * DMA_DESC_SIZE;
        write(desc + DESC_LENGTH_STATUS, status);
    }
}

fn program_rx_desc(index: usize) {
    unsafe {
        let addr = crate::addr::virt_to_phys(RX_BUFS[index].as_ptr() as u64);
        let desc = GENET_RX_OFF + index * DMA_DESC_SIZE;
        write(desc + DESC_ADDRESS_LO, addr as u32);
        write(desc + DESC_ADDRESS_HI, (addr >> 32) as u32);
        write(
            desc + DESC_LENGTH_STATUS,
            (BUF_LEN as u32) << DMA_BUFLENGTH_SHIFT | DMA_OWN,
        );
    }
}

fn init_rx_ring() {
    write(RDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST);
    write(RDMA_RING + DMA_START_ADDR, 0);
    write(RDMA_RING + DMA_READ_PTR, 0);
    write(RDMA_RING + DMA_WRITE_PTR, 0);
    write(RDMA_RING + DMA_END_ADDR, (DESC_COUNT * DMA_DESC_SIZE / 4 - 1) as u32);
    write(
        RDMA_RING + DMA_RING_BUF_SIZE_REG,
        ((DESC_COUNT as u32) << 16) | BUF_LEN as u32,
    );
    write(RDMA_RING + DMA_FLOW_PERIOD, (5 << 16) | (DESC_COUNT as u32 >> 4));
    write(RDMA_REG_BASE + DMA_RING_CFG, 1 << DEFAULT_Q);
}

fn init_tx_ring() {
    write(TDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST);
    write(TDMA_RING + DMA_START_ADDR, 0);
    write(TDMA_RING + DMA_READ_PTR, 0);
    write(TDMA_RING + DMA_WRITE_PTR, 0);
    write(TDMA_RING + DMA_END_ADDR, (DESC_COUNT * DMA_DESC_SIZE / 4 - 1) as u32);
    write(TDMA_RING + DMA_MBUF_DONE_THRESH, 1);
    write(TDMA_RING + DMA_FLOW_PERIOD, 0);
    write(
        TDMA_RING + DMA_RING_BUF_SIZE_REG,
        ((DESC_COUNT as u32) << 16) | BUF_LEN as u32,
    );
    write(TDMA_REG_BASE + DMA_RING_CFG, 1 << DEFAULT_Q);
}

fn bring_up_link() {
    let _phy = mdio_read(0x01, 0x01).or_else(|| mdio_read(0x00, 0x01));
    let oob = (read(EXT_RGMII_OOB_CTRL) & !OOB_DISABLE) | RGMII_LINK | RGMII_MODE_EN | ID_MODE_DIS;
    write(EXT_RGMII_OOB_CTRL, oob);
    write(UMAC_CMD, (UMAC_SPEED_1000 << CMD_SPEED_SHIFT) | CMD_TX_EN | CMD_RX_EN);
}

fn mdio_read(phy: u32, reg: u32) -> Option<u16> {
    let cmd = (1 << 29) | (2 << 26) | ((phy & 0x1f) << 21) | ((reg & 0x1f) << 16);
    write(MDIO_CMD, cmd);
    for _ in 0..10_000 {
        let value = read(MDIO_CMD);
        if value & (1 << 29) == 0 {
            if value & (1 << 28) != 0 {
                return None;
            }
            return Some(value as u16);
        }
        spin(50);
    }
    None
}

fn write_mac(mac: &[u8; 6]) {
    let hi = (mac[0] as u32) << 24 | (mac[1] as u32) << 16 | (mac[2] as u32) << 8 | mac[3] as u32;
    let lo = (mac[4] as u32) << 8 | mac[5] as u32;
    write(UMAC_MAC0, hi);
    write(UMAC_MAC1, lo);
}

fn board_mac() -> [u8; 6] {
    #[repr(C, align(16))]
    struct Msg {
        size: u32,
        code: u32,
        tag: u32,
        val_len: u32,
        req: u32,
        mac: [u8; 6],
        pad: [u8; 2],
        end: u32,
    }
    let mut msg = Msg {
        size: 32,
        code: 0,
        tag: 0x0001_0003,
        val_len: 6,
        req: 0,
        mac: [0; 6],
        pad: [0; 2],
        end: 0,
    };
    let addr = crate::addr::virt_to_phys(&mut msg as *mut Msg as u64) as u32;
    if mailbox_call(addr) {
        if msg.mac.iter().any(|b| *b != 0) {
            return msg.mac;
        }
    }
    [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]
}

fn mailbox_call(addr: u32) -> bool {
    const MAILBOX: usize = crate::addr::phys_to_virt(0xFE00_B880) as usize;
    const STATUS: usize = 0x18;
    const WRITE: usize = 0x20;
    const READ: usize = 0x00;
    const FULL: u32 = 0x8000_0000;
    const EMPTY: u32 = 0x4000_0000;
    let msg = (addr & !0xf) | 8;
    let mut spins = 0;
    while read_at(MAILBOX + STATUS) & FULL != 0 {
        spins += 1;
        if spins > 100_000 {
            return false;
        }
    }
    write_at(MAILBOX + WRITE, msg | 0xC000_0000);
    spins = 0;
    loop {
        while read_at(MAILBOX + STATUS) & EMPTY != 0 {
            spins += 1;
            if spins > 200_000 {
                return false;
            }
        }
        let response = read_at(MAILBOX + READ);
        if response & 0xf == 8 {
            return true;
        }
        spins += 1;
        if spins > 200_000 {
            return false;
        }
    }
}

fn read(offset: usize) -> u32 {
    read_at(BASE + offset)
}

fn write(offset: usize, value: u32) {
    write_at(BASE + offset, value);
}

fn read_at(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn write_at(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

fn barrier() {
    unsafe {
        core::arch::asm!("dsb sy", options(nostack, preserves_flags));
    }
}

fn spin(count: u32) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}

impl Device for GenetDevice {
    type RxToken<'a> = GenetRxToken where Self: 'a;
    type TxToken<'a> = GenetTxToken<'a> where Self: 'a;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let (index, len) = poll_rx()?;
        Some((
            GenetRxToken { index, len },
            GenetTxToken { device: self },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(GenetTxToken { device: self })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(8);
        caps.medium = Medium::Ethernet;
        caps
    }
}

pub struct GenetRxToken {
    index: u16,
    len: u16,
}

impl RxToken for GenetRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let result = unsafe {
            let frame = RX_BUFS[self.index as usize % DESC_COUNT]
                .as_mut_ptr()
                .add(RX_ALIGN);
            let slice = core::slice::from_raw_parts_mut(frame, self.len as usize);
            f(slice)
        };
        // The device that handed out the token is the only GENET. Reclaim
        // through the ring registers directly; the next receive() observes it.
        reclaim_rx_static(self.index);
        result
    }
}

fn poll_rx() -> Option<(u16, u16)> {
    let cons = RX_CONS.load(Ordering::Relaxed);
    let prod = read(RDMA_RING + DMA_PROD_INDEX) & 0xffff;
    if prod == cons {
        return None;
    }
    let index = RX_INDEX.load(Ordering::Relaxed) as usize % DESC_COUNT;
    let desc = GENET_RX_OFF + index * DMA_DESC_SIZE;
    let raw = read(desc + DESC_LENGTH_STATUS);
    let length = ((raw >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK) as usize;
    if length <= RX_ALIGN || length > BUF_LEN {
        reclaim_rx_static(index as u16);
        return None;
    }
    Some((index as u16, (length - RX_ALIGN) as u16))
}

fn reclaim_rx_static(index: u16) {
    let slot = index as usize % DESC_COUNT;
    program_rx_desc(slot);
    RX_INDEX.store((slot as u32 + 1) % DESC_COUNT as u32, Ordering::Relaxed);
    let cons = RX_CONS.load(Ordering::Relaxed).wrapping_add(1) & 0xffff;
    RX_CONS.store(cons, Ordering::Relaxed);
    write(RDMA_RING + DMA_CONS_INDEX, cons);
    RX_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub struct GenetTxToken<'a> {
    device: &'a mut GenetDevice,
}

impl<'a> TxToken for GenetTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = [0u8; 1514];
        let n = len.min(buf.len());
        let result = f(&mut buf[..n]);
        self.device.transmit_frame(&buf[..n]);
        result
    }
}
