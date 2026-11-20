//! BCM2711 GENET Ethernet (Pi 4, MMIO `0xFD580000`) and the BCM54213PE PHY.
//!
//! The March 2025 firmware leaves this block clock-gated when a device tree
//! is loaded, because Linux turns the clock on itself. A read of the
//! revision register then aborts or returns nothing, which QEMU raspi4b
//! also does because it has no GENET. The probe enables the GENET clocks
//! and retries before it decides the block is absent.
//!
//! Silicon major 6 is GENET v5. Major 5 is v4. A readable value other than
//! 0 or `0xffffffff` is accepted either way. The raw word is logged.
//!
//! The PHY is external, address 1, RGMII with the receive delay in the PHY
//! (`rgmii-rxid`). Packet data is polled. The GIC lines are armed so the
//! status counter can move. RX masks the line until the poll drains and
//! rearms it. TX done is cleared in the handler and does not wake the
//! owner, so a transmit cannot cancel the idle sleep.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

const BASE: usize = crate::addr::phys_to_virt(0xFD58_0000) as usize;

const SYS_REV_CTRL: usize = 0x00;
const SYS_PORT_CTRL: usize = 0x04;
const PORT_MODE_EXT_GPHY: u32 = 3;
const SYS_RBUF_FLUSH_CTRL: usize = 0x08;
const SYS_TBUF_FLUSH_CTRL: usize = 0x0c;
const EXT_RGMII_OOB_CTRL: usize = 0x8c;
const INTRL2_0: usize = 0x200;
const INTRL2_1: usize = 0x240;
const INTRL2_STAT: usize = 0x00;
const INTRL2_CLEAR: usize = 0x08;
const INTRL2_MASK_STATUS: usize = 0x0c;
const INTRL2_MASK_SET: usize = 0x10;
const INTRL2_MASK_CLEAR: usize = 0x14;
const IRQ_RXDMA_DONE: u32 = 1 << 13;
const IRQ_TXDMA_DONE: u32 = 1 << 16;
const RBUF_CTRL: usize = 0x300;
const RBUF_TBUF_SIZE_CTRL: usize = 0x3b4;
const UMAC_CMD: usize = 0x808;
const UMAC_MAC0: usize = 0x80c;
const UMAC_MAC1: usize = 0x810;
const UMAC_MAX_FRAME_LEN: usize = 0x814;
const UMAC_MDF_CTRL: usize = 0xe50;
const UMAC_MDF_ADDR: usize = 0xe54;
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
const DMA_STATUS: usize = 0x08;
const DMA_SCB_BURST_SIZE: usize = 0x0c;
const DMA_EN: u32 = 1;
const DMA_DISABLED: u32 = 1;
const DMA_RING_BUF_EN_SHIFT: u32 = 1;

// GENET v4/v5 ring map. The 40-bit pointer words sit between the v3
// indexes, and the RX names are swapped with the TX names.
// TX producer (software) is 0x0C. TX consumer (hardware) is 0x08.
// RX producer (hardware) is 0x08. RX consumer (software) is 0x0C.
const TDMA_CONS_INDEX: usize = 0x08;
const TDMA_PROD_INDEX: usize = 0x0c;
const RDMA_PROD_INDEX: usize = 0x08;
const RDMA_CONS_INDEX: usize = 0x0c;
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
const DMA_TX_QTAG_SHIFT: u32 = 7;
const DMA_RX_CRC_ERROR: u32 = 0x0002;
const DMA_RX_OV: u32 = 0x0001;
const DMA_RX_NO: u32 = 0x0008;
const DMA_RX_LG: u32 = 0x0010;
const DMA_RX_RXER: u32 = 0x0004;
const DMA_RX_ERRORS: u32 =
    DMA_RX_CRC_ERROR | DMA_RX_OV | DMA_RX_NO | DMA_RX_LG | DMA_RX_RXER;
const DMA_BUFLENGTH_SHIFT: u32 = 16;
const DMA_BUFLENGTH_MASK: u32 = 0x0fff;
/// U-Boot's Pi 4 driver uses 8 for both rings.
const DMA_MAX_BURST_TX: u32 = 0x08;
/// U-Boot assigns all 256 TDMA descriptors to ring 16. A 32-deep window
/// left the consumer at 0 on this silicon.
const TX_RING_DESCS: u32 = 256;

const CMD_TX_EN: u32 = 1 << 0;
const CMD_RX_EN: u32 = 1 << 1;
const CMD_SPEED_SHIFT: u32 = 2;
const CMD_SPEED_MASK: u32 = 3;
const CMD_PROMISC: u32 = 1 << 4;
const CMD_HD_EN: u32 = 1 << 10;
const CMD_SW_RESET: u32 = 1 << 13;
const CMD_LCL_LOOP_EN: u32 = 1 << 15;
const CMD_RX_PAUSE_IGNORE: u32 = 1 << 8;
const CMD_TX_PAUSE_IGNORE: u32 = 1 << 28;
const RGMII_LINK: u32 = 1 << 4;
const OOB_DISABLE: u32 = 1 << 5;
const RGMII_MODE_EN: u32 = 1 << 6;
const ID_MODE_DIS: u32 = 1 << 16;
const RBUF_64B_EN: u32 = 1 << 0;
const RBUF_ALIGN_2B: u32 = 1 << 1;
const HFB_CTRL: usize = 0xfc00;

const DESC_COUNT: usize = 32;
/// U-Boot gives ring 16 every RX descriptor. The register block is already
/// placed after 256 of them. A 32-deep END left the engine walking
/// descriptors this driver never armed, so the MAC counted frames the
/// stack never saw.
const RX_RING_DESCS: usize = 256;
const WORDS_PER_BD: u32 = 3;
const BUF_LEN: usize = 2048;
const RX_ALIGN: usize = 2;

const DESC_LENGTH_STATUS: usize = 0x00;
const DESC_ADDRESS_LO: usize = 0x04;
const DESC_ADDRESS_HI: usize = 0x08;

const CM_BASE: usize = crate::addr::phys_to_virt(0xFE10_1000) as usize;
const CM_PASSWORD: u32 = 0x5a00_0000;
const CM_ENABLE: u32 = 1 << 4;
const CM_BUSY: u32 = 1 << 7;
const CM_GENET250_CTL: usize = 0x1e8;
const CM_GENET250_DIV: usize = 0x1ec;
const CM_GENET125_CTL: usize = 0x210;
const CM_GENET125_DIV: usize = 0x214;
const CLKGEN: usize = crate::addr::phys_to_virt(0xFD5E_0048) as usize;
const CLKGEN_GATES: u32 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) | (1 << 6) | (1 << 7)
    | (1 << 8);

const GENET_IRQ0: u32 = 32 + 157;
const GENET_IRQ1: u32 = 32 + 158;

const MII_BMCR: u32 = 0;
const MII_BMSR: u32 = 1;
const MII_ADVERTISE: u32 = 4;
const MII_LPA: u32 = 5;
const MII_CTRL1000: u32 = 9;
const MII_STAT1000: u32 = 10;
const BMCR_RESET: u16 = 1 << 15;
const BMCR_ANENABLE: u16 = 1 << 12;
const BMCR_ANRESTART: u16 = 1 << 9;
const BMSR_LSTATUS: u16 = 1 << 2;
const ADVERTISE_ALL: u16 = 0x0de1;
const ADVERTISE_100FULL: u16 = 0x0101;
const ADVERTISE_1000FULL: u16 = 0x0200;
const LPA_1000FULL: u16 = 0x0800;
const LPA_1000HALF: u16 = 0x0400;
const AUXCTL: u32 = 0x18;
const AUX_MISC: u16 = 7;
const AUX_RGMII_SKEW: u16 = 0x0100;

static RX_COUNT: AtomicU32 = AtomicU32::new(0);
static RX_ERR: AtomicU32 = AtomicU32::new(0);
static TX_COUNT: AtomicU32 = AtomicU32::new(0);
static RX_BYTES: AtomicU64 = AtomicU64::new(0);
static TX_BYTES: AtomicU64 = AtomicU64::new(0);

#[link_section = ".dma"]
static mut RX_BUFS: [[u8; BUF_LEN]; RX_RING_DESCS] = [[0; BUF_LEN]; RX_RING_DESCS];
#[link_section = ".dma"]
static mut TX_BUFS: [[u8; BUF_LEN]; DESC_COUNT] = [[0; BUF_LEN]; DESC_COUNT];

static RX_CONS: AtomicU32 = AtomicU32::new(0);
static TX_PROD: AtomicU32 = AtomicU32::new(0);
/// Set when the consumer does not catch the producer. Further transmits stop.
static TX_STUCK: AtomicBool = AtomicBool::new(false);
static DESC_MISMATCH: AtomicBool = AtomicBool::new(false);
static ARP_TX_LOG_MS: AtomicU64 = AtomicU64::new(0);
static ARP_RX_LOG_MS: AtomicU64 = AtomicU64::new(0);
static DHCP_NOTED: AtomicBool = AtomicBool::new(false);
/// CPU copy of one received frame. The owner is the only caller of
/// `consume`, so this is not shared. The DMA buffer is returned before
/// the stack reads it.
static mut RX_COPY: [u8; BUF_LEN] = [0; BUF_LEN];
static SRC_FIXED: AtomicBool = AtomicBool::new(false);

static PRESENT: AtomicBool = AtomicBool::new(false);
static RX_DROP_NOTED: AtomicBool = AtomicBool::new(false);
static SPEED_CAP: AtomicU32 = AtomicU32::new(0);

pub fn set_speed_cap(cap: u32) {
    SPEED_CAP.store(cap, Ordering::Relaxed);
}
static REV: AtomicU32 = AtomicU32::new(0);
static VERSION: AtomicU32 = AtomicU32::new(0);
static LINK_UP: AtomicBool = AtomicBool::new(false);
static SPEED: AtomicU32 = AtomicU32::new(0);
static FULL_DUPLEX: AtomicBool = AtomicBool::new(true);
static PHY_ADDR: AtomicU32 = AtomicU32::new(1);
static LAST_LINK_MS: AtomicU64 = AtomicU64::new(0);
static MAC: [AtomicU8; 6] = [
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
    AtomicU8::new(0),
];

// 64 bytes so the following RX buffer cannot share this cache line.
// A clean of that buffer would otherwise write RX bytes over a mailbox
// reply still sitting in the same line. `guard` is not part of the
// 32-byte property message the GPU reads.
#[repr(C, align(64))]
struct MacMsg {
    size: u32,
    code: u32,
    tag: u32,
    val_len: u32,
    req: u32,
    mac: [u8; 6],
    pad: [u8; 2],
    end: u32,
    #[allow(dead_code)]
    guard: [u8; 32],
}

const _: () = assert!(core::mem::align_of::<MacMsg>() == 64);
const _: () = assert!(core::mem::size_of::<MacMsg>() == 64);

#[link_section = ".dma"]
static mut MAC_MSG: MacMsg = MacMsg {
    size: 0,
    code: 0,
    tag: 0,
    val_len: 0,
    req: 0,
    mac: [0; 6],
    pad: [0; 2],
    end: 0,
    guard: [0; 32],
};

pub fn link_is_up() -> bool {
    LINK_UP.load(Ordering::Relaxed)
}

pub fn write_status(out: &mut [u8]) -> usize {
    let mut line = StatusLine { buf: out, n: 0 };
    if !PRESENT.load(Ordering::Relaxed) {
        line.push("absent");
        return line.n;
    }
    line.push("v");
    line.dec(VERSION.load(Ordering::Relaxed));
    line.push(" ");
    line.hex(REV.load(Ordering::Relaxed));
    if LINK_UP.load(Ordering::Relaxed) {
        line.push(" UP ");
        line.dec(SPEED.load(Ordering::Relaxed));
        if FULL_DUPLEX.load(Ordering::Relaxed) {
            line.push("F");
        } else {
            line.push("H");
        }
    } else {
        line.push(" DOWN");
    }
    line.push(" ");
    for (index, slot) in MAC.iter().enumerate() {
        if index > 0 {
            line.push(":");
        }
        line.byte(slot.load(Ordering::Relaxed));
    }
    line.n
}

/// Second pinned row: frames this driver queued, frames it accepted,
/// and descriptors it rejected. No MAC counter registers.
pub fn write_diag(out: &mut [u8]) -> usize {
    let mut line = StatusLine { buf: out, n: 0 };
    if !PRESENT.load(Ordering::Relaxed) {
        line.push("tx=0 rx=0");
        return line.n;
    }
    line.push("tx=");
    line.dec(TX_COUNT.load(Ordering::Relaxed));
    line.push(" rx=");
    line.dec(RX_COUNT.load(Ordering::Relaxed));
    line.push(" rxE=");
    line.dec(RX_ERR.load(Ordering::Relaxed));
    line.n
}

struct StatusLine<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl StatusLine<'_> {
    fn push(&mut self, text: &str) {
        for byte in text.bytes() {
            if self.n >= self.buf.len() {
                return;
            }
            self.buf[self.n] = byte;
            self.n += 1;
        }
    }

    fn dec(&mut self, mut value: u32) {
        let mut tmp = [0u8; 10];
        let mut i = tmp.len();
        if value == 0 {
            self.push("0");
            return;
        }
        while value > 0 {
            i -= 1;
            tmp[i] = b'0' + (value % 10) as u8;
            value /= 10;
        }
        if let Ok(text) = core::str::from_utf8(&tmp[i..]) {
            self.push(text);
        }
    }

    fn hex(&mut self, value: u32) {
        self.push("0x");
        for shift in (0..8).rev() {
            let nibble = ((value >> (shift * 4)) & 0xf) as u8;
            let digit = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + (nibble - 10)
            };
            if self.n >= self.buf.len() {
                return;
            }
            self.buf[self.n] = digit;
            self.n += 1;
        }
    }

    fn byte(&mut self, value: u8) {
        let hi = value >> 4;
        let lo = value & 0xf;
        let digit = |nibble: u8| {
            if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + (nibble - 10)
            }
        };
        if self.n + 2 > self.buf.len() {
            return;
        }
        self.buf[self.n] = digit(hi);
        self.buf[self.n + 1] = digit(lo);
        self.n += 2;
    }
}

pub struct GenetDevice {
    mac: [u8; 6],
}

pub fn packet_bytes() -> (u64, u64) {
    (
        RX_BYTES.load(Ordering::Relaxed),
        TX_BYTES.load(Ordering::Relaxed),
    )
}

pub fn packet_counts() -> (u32, u32) {
    (
        RX_COUNT.load(Ordering::Relaxed),
        TX_COUNT.load(Ordering::Relaxed),
    )
}

impl GenetDevice {
    pub fn new() -> Option<Self> {
        enable_clocks();
        let Some(rev) = read_revision() else {
            say("[FAIL] GENET revision register is not present");
            return None;
        };
        let version = genet_version(rev);
        REV.store(rev, Ordering::Relaxed);
        VERSION.store(version, Ordering::Relaxed);
        say_hex("[OK] GENET revision", rev);
        if version == 5 {
            say("[OK] GENET version 5 (silicon major 6)");
        } else {
            say_hex("[WARN] GENET version is not 5, continuing", version);
        }

        umac_reset();
        write(SYS_PORT_CTRL, PORT_MODE_EXT_GPHY);
        say("[OK] GENET port mode external RGMII");

        let mac = board_mac();
        store_mac(&mac);
        write_mac(&mac);
        set_rx_filter(&mac);
        say_mac("[OK] GENET MAC", &mac);

        disable_dma();
        init_rx_descriptors();
        init_rx_ring();
        init_tx_ring();
        enable_dma();
        program_irqs();

        if !start_phy() {
            say("[WARN] GENET PHY did not answer on MDIO");
        }

        PRESENT.store(true, Ordering::Relaxed);
        service_link();
        crate::dhcp_rx::arm(&mac);
        say("[OK] GENET v5 TX and RX rings are 256 descriptors");

        Some(Self { mac })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn transmit_frame(&mut self, frame: &[u8]) {
        if TX_STUCK.load(Ordering::Relaxed) {
            return;
        }
        // U-Boot `bcmgenet_gmac_eth_send`: TDMA stays enabled. Write the
        // 12-byte descriptor (address low, address high, length/status)
        // and then the full producer word plus one. Length/status is
        // `(len << 16) | SOP | EOP | APPEND_CRC | (0x3F << 7)`.
        let raw_prod = read(TDMA_RING + TDMA_PROD_INDEX);
        let prod = raw_prod & 0xffff;
        let cons = read(TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
        if prod.wrapping_sub(cons) & 0xffff >= DESC_COUNT as u32 {
            return;
        }
        let desc_index = (prod as usize) & 0xff;
        let slot = desc_index % DESC_COUNT;
        let mut n = frame.len().min(BUF_LEN);
        if n < 60 {
            n = 60;
        }
        let (addr, status) = unsafe {
            let buf = &mut TX_BUFS[slot];
            let copied = frame.len().min(BUF_LEN);
            buf[..copied].copy_from_slice(&frame[..copied]);
            if copied < 60 {
                buf[copied..60].fill(0);
            }
            // smoltcp copies the interface MAC into the source field. A
            // frame built before that address was installed, or from a
            // stale buffer, must not leave with a different source.
            stamp_tx_source(&mut buf[..n]);
            clean_poc(buf.as_ptr(), n);
            let addr = crate::addr::virt_to_phys(buf.as_ptr() as u64);
            let status = ((n as u32) << DMA_BUFLENGTH_SHIFT)
                | DMA_SOP
                | DMA_EOP
                | DMA_TX_APPEND_CRC
                | (0x3f << DMA_TX_QTAG_SHIFT);
            (addr, status)
        };
        let desc = GENET_TX_OFF + desc_index * DMA_DESC_SIZE;
        // The image that forwarded (c057543) read the descriptor status
        // and the whole ring-16 block back after the three stores and
        // before the producer store. Each of those loads was wrapped in
        // a data barrier. A later image kept only status, consumer, and
        // DMA_CTRL; off-link traffic still died while on-link TX did not.
        // The producer store stays last, and only after the same registers
        // have been read back.
        publish_tx_descriptor(desc, addr, status);
        crate::net::note_tx_frame(frame);
        note_tx_arp(frame);
        let posted = raw_prod.wrapping_add(1);
        write_at(BASE + TDMA_RING + TDMA_PROD_INDEX, posted);
        let next = posted & 0xffff;
        TX_PROD.store(next, Ordering::Relaxed);
        TX_COUNT.fetch_add(1, Ordering::Relaxed);
        TX_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        let start = crate::timer::millis();
        loop {
            let got = read_at(BASE + TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
            let advanced = got.wrapping_sub(prod) & 0xffff;
            let posted = next.wrapping_sub(prod) & 0xffff;
            if advanced >= posted {
                return;
            }
            if crate::timer::millis().saturating_sub(start) > 20 {
                say("[WARN] GENET TX consumer did not catch the producer");
                TX_STUCK.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn publish_tx_descriptor(desc: usize, addr: u64, status: u32) {
    let mut spins = 0;
    loop {
        write(desc + DESC_ADDRESS_LO, addr as u32);
        write(desc + DESC_ADDRESS_HI, (addr >> 32) as u32);
        write(desc + DESC_LENGTH_STATUS, status);
        barrier();
        let got = read(desc + DESC_LENGTH_STATUS);
        let lo = read(desc + DESC_ADDRESS_LO);
        let hi = read(desc + DESC_ADDRESS_HI);
        if got == status && lo == addr as u32 && hi == (addr >> 32) as u32 {
            break;
        }
        spins += 1;
        if spins >= 4 {
            if !DESC_MISMATCH.swap(true, Ordering::Relaxed) {
                log::info!("genet tx descriptor readback mismatch");
            }
            break;
        }
    }
    barrier();
    let _ = read(TDMA_RING + DMA_READ_PTR);
    let _ = read(TDMA_RING + DMA_WRITE_PTR);
    let _ = read(TDMA_RING + TDMA_PROD_INDEX);
    let _ = read(TDMA_RING + TDMA_CONS_INDEX);
    let _ = read(TDMA_RING + DMA_START_ADDR);
    let _ = read(TDMA_RING + DMA_END_ADDR);
    let _ = read(TDMA_RING + DMA_RING_BUF_SIZE_REG);
    let _ = read(TDMA_REG_BASE + DMA_RING_CFG);
    let _ = read(TDMA_REG_BASE + DMA_CTRL);
    let _ = read(TDMA_REG_BASE + DMA_STATUS);
    barrier();
}

/// One line when we ask for a neighbor that is not ourselves. Gratuitous
/// announcements are requests whose target is our own address and are not
/// logged. At most one line every two seconds.
fn note_tx_arp(frame: &[u8]) {
    if frame.len() < 42 || frame[12] != 0x08 || frame[13] != 0x06 {
        return;
    }
    if frame[20] != 0 || frame[21] != 1 {
        return;
    }
    let spa = [frame[28], frame[29], frame[30], frame[31]];
    let tpa = [frame[38], frame[39], frame[40], frame[41]];
    if tpa == spa || !arp_log_due(&ARP_TX_LOG_MS) {
        return;
    }
    log::info!(
        "arp who-has {}.{}.{}.{}",
        tpa[0], tpa[1], tpa[2], tpa[3]
    );
}

fn note_rx_arp(frame: &[u8]) {
    if frame.len() < 42 || frame[12] != 0x08 || frame[13] != 0x06 {
        return;
    }
    if frame[20] != 0 || frame[21] != 2 || !arp_log_due(&ARP_RX_LOG_MS) {
        return;
    }
    let mac = [
        frame[22], frame[23], frame[24], frame[25], frame[26], frame[27],
    ];
    let spa = [frame[28], frame[29], frame[30], frame[31]];
    log::info!(
        "arp reply {}.{}.{}.{} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        spa[0],
        spa[1],
        spa[2],
        spa[3],
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
}

fn arp_log_due(slot: &AtomicU64) -> bool {
    let now = crate::timer::millis();
    let prev = slot.load(Ordering::Relaxed);
    if now.saturating_sub(prev) < 2_000 {
        return false;
    }
    slot.store(now, Ordering::Relaxed);
    true
}

fn tx_ring_has_room() -> bool {
    let prod = TX_PROD.load(Ordering::Relaxed);
    let cons = read(TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
    prod.wrapping_sub(cons) & 0xffff < DESC_COUNT as u32
}

/// The consumer is hardware-owned. U-Boot does not write it ("cannot init
/// TDMA_CONS_INDEX to 0") and copies the whole register into the producer
/// so the engine is idle. A later frame publishes producer+1 only.
fn align_tx_prod() {
    let cons = read(TDMA_RING + TDMA_CONS_INDEX);
    write(TDMA_RING + TDMA_PROD_INDEX, cons);
    TX_PROD.store(cons & 0xffff, Ordering::Relaxed);
}

fn genet_version(rev: u32) -> u32 {
    match (rev >> 24) & 0xf {
        6 => 5,
        5 => 4,
        0 => 1,
        n => n,
    }
}

fn read_revision() -> Option<u32> {
    match crate::exception::read_u32_catching((BASE + SYS_REV_CTRL) as *const u32) {
        Some(rev) if rev != 0 && rev != 0xffff_ffff => Some(rev),
        _ => None,
    }
}

fn enable_clocks() {
    match crate::exception::read_u32_catching((CM_BASE + CM_GENET250_CTL) as *const u32) {
        None => say("[WARN] GENET clock manager is not present"),
        Some(0xffff_ffff) => say("[WARN] GENET clock manager read 0xffffffff"),
        Some(_) => {
            let genet250 = enable_one_clock(CM_GENET250_CTL, CM_GENET250_DIV);
            let genet125 = enable_one_clock(CM_GENET125_CTL, CM_GENET125_DIV);
            if genet250 && genet125 {
                say("[OK] GENET clocks enabled");
            }
        }
    }
    match crate::exception::read_u32_catching(CLKGEN as *const u32) {
        Some(value) if value != 0xffff_ffff => {
            write_at(CLKGEN, value | CLKGEN_GATES);
            say("[OK] GENET clock gates on");
        }
        Some(_) => say("[WARN] GENET clock gate read 0xffffffff"),
        None => say("[WARN] GENET clock gate is not present"),
    }
}

fn enable_one_clock(ctl: usize, div: usize) -> bool {
    let ctl_addr = CM_BASE + ctl;
    let Some(cur) = crate::exception::read_u32_catching(ctl_addr as *const u32) else {
        say("[WARN] GENET clock manager is not present");
        return false;
    };
    if cur == 0xffff_ffff {
        say("[WARN] GENET clock manager read 0xffffffff");
        return false;
    }
    if cur & CM_ENABLE != 0 {
        return true;
    }
    let div_addr = CM_BASE + div;
    let mut divisor = crate::exception::read_u32_catching(div_addr as *const u32).unwrap_or(0);
    if divisor & 0xfff == 0 {
        divisor = 1 << 8;
    }
    write_at(div_addr, CM_PASSWORD | (divisor & 0x00ff_ffff));
    let src = {
        let source = cur & 0xf;
        if source == 0 {
            6
        } else {
            source
        }
    };
    write_at(ctl_addr, CM_PASSWORD | CM_ENABLE | src);
    let start = crate::timer::millis();
    let mut spins = 0u32;
    while crate::timer::millis().saturating_sub(start) < 20 {
        spins = spins.saturating_add(1);
        if spins > 100_000 {
            break;
        }
        match crate::exception::read_u32_catching(ctl_addr as *const u32) {
            Some(value) if value & CM_BUSY == 0 => break,
            _ => core::hint::spin_loop(),
        }
    }
    true
}

fn umac_reset() {
    // U-Boot pulses RBUF flush bit 1, clears that bit, then writes 0,
    // with a delay after each step. The same pulse on the TBUF register
    // drops a transmit buffer the VideoCore firmware may have left armed.
    flush_buf(SYS_RBUF_FLUSH_CTRL);
    flush_buf(SYS_TBUF_FLUSH_CTRL);
    write(UMAC_CMD, 0);
    write(UMAC_CMD, CMD_SW_RESET | CMD_LCL_LOOP_EN);
    spin(100);
    write(UMAC_CMD, 0);
    write(UMAC_MIB_CTRL, (1 << 0) | (1 << 1) | (1 << 2));
    write(UMAC_MIB_CTRL, 0);
    write(UMAC_MAX_FRAME_LEN, 1536);
    let mut rbuf = read(RBUF_CTRL);
    rbuf &= !RBUF_64B_EN;
    rbuf |= RBUF_ALIGN_2B;
    write(RBUF_CTRL, rbuf);
    write(RBUF_TBUF_SIZE_CTRL, 1);
    let hfb = read(HFB_CTRL);
    if hfb != 0 {
        say_hex("[WARN] GENET HFB was enabled", hfb);
    }
    write(HFB_CTRL, 0);
    write(HFB_CTRL + 4, 0);
    write(HFB_CTRL + 8, 0);
    say("[OK] GENET RBUF align 2, HFB off, RBUF and TBUF flushed");
}

fn flush_buf(offset: usize) {
    let mut reg = read(offset);
    reg |= 1 << 1;
    write(offset, reg);
    spin(1000);
    reg &= !(1 << 1);
    write(offset, reg);
    spin(1000);
    write(offset, 0);
    spin(1000);
}

fn disable_dma() {
    // Firmware can leave rings 0-15 enabled. Clear the whole control
    // word and the ring-config mask, then flush the UniMAC TX queue.
    // U-Boot only clears DMA_EN; the following enable write is the
    // value `(1 << (16 + 1)) | DMA_EN`, which keeps ring 16 alone.
    write(TDMA_REG_BASE + DMA_CTRL, 0);
    write(RDMA_REG_BASE + DMA_CTRL, 0);
    write(TDMA_REG_BASE + DMA_RING_CFG, 0);
    write(RDMA_REG_BASE + DMA_RING_CFG, 0);
    write(UMAC_TX_FLUSH, 1);
    spin(1000);
    write(UMAC_TX_FLUSH, 0);
}

fn enable_dma() {
    let mask = (1 << (DEFAULT_Q as u32 + DMA_RING_BUF_EN_SHIFT)) | DMA_EN;
    write(TDMA_REG_BASE + DMA_CTRL, mask);
    write(RDMA_REG_BASE + DMA_CTRL, mask);
    let start = crate::timer::millis();
    let mut spins = 0u32;
    while crate::timer::millis().saturating_sub(start) < 20 {
        spins = spins.saturating_add(1);
        if spins > 100_000 {
            break;
        }
        if read(TDMA_REG_BASE + DMA_STATUS) & DMA_DISABLED == 0 {
            break;
        }
        core::hint::spin_loop();
    }
}

fn init_rx_descriptors() {
    for index in 0..RX_RING_DESCS {
        program_rx_desc(index);
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
    let end = RX_RING_DESCS as u32 * WORDS_PER_BD - 1;
    let size = ((RX_RING_DESCS as u32) << 16) | BUF_LEN as u32;
    write(RDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_TX);
    // The producer is hardware-owned. U-Boot does not write it; the
    // consumer is aligned to whatever the register already holds.
    // U-Boot writes START, the two pointers, and END. It does not
    // program the high pointer words.
    write(RDMA_RING + DMA_START_ADDR, 0);
    write(RDMA_RING + DMA_READ_PTR, 0);
    write(RDMA_RING + DMA_WRITE_PTR, 0);
    write(RDMA_RING + DMA_END_ADDR, end);
    write(RDMA_RING + DMA_RING_BUF_SIZE_REG, size);
    // One completed buffer raises RXDMA_DONE. TX already programs this.
    // A zero threshold does not, so an echo request never wakes the owner
    // and the next look is the 2 s gratuitous ARP.
    write(RDMA_RING + DMA_MBUF_DONE_THRESH, 1);
    write(RDMA_RING + DMA_FLOW_PERIOD, (5 << 16) | (RX_RING_DESCS as u32 >> 4));
    write(RDMA_REG_BASE + DMA_RING_CFG, 1 << DEFAULT_Q);
    let got_end = read(RDMA_RING + DMA_END_ADDR);
    let got_size = read(RDMA_RING + DMA_RING_BUF_SIZE_REG);
    if got_end != end || got_size != size {
        say("[WARN] GENET RDMA ring end or size did not stick");
    }
    let prod = read(RDMA_RING + RDMA_PROD_INDEX) & 0xffff;
    write(RDMA_RING + RDMA_CONS_INDEX, prod);
    RX_CONS.store(prod, Ordering::Relaxed);
    say("[OK] GENET RDMA ring 16 uses 256 descriptors from 0");
}

fn init_tx_ring() {
    // U-Boot `tx_ring_init`: burst 8, START/READ/WRITE 0, END = 256*3-1,
    // producer copied from the consumer register, threshold 1, flow 0,
    // size `(256 << 16) | 2048`, RING_CFG bit 16 only. Descriptor RAM is
    // left alone. Later frames keep TDMA enabled and publish prod+1.
    let end = TX_RING_DESCS * WORDS_PER_BD - 1;
    let size = (TX_RING_DESCS << 16) | BUF_LEN as u32;
    write(TDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST_TX);
    write(TDMA_RING + DMA_START_ADDR, 0);
    write(TDMA_RING + DMA_READ_PTR, 0);
    write(TDMA_RING + DMA_WRITE_PTR, 0);
    write(TDMA_RING + DMA_END_ADDR, end);
    write(TDMA_RING + DMA_MBUF_DONE_THRESH, 1);
    write(TDMA_RING + DMA_FLOW_PERIOD, 0);
    write(TDMA_RING + DMA_RING_BUF_SIZE_REG, size);
    align_tx_prod();
    write(TDMA_REG_BASE + DMA_RING_CFG, 1 << DEFAULT_Q);
    let got_end = read(TDMA_RING + DMA_END_ADDR);
    let got_size = read(TDMA_RING + DMA_RING_BUF_SIZE_REG);
    let got_cfg = read(TDMA_REG_BASE + DMA_RING_CFG);
    if got_end != end || got_size != size || got_cfg != (1 << DEFAULT_Q) as u32 {
        say("[WARN] GENET TDMA ring end, size, or config did not stick");
    }
    say("[OK] GENET TDMA ring 16 uses 256 descriptors from 0");
}

fn program_irqs() {
    for base in [INTRL2_0, INTRL2_1] {
        write(base + INTRL2_MASK_SET, 0xffff_ffff);
        write(base + INTRL2_CLEAR, 0xffff_ffff);
    }
    write(
        INTRL2_0 + INTRL2_MASK_CLEAR,
        IRQ_RXDMA_DONE | IRQ_TXDMA_DONE,
    );
    // Core 0 owns the NIC until core 1 claims it and retarget() moves these.
    crate::gic::arm_spi(GENET_IRQ0, 0x80, 1);
    crate::gic::arm_spi(GENET_IRQ1, 0x80, 1);
    say("[OK] GENET IRQ armed, data path is polled");
}

/// Point both GENET SPIs at the network owner. Bit 0 of the mask is CPU 0.
pub fn retarget(cpu: usize) {
    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    let mask = 1u8 << (cpu & 7);
    crate::gic::route_spi(GENET_IRQ0, mask);
    crate::gic::route_spi(GENET_IRQ1, mask);
}

static IRQ_HELD: AtomicBool = AtomicBool::new(false);

/// The handler masks the GIC line. The poll that follows clears any
/// summary a frame left behind and enables the line again.
pub fn note_irq_held() {
    IRQ_HELD.store(true, Ordering::Relaxed);
}

pub fn rearm_irq() {
    if !PRESENT.load(Ordering::Relaxed) || !IRQ_HELD.load(Ordering::Relaxed) {
        return;
    }
    // A frame that arrives after the handler's clear and before this poll
    // sets the summary again. Skipping the enable in that case left the
    // GIC line masked for good: nothing else writes INTRL2_CLEAR, so every
    // later packet waited for the 2 s ARP deadline.
    for base in [INTRL2_0, INTRL2_1] {
        let stat = read(base + INTRL2_STAT);
        if stat != 0 {
            write(base + INTRL2_CLEAR, stat);
        }
    }
    crate::gic::enable_id(GENET_IRQ0);
    crate::gic::enable_id(GENET_IRQ1);
    IRQ_HELD.store(false, Ordering::Relaxed);
}

/// What the GENET summary was asking for, after it has been cleared.
pub enum NicIrq {
    /// RX, or a summary that did not drop. Mask the line and wake the owner.
    Wake,
    /// Transmit done, and the summary is clear. Leave the line enabled and
    /// do not wake. The send path already waited for the consumer. Waking
    /// here made every transmit another poll, so a socket that stayed at
    /// `PollAt::Now` never reached `wfi`.
    TxOnly,
}

/// Clear the GENET interrupt summary and say whether the owner must run.
pub fn take_irq(which: u32) -> NicIrq {
    let base = if which == 0 { INTRL2_0 } else { INTRL2_1 };
    let stat_addr = (BASE + base + INTRL2_STAT) as *const u32;
    let Some(stat) = crate::exception::read_u32_catching(stat_addr) else {
        return NicIrq::Wake;
    };
    write(base + INTRL2_CLEAR, stat);
    let Some(left) = crate::exception::read_u32_catching(stat_addr) else {
        return NicIrq::Wake;
    };
    let mask = read(base + INTRL2_MASK_STATUS);
    let stuck = left & !mask != 0;
    let rx = stat & IRQ_RXDMA_DONE != 0;
    let tx = stat & IRQ_TXDMA_DONE != 0;
    if rx || !tx || stuck {
        NicIrq::Wake
    } else {
        NicIrq::TxOnly
    }
}

fn start_phy() -> bool {
    let phy = if mdio_read(1, MII_BMSR).is_some() {
        1
    } else if mdio_read(0, MII_BMSR).is_some() {
        say("[WARN] GENET PHY address 1 did not answer, trying 0");
        0
    } else {
        return false;
    };
    PHY_ADDR.store(phy, Ordering::Relaxed);
    if mdio_write(phy, MII_BMCR, BMCR_RESET).is_none() {
        return false;
    }
    // The reset bit stays set longer than a few MDIO transactions.
    // Advertise and autoneg writes are ignored while it is set.
    let start = crate::timer::millis();
    let mut spins = 0u32;
    let mut clear = false;
    while crate::timer::millis().saturating_sub(start) < 50 {
        spins = spins.saturating_add(1);
        if spins > 100_000 {
            break;
        }
        match mdio_read(phy, MII_BMCR) {
            Some(value) if value & BMCR_RESET == 0 => {
                clear = true;
                break;
            }
            _ => core::hint::spin_loop(),
        }
    }
    if !clear {
        say("[WARN] GENET PHY reset did not clear");
    }
    let cap = SPEED_CAP.load(Ordering::Relaxed);
    if cap == 100 {
        let _ = mdio_write(phy, MII_ADVERTISE, ADVERTISE_100FULL);
        let _ = mdio_write(phy, MII_CTRL1000, 0);
        say("[OK] GENET PHY advertises 100full only");
    } else {
        let _ = mdio_write(phy, MII_ADVERTISE, ADVERTISE_ALL);
        let _ = mdio_write(phy, MII_CTRL1000, ADVERTISE_1000FULL);
    }
    enable_rx_skew(phy);
    clear_phy_tx_delay(phy);
    program_rgmii(false);
    say("[OK] GENET rgmii-rxid: MAC TX delay on, PHY RX delay on, PHY TX delay off");
    let _ = mdio_write(phy, MII_BMCR, BMCR_ANENABLE | BMCR_ANRESTART);
    true
}

fn enable_rx_skew(phy: u32) {
    let select = AUX_MISC | (AUX_MISC << 12);
    if mdio_write(phy, AUXCTL, select).is_none() {
        say("[WARN] GENET PHY skew register did not write");
        return;
    }
    let Some(value) = mdio_read(phy, AUXCTL) else {
        say("[WARN] GENET PHY skew register did not read");
        return;
    };
    let _ = mdio_write(phy, AUXCTL, AUX_MISC | value | AUX_RGMII_SKEW);
}

/// Shadow 0x03 bit 9 is the PHY's own TX clock delay. rgmii-rxid leaves
/// that to the MAC, so a strap that set it would delay TX twice at 1000.
fn clear_phy_tx_delay(phy: u32) {
    let Some(raw) = shadow_read(phy, 0x03) else {
        say("[WARN] GENET PHY TX clock shadow did not read");
        return;
    };
    let cleared = raw & !(1 << 9);
    let _ = shadow_write(phy, 0x03, cleared);
}

fn shadow_read(phy: u32, shadow: u16) -> Option<u16> {
    mdio_write(phy, 0x1c, shadow << 10)?;
    mdio_read(phy, 0x1c).map(|raw| raw & 0x3ff)
}

fn shadow_write(phy: u32, shadow: u16, value: u16) -> Option<()> {
    mdio_write(phy, 0x1c, 0x8000 | (shadow << 10) | (value & 0x3ff))
}

pub fn service_link() {
    if !PRESENT.load(Ordering::Relaxed) {
        return;
    }
    let now = crate::timer::millis();
    if now.saturating_sub(LAST_LINK_MS.load(Ordering::Relaxed)) < 200 {
        return;
    }
    LAST_LINK_MS.store(now, Ordering::Relaxed);
    let phy = PHY_ADDR.load(Ordering::Relaxed);
    let Some(first) = mdio_read(phy, MII_BMSR) else {
        return;
    };
    let status = mdio_read(phy, MII_BMSR).unwrap_or(first);
    let up = status & BMSR_LSTATUS != 0;
    let (speed, full) = if up {
        link_speed(phy)
    } else {
        (0, true)
    };
    let changed = up != LINK_UP.load(Ordering::Relaxed)
        || (up && speed != SPEED.load(Ordering::Relaxed));
    if !changed {
        return;
    }
    apply_link(up, speed, full);
    LINK_UP.store(up, Ordering::Relaxed);
    SPEED.store(speed, Ordering::Relaxed);
    FULL_DUPLEX.store(full, Ordering::Relaxed);
    if up {
        say_hex("[OK] GENET link up, Mb/s", speed);
    } else {
        say("[WARN] GENET link down");
    }
}

fn link_speed(phy: u32) -> (u32, bool) {
    let gig = mdio_read(phy, MII_STAT1000).unwrap_or(0);
    if gig & LPA_1000FULL != 0 {
        return (1000, true);
    }
    if gig & LPA_1000HALF != 0 {
        return (1000, false);
    }
    let lpa = mdio_read(phy, MII_LPA).unwrap_or(0);
    if lpa & 0x0100 != 0 {
        return (100, true);
    }
    if lpa & 0x0080 != 0 {
        return (100, false);
    }
    if lpa & 0x0040 != 0 {
        return (10, true);
    }
    if lpa & 0x0020 != 0 {
        return (10, false);
    }
    (100, true)
}

fn apply_link(up: bool, speed: u32, full: bool) {
    let spd = match speed {
        1000 => 2,
        100 => 1,
        _ => 0,
    };
    let mut cmd = read(UMAC_CMD);
    cmd &= !(CMD_SPEED_MASK << CMD_SPEED_SHIFT);
    cmd &= !CMD_HD_EN;
    cmd &= !(CMD_TX_EN | CMD_RX_EN | CMD_PROMISC);
    cmd |= spd << CMD_SPEED_SHIFT;
    if !full {
        cmd |= CMD_HD_EN;
    }
    if up {
        cmd |= CMD_TX_EN | CMD_RX_EN | CMD_RX_PAUSE_IGNORE | CMD_TX_PAUSE_IGNORE;
    }
    write(UMAC_CMD, cmd);
    program_rgmii(up);
}

/// Linux `bcmgenet_mii_config` sets `ID_MODE_DIS` only for
/// `PHY_INTERFACE_MODE_RGMII`. `RGMII_RXID` and `RGMII_TXID` leave it
/// clear, so the MAC inserts the 2 ns TX delay. The Pi 4 device tree is
/// `rgmii-rxid`: PHY RX delay, MAC TX delay. At 100 Mbit the MAC turns
/// that internal delay off by itself.
fn program_rgmii(link: bool) {
    let mut oob = read(EXT_RGMII_OOB_CTRL) & !OOB_DISABLE & !ID_MODE_DIS;
    oob |= RGMII_MODE_EN;
    if link {
        oob |= RGMII_LINK;
    } else {
        oob &= !RGMII_LINK;
    }
    write(EXT_RGMII_OOB_CTRL, oob);
}

fn mdio_read(phy: u32, reg: u32) -> Option<u16> {
    let cmd = (1 << 29) | (2 << 26) | ((phy & 0x1f) << 21) | ((reg & 0x1f) << 16);
    write(MDIO_CMD, cmd);
    let start = crate::timer::millis();
    let mut spins = 0u32;
    while crate::timer::millis().saturating_sub(start) < 10 {
        spins = spins.saturating_add(1);
        if spins > 100_000 {
            return None;
        }
        let value = read(MDIO_CMD);
        if value & (1 << 29) == 0 {
            if value & (1 << 28) != 0 {
                return None;
            }
            return Some(value as u16);
        }
        core::hint::spin_loop();
    }
    None
}

fn mdio_write(phy: u32, reg: u32, val: u16) -> Option<()> {
    let cmd = (1 << 29) | (1 << 26) | ((phy & 0x1f) << 21) | ((reg & 0x1f) << 16) | (val as u32);
    write(MDIO_CMD, cmd);
    let start = crate::timer::millis();
    let mut spins = 0u32;
    while crate::timer::millis().saturating_sub(start) < 10 {
        spins = spins.saturating_add(1);
        if spins > 100_000 {
            return None;
        }
        let value = read(MDIO_CMD);
        if value & (1 << 29) == 0 {
            if value & (1 << 28) != 0 {
                return None;
            }
            return Some(());
        }
        core::hint::spin_loop();
    }
    None
}

fn write_mac(mac: &[u8; 6]) {
    let hi = (mac[0] as u32) << 24 | (mac[1] as u32) << 16 | (mac[2] as u32) << 8 | mac[3] as u32;
    let lo = (mac[4] as u32) << 8 | mac[5] as u32;
    write(UMAC_MAC0, hi);
    write(UMAC_MAC1, lo);
}

fn store_mac(mac: &[u8; 6]) {
    for (slot, byte) in MAC.iter().zip(mac.iter()) {
        slot.store(*byte, Ordering::Relaxed);
    }
}

fn our_mac() -> [u8; 6] {
    let mut mac = [0u8; 6];
    for (byte, slot) in mac.iter_mut().zip(MAC.iter()) {
        *byte = slot.load(Ordering::Relaxed);
    }
    mac
}

/// Force the Ethernet source to the board MAC. Logs once if the buffer
/// had a different address.
fn stamp_tx_source(buf: &mut [u8]) {
    if buf.len() < 12 {
        return;
    }
    let mac = our_mac();
    if mac == [0; 6] {
        return;
    }
    let mut differ = false;
    let mut i = 0;
    while i < 6 {
        if buf[6 + i] != mac[i] {
            differ = true;
            buf[6 + i] = mac[i];
        }
        i += 1;
    }
    if differ && !SRC_FIXED.swap(true, Ordering::Relaxed) {
        say("[WARN] GENET TX source was not the board MAC; rewritten");
    }
}

fn set_rx_filter(mac: &[u8; 6]) {
    write(UMAC_MDF_CTRL, 0);
    mdf_slot(0, &[0xff; 6], 0);
    mdf_slot(1, mac, 1);
    let mut cmd = read(UMAC_CMD);
    cmd &= !CMD_PROMISC;
    write(UMAC_CMD, cmd);
}

fn mdf_slot(index: usize, addr: &[u8; 6], which: u32) {
    let base = UMAC_MDF_ADDR + index * 8;
    write(base, (addr[0] as u32) << 8 | addr[1] as u32);
    write(
        base + 4,
        (addr[2] as u32) << 24 | (addr[3] as u32) << 16 | (addr[4] as u32) << 8 | addr[5] as u32,
    );
    let mut ctrl = read(UMAC_MDF_CTRL);
    ctrl |= 1 << (16 - which);
    write(UMAC_MDF_CTRL, ctrl);
}

fn board_mac() -> [u8; 6] {
    let mac = mailbox_mac();
    if mac.iter().any(|byte| *byte != 0) && mac != [0xff; 6] {
        return mac;
    }
    say("[WARN] GENET mailbox MAC unavailable, using 02:00:00:00:00:01");
    [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]
}

fn mailbox_mac() -> [u8; 6] {
    unsafe {
        let msg = &mut *core::ptr::addr_of_mut!(MAC_MSG);
        fill_mac_msg(msg);
        let ptr = msg as *mut MacMsg as *const u8;
        let phys = crate::addr::virt_to_phys(msg as *mut MacMsg as u64) as u32;
        // The buffer is Normal non-cacheable here, so the stores above
        // reach RAM. Invalidate before the read so a stale write-back
        // line cannot hide the GPU's reply.
        if !mailbox_call(phys | 0xC000_0000) {
            fill_mac_msg(msg);
            if !mailbox_call(phys) {
                return [0; 6];
            }
        }
        invalidate_poc(ptr, core::mem::size_of::<MacMsg>());
        if msg.code != 0x8000_0000 {
            return [0; 6];
        }
        msg.mac
    }
}

fn fill_mac_msg(msg: &mut MacMsg) {
    msg.size = 32;
    msg.code = 0;
    msg.tag = 0x0001_0003;
    msg.val_len = 6;
    msg.req = 0;
    msg.mac = [0; 6];
    msg.pad = [0; 2];
    msg.end = 0;
}

fn mailbox_call(addr: u32) -> bool {
    const MAILBOX: usize = crate::addr::phys_to_virt(0xFE00_B880) as usize;
    const STATUS: usize = 0x18;
    const WRITE: usize = 0x20;
    const READ: usize = 0x00;
    const FULL: u32 = 0x8000_0000;
    const EMPTY: u32 = 0x4000_0000;
    let msg = (addr & !0xf) | 8;
    let start = crate::timer::get_ticks();
    let budget = crate::timer::frequency() / 5;
    if budget == 0 {
        return false;
    }
    let mut spins = 0u32;
    while read_at(MAILBOX + STATUS) & FULL != 0 {
        spins = spins.saturating_add(1);
        if spins > 100_000 || crate::timer::get_ticks().wrapping_sub(start) > budget {
            return false;
        }
    }
    write_at(MAILBOX + WRITE, msg);
    loop {
        spins = spins.saturating_add(1);
        if spins > 200_000 || crate::timer::get_ticks().wrapping_sub(start) > budget {
            return false;
        }
        if read_at(MAILBOX + STATUS) & EMPTY != 0 {
            continue;
        }
        let response = read_at(MAILBOX + READ);
        if response & 0xf == 8 {
            return true;
        }
    }
}

fn say(text: &str) {
    crate::console::write_str(text);
    crate::console::write_str("\r\n");
}

fn say_hex(prefix: &str, value: u32) {
    let mut buf = [0u8; 80];
    let mut n = 0;
    for byte in prefix.bytes() {
        if n + 1 >= buf.len() {
            break;
        }
        buf[n] = byte;
        n += 1;
    }
    if n + 1 < buf.len() {
        buf[n] = b' ';
        n += 1;
    }
    if n + 2 < buf.len() {
        buf[n] = b'0';
        buf[n + 1] = b'x';
        n += 2;
    }
    for shift in (0..8).rev() {
        if n >= buf.len() {
            break;
        }
        let nibble = ((value >> (shift * 4)) & 0xf) as u8;
        buf[n] = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + (nibble - 10)
        };
        n += 1;
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        say(text);
    }
}

fn say_mac(prefix: &str, mac: &[u8; 6]) {
    let mut buf = [0u8; 80];
    let mut n = 0;
    for byte in prefix.bytes() {
        if n + 1 >= buf.len() {
            break;
        }
        buf[n] = byte;
        n += 1;
    }
    for (index, byte) in mac.iter().enumerate() {
        if n + 3 >= buf.len() {
            break;
        }
        if index > 0 {
            buf[n] = b':';
            n += 1;
        } else if n < buf.len() {
            buf[n] = b' ';
            n += 1;
        }
        buf[n] = hex_digit(byte >> 4);
        buf[n + 1] = hex_digit(byte & 0xf);
        n += 2;
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        say(text);
    }
}

fn hex_digit(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + (nibble - 10)
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

fn note_rx_drop(raw: u32) {
    if RX_DROP_NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    say_hex("[WARN] GENET RX drop", raw);
}

/// Push the transmit buffer to the point of coherency. The `.dma` window
/// is Normal non-cacheable, and a line that was still dirty from an earlier
/// mapping would otherwise be what the MAC reads.
fn clean_poc(ptr: *const u8, len: usize) {
    let start = (ptr as usize) & !63;
    let end = (ptr as usize).saturating_add(len);
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc civac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    barrier();
}

/// Drop stale write-back lines so a CPU read sees the MAC's bytes.
fn invalidate_poc(ptr: *const u8, len: usize) {
    let start = (ptr as usize) & !63;
    let end = (ptr as usize).saturating_add(len);
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc ivac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    barrier();
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
        service_link();
        if let Some(len) = crate::dhcp_rx::take_len() {
            return Some((
                GenetRxToken {
                    index: 0,
                    len,
                    source: RX_PROBE,
                },
                GenetTxToken { device: self },
            ));
        }
        if let Some(len) = crate::dhcp_rx::take_arp_len() {
            return Some((
                GenetRxToken {
                    index: 0,
                    len,
                    source: RX_ARP,
                },
                GenetTxToken { device: self },
            ));
        }
        let (index, len) = poll_rx()?;
        Some((
            GenetRxToken {
                index,
                len,
                source: RX_HW,
            },
            GenetTxToken { device: self },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if !LINK_UP.load(Ordering::Relaxed) || TX_STUCK.load(Ordering::Relaxed) {
            return None;
        }
        if !tx_ring_has_room() {
            return None;
        }
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

const RX_HW: u8 = 0;
const RX_PROBE: u8 = 1;
const RX_ARP: u8 = 2;

pub struct GenetRxToken {
    index: u16,
    len: u16,
    source: u8,
}

impl RxToken for GenetRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        if self.source == RX_PROBE {
            let mut tmp = [0u8; 384];
            let n = crate::dhcp_rx::copy_out(&mut tmp);
            let result = f(&mut tmp[..n]);
            say("[OK] DHCP offer probe delivered");
            return result;
        }
        if self.source == RX_ARP {
            let mut tmp = [0u8; 64];
            let n = crate::dhcp_rx::copy_arp(&mut tmp);
            if n >= 42 && crate::dhcp_rx::log_inject() {
                log::info!("gateway arp injected");
            }
            return f(&mut tmp[..n]);
        }
        let n = self.len as usize;
        let result = unsafe {
            let frame = RX_BUFS[self.index as usize % RX_RING_DESCS]
                .as_mut_ptr()
                .add(RX_ALIGN);
            // The early boot map was write-back. A stale line would hide
            // the bytes the MAC just wrote. Copy out and give the buffer
            // back before the stack or the console looks at it.
            invalidate_poc(frame, n);
            let copy = &mut *core::ptr::addr_of_mut!(RX_COPY);
            let take = n.min(copy.len());
            copy[..take].copy_from_slice(core::slice::from_raw_parts(frame, take));
            reclaim_rx_static(self.index);
            let slice = &mut copy[..take];
            match crate::dhcp_rx::verdict(slice, &our_mac()) {
                crate::dhcp_rx::Verdict::DropChaddr => {
                    if !DHCP_NOTED.swap(true, Ordering::Relaxed) {
                        say("[WARN] GENET DHCP chaddr is not the board MAC; dropped");
                    }
                    f(&mut [])
                }
                crate::dhcp_rx::Verdict::Pass => {
                    if slice.len() >= 12 {
                        let mut mac = [0u8; 6];
                        mac.copy_from_slice(&slice[6..12]);
                        crate::dhcp_rx::note_reply_mac(mac);
                    }
                    crate::config_manager::note_dhcp_dns_frame(slice);
                    if !DHCP_NOTED.swap(true, Ordering::Relaxed) {
                        say("[OK] GENET DHCP reply");
                    }
                    f(slice)
                }
                crate::dhcp_rx::Verdict::Ignore => {
                    crate::net::note_rx_frame(slice);
                    note_rx_arp(slice);
                    f(slice)
                }
            }
        };
        result
    }
}

fn poll_rx() -> Option<(u16, u16)> {
    let mut seen = 0;
    while seen < RX_RING_DESCS {
        seen += 1;
        let cons = RX_CONS.load(Ordering::Relaxed);
        let prod = read(RDMA_RING + RDMA_PROD_INDEX) & 0xffff;
        if prod == cons {
            return None;
        }
        let index = cons as usize % RX_RING_DESCS;
        let desc = GENET_RX_OFF + index * DMA_DESC_SIZE;
        let raw = read(desc + DESC_LENGTH_STATUS);
        barrier();
        let flags = raw & 0xffff;
        let length = ((raw >> DMA_BUFLENGTH_SHIFT) & DMA_BUFLENGTH_MASK) as usize;
        let sop_eop = DMA_SOP | DMA_EOP;
        if flags & DMA_RX_ERRORS != 0
            || (flags & sop_eop) != sop_eop
            || length <= RX_ALIGN
            || length > BUF_LEN
        {
            note_rx_drop(raw);
            reclaim_rx(index as u16, false);
            continue;
        }
        RX_BYTES.fetch_add((length - RX_ALIGN) as u64, Ordering::Relaxed);
        return Some((index as u16, (length - RX_ALIGN) as u16));
    }
    None
}

fn reclaim_rx_static(index: u16) {
    reclaim_rx(index, true);
}

fn reclaim_rx(_index: u16, good: bool) {
    // The address was programmed at init and stays. U-Boot only advances
    // the consumer.
    let cons = RX_CONS.load(Ordering::Relaxed).wrapping_add(1) & 0xffff;
    RX_CONS.store(cons, Ordering::Relaxed);
    write(RDMA_RING + RDMA_CONS_INDEX, cons);
    if good {
        RX_COUNT.fetch_add(1, Ordering::Relaxed);
    } else {
        RX_ERR.fetch_add(1, Ordering::Relaxed);
    }
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
