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
//! status counter can move, then masked in the handler and re-armed from
//! the poll path only while the summary is clear, so a level line cannot
//! livelock the core.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

const BASE: usize = crate::addr::phys_to_virt(0xFD58_0000) as usize;

const SYS_REV_CTRL: usize = 0x00;
const SYS_PORT_CTRL: usize = 0x04;
const PORT_MODE_EXT_GPHY: u32 = 3;
const SYS_RBUF_FLUSH_CTRL: usize = 0x08;
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
const DMA_DESC_RAM_INIT_BUSY: u32 = 1 << 1;
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
const DMA_START_ADDR_HI: usize = 0x18;
const DMA_END_ADDR: usize = 0x1c;
const DMA_END_ADDR_HI: usize = 0x20;
const DMA_MBUF_DONE_THRESH: usize = 0x24;
const DMA_FLOW_PERIOD: usize = 0x28;
const DMA_READ_PTR: usize = 0x00;
const DMA_READ_PTR_HI: usize = 0x04;
const DMA_WRITE_PTR: usize = 0x2c;
const DMA_WRITE_PTR_HI: usize = 0x30;

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
const DMA_MAX_BURST: u32 = 0x10;
/// U-Boot's Pi 4 driver uses 8. TX follows that. RX keeps the Linux burst.
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
/// UniMAC MIB block, offsets from the GENET base. RX good packets, RX FCS
/// errors, and TX packets. These count frames the MAC accepted, which is
/// independent of the DMA rings.
const MIB_RX_PKT: usize = 0xc28;
const MIB_RX_MCA: usize = 0xc30;
const MIB_RX_BCA: usize = 0xc34;
const MIB_RX_FCS: usize = 0xc38;
const MIB_RX_ALN: usize = 0xc48;
const MIB_RX_OVR: usize = 0xc58;
const MIB_RX_POK: usize = 0xc64;
const MIB_RX_UC: usize = 0xc68;
const MIB_TX_PKT: usize = 0xca8;
const MIB_TX_FCS: usize = 0xcbc;
const MIB_TX_POK: usize = 0xcec;
const MIB_RX_RUNT: usize = 0xd00;
const RBUF_OVFL_CNT: usize = 0x394;
/// v1 kept this in the UniMAC block. v5 counts overflow at `RBUF_OVFL_CNT`.
const UMAC_RBUF_OVFL: usize = 0xe1c;
const RBUF_ERR_CNT: usize = 0x398;
const UMAC_MDF_ERR_CNT: usize = 0xe38;
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
const MII_PHYSID1: u32 = 2;
const MII_PHYSID2: u32 = 3;
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
/// Set around the TX register accesses so `read`/`write` can name the
/// register before the access. A hung access leaves the name on screen.
static TX_TRACE: AtomicBool = AtomicBool::new(false);
static TX_STEP: AtomicU32 = AtomicU32::new(0);
static TX_STUCK: AtomicBool = AtomicBool::new(false);
static LAST_MARK_LEN: AtomicUsize = AtomicUsize::new(0);
static mut LAST_MARK: [u8; 48] = [0; 48];
static TX_DROP: AtomicU32 = AtomicU32::new(0);
static DHCP_NOTED: AtomicBool = AtomicBool::new(false);
/// CPU copy of one received frame. The owner is the only caller of
/// `consume`, so this is not shared. The DMA buffer is returned before
/// the stack reads it.
static mut RX_COPY: [u8; BUF_LEN] = [0; BUF_LEN];
static SRC_FIXED: AtomicBool = AtomicBool::new(false);
static LAST_BMSR: AtomicU32 = AtomicU32::new(0);
static LAST_LPA: AtomicU32 = AtomicU32::new(0);

static PRESENT: AtomicBool = AtomicBool::new(false);
static TX_LOGS: AtomicU32 = AtomicU32::new(0);
static RX_LOGS: AtomicU32 = AtomicU32::new(0);
static RX_DROP_NOTED: AtomicBool = AtomicBool::new(false);
static SPEED_CAP: AtomicU32 = AtomicU32::new(0);
static ARP_TX: AtomicU32 = AtomicU32::new(0);
static ARP_RX: AtomicU32 = AtomicU32::new(0);
static ICMP_TX: AtomicU32 = AtomicU32::new(0);
static ICMP_RX: AtomicU32 = AtomicU32::new(0);
static HAVE_TX_HDR: AtomicBool = AtomicBool::new(false);
static HAVE_RX_HDR: AtomicBool = AtomicBool::new(false);
static LAST_TX: [AtomicU8; 14] = hdr_zeros();
static LAST_RX: [AtomicU8; 14] = hdr_zeros();

const fn hdr_zeros() -> [AtomicU8; 14] {
    [
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
        AtomicU8::new(0),
    ]
}

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

#[repr(C, align(16))]
struct MacMsg {
    size: u32,
    code: u32,
    tag: u32,
    val_len: u32,
    req: u32,
    mac: [u8; 6],
    pad: [u8; 2],
    end: u32,
}

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
        line.push(" DOWN bmsr=");
        line.hex32(LAST_BMSR.load(Ordering::Relaxed));
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

/// Second and third pinned rows. The second is the UniMAC and RDMA
/// breakdown. The third is the last Ethernet header each way, plus how
/// many of those frames were ARP or ICMP.
pub fn write_diag(out: &mut [u8]) -> usize {
    let mut line = StatusLine { buf: out, n: 0 };
    if !PRESENT.load(Ordering::Relaxed) {
        line.push("txS=- mib=-");
        return line.n;
    }
    let rx_prod_raw = read(RDMA_RING + RDMA_PROD_INDEX);
    line.push("tS=");
    line.dec(TX_COUNT.load(Ordering::Relaxed));
    line.push(" tPkt=");
    line.dec(read(MIB_TX_PKT));
    line.push(" tOk=");
    line.dec(read(MIB_TX_POK));
    line.push(" tFcs=");
    line.dec(read(MIB_TX_FCS));
    // Ring END, not an error counter. 256 descriptors * 3 words - 1 is 0x2ff.
    line.push(" end=");
    line.hex32(read(RDMA_RING + DMA_END_ADDR));
    line.push(" h=");
    line.hex32(read(HFB_CTRL));
    line.push(" r=");
    line.dec(read(MIB_RX_PKT));
    line.push(" rOk=");
    line.dec(read(MIB_RX_POK));
    line.push(" bca=");
    line.dec(read(MIB_RX_BCA));
    line.push(" mca=");
    line.dec(read(MIB_RX_MCA));
    line.push(" uc=");
    line.dec(read(MIB_RX_UC));
    line.push(" fcs=");
    line.dec(read(MIB_RX_FCS));
    line.push(" aln=");
    line.dec(read(MIB_RX_ALN));
    line.push(" ovr=");
    line.dec(read(MIB_RX_OVR));
    line.push(" run=");
    line.dec(read(MIB_RX_RUNT));
    line.push(" ovf=");
    line.dec(read(RBUF_OVFL_CNT));
    line.push(" uov=");
    line.dec(read(UMAC_RBUF_OVFL));
    line.push(" rer=");
    line.dec(read(RBUF_ERR_CNT));
    line.push(" mdf=");
    line.dec(read(UMAC_MDF_ERR_CNT));
    line.push(" p=");
    line.dec(rx_prod_raw & 0xffff);
    line.push(" c=");
    line.dec(read(RDMA_RING + RDMA_CONS_INDEX) & 0xffff);
    line.push(" d=");
    line.dec(rx_prod_raw >> 16);
    let mark_n = LAST_MARK_LEN.load(Ordering::Acquire);
    if mark_n > 0 {
        line.push(" mk=");
        let n = mark_n.min(48);
        let bytes = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(LAST_MARK).cast::<u8>(), n) };
        line.push_bytes(bytes);
    }
    line.push("\n");
    if HAVE_TX_HDR.load(Ordering::Relaxed) {
        line.push("tx ");
        line.hdr(&LAST_TX);
    } else {
        line.push("tx -");
    }
    line.push(" rx ");
    if HAVE_RX_HDR.load(Ordering::Relaxed) {
        line.hdr(&LAST_RX);
    } else {
        line.push("-");
    }
    line.push(" arp=");
    line.dec(ARP_RX.load(Ordering::Relaxed));
    line.push("/");
    line.dec(ARP_TX.load(Ordering::Relaxed));
    line.push(" icmp=");
    line.dec(ICMP_RX.load(Ordering::Relaxed));
    line.push("/");
    line.dec(ICMP_TX.load(Ordering::Relaxed));
    line.push(" rxE=");
    line.dec(RX_ERR.load(Ordering::Relaxed));
    line.n
}

struct StatusLine<'a> {
    buf: &'a mut [u8],
    n: usize,
}

impl StatusLine<'_> {
    fn push_bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let tmp = [byte];
            if let Ok(text) = core::str::from_utf8(&tmp) {
                self.push(text);
            }
        }
    }

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

    fn hex32(&mut self, value: u32) {
        for shift in (0..8).rev() {
            let nibble = ((value >> (shift * 4)) & 0xf) as u8;
            if self.n >= self.buf.len() {
                return;
            }
            self.buf[self.n] = if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + (nibble - 10)
            };
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

    fn hdr(&mut self, bytes: &[AtomicU8; 14]) {
        for index in 0..6 {
            if index > 0 {
                self.push(":");
            }
            self.byte(bytes[index].load(Ordering::Relaxed));
        }
        self.push(" ");
        for index in 6..12 {
            if index > 6 {
                self.push(":");
            }
            self.byte(bytes[index].load(Ordering::Relaxed));
        }
        self.push(" ");
        self.byte(bytes[12].load(Ordering::Relaxed));
        self.byte(bytes[13].load(Ordering::Relaxed));
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

        let rx_cons = read(RDMA_RING + RDMA_PROD_INDEX) & 0xffff;
        write(RDMA_RING + RDMA_CONS_INDEX, rx_cons);
        RX_CONS.store(rx_cons, Ordering::Relaxed);
        align_tx_prod();

        PRESENT.store(true, Ordering::Relaxed);
        service_link();
        crate::dhcp_rx::arm(&mac);
        say("[OK] GENET v5 TX and RX rings are 256 descriptors");
        unsafe {
            let buf0 = crate::addr::virt_to_phys(TX_BUFS[0].as_ptr() as u64);
            let buf1 = crate::addr::virt_to_phys(TX_BUFS[1].as_ptr() as u64);
            say_hex("[OK] GENET TX buf0", buf0 as u32);
            say_hex("[OK] GENET TX buf1", buf1 as u32);
            if buf0 >> 32 != 0 || buf1 >> 32 != 0 {
                say_hex("[WARN] GENET TX buf high", (buf0 >> 32) as u32);
            }
            if buf0 >= 0x4000_0000 || buf1 >= 0x4000_0000 {
                say("[WARN] GENET TX buffer is at or above 1 GiB");
            }
        }

        Some(Self { mac })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn transmit_frame(&mut self, frame: &[u8]) {
        if TX_STUCK.load(Ordering::Relaxed) {
            mark_text("TX stuck");
            return;
        }
        // U-Boot `bcmgenet_gmac_eth_send`: program the 12-byte descriptor
        // for `(prod & 0xff)`, then write `prod + 1` to TDMA_PROD_INDEX,
        // and wait until the consumer catches it. The write pointer is
        // left at the value init stored while DMA was off. Moving it
        // after DMA is enabled points the engine at the next descriptor
        // before that descriptor is valid; the following producer update
        // is the access that locks the AXI fabric. Linux `bcmgenet_xmit`
        // likewise publishes only the producer index.
        TX_STEP.store(0, Ordering::Relaxed);
        TX_TRACE.store(true, Ordering::Release);
        let raw_prod = read(TDMA_RING + TDMA_PROD_INDEX);
        let prod = raw_prod & 0xffff;
        let cons = read(TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
        if prod.wrapping_sub(cons) & 0xffff >= DESC_COUNT as u32 {
            TX_DROP.fetch_add(1, Ordering::Relaxed);
            TX_TRACE.store(false, Ordering::Release);
            mark_text("TX full");
            return;
        }
        let desc_index = (prod as usize) & 0xff;
        let slot = desc_index % DESC_COUNT;
        let mut n = frame.len().min(BUF_LEN);
        if n < 60 {
            n = 60;
        }
        unsafe {
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
            mark_tx_buf(buf.as_ptr(), n, desc_index);
            clean_poc(buf.as_ptr(), n);
            note_l2(buf.as_ptr(), n, true);
            let addr = crate::addr::virt_to_phys(buf.as_ptr() as u64);
            let status = ((n as u32) << DMA_BUFLENGTH_SHIFT)
                | DMA_SOP
                | DMA_EOP
                | DMA_TX_APPEND_CRC
                | (0x3f << DMA_TX_QTAG_SHIFT);
            let desc = GENET_TX_OFF + desc_index * DMA_DESC_SIZE;
            write(desc + DESC_ADDRESS_LO, addr as u32);
            write(desc + DESC_ADDRESS_HI, (addr >> 32) as u32);
            barrier();
            write(desc + DESC_LENGTH_STATUS, status);
            note_tx_frame(buf.as_ptr(), n, status);
        }
        let next = prod.wrapping_add(1) & 0xffff;
        barrier();
        // Keep any high bits the register already held. U-Boot increments
        // the raw producer and writes it back; the index in use is the
        // low 16 bits, and the descriptor slot is that value mod 256.
        write(TDMA_RING + TDMA_PROD_INDEX, (raw_prod & !0xffff) | next);
        TX_PROD.store(next, Ordering::Relaxed);
        TX_COUNT.fetch_add(1, Ordering::Relaxed);
        TX_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        TX_TRACE.store(false, Ordering::Release);
        mark_text("TX prod posted");
        let start = crate::timer::millis();
        loop {
            let got = read_at(BASE + TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
            let advanced = got.wrapping_sub(prod) & 0xffff;
            let posted = next.wrapping_sub(prod) & 0xffff;
            if advanced >= posted {
                mark_text("TX cons caught");
                return;
            }
            if crate::timer::millis().saturating_sub(start) > 20 {
                mark_text("TX cons timeout");
                say("[WARN] GENET TX consumer did not catch the producer");
                TX_STUCK.store(true, Ordering::Relaxed);
                return;
            }
        }
    }
}

fn tx_ring_has_room() -> bool {
    let prod = TX_PROD.load(Ordering::Relaxed);
    let cons = read(TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
    prod.wrapping_sub(cons) & 0xffff < DESC_COUNT as u32
}

/// The consumer is hardware-owned. U-Boot does not write it. The producer
/// starts equal to it, and the first free descriptor is that index.
fn align_tx_prod() {
    let cons = read(TDMA_RING + TDMA_CONS_INDEX) & 0xffff;
    write(TDMA_RING + TDMA_PROD_INDEX, cons);
    TX_PROD.store(cons, Ordering::Relaxed);
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
    write(SYS_RBUF_FLUSH_CTRL, 1 << 1);
    spin(1000);
    write(SYS_RBUF_FLUSH_CTRL, 0);
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
    say("[OK] GENET RBUF align 2, HFB off, TBUF left as reset");
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
    say_hex("[OK] GENET TDMA_CTRL", read(TDMA_REG_BASE + DMA_CTRL));
    say_hex("[OK] GENET TDMA_STATUS", read(TDMA_REG_BASE + DMA_STATUS));
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
    write(RDMA_REG_BASE + DMA_SCB_BURST_SIZE, DMA_MAX_BURST);
    // The producer is hardware-owned. U-Boot does not write it; the
    // consumer is aligned to whatever the register already holds.
    write(RDMA_RING + DMA_START_ADDR, 0);
    write(RDMA_RING + DMA_START_ADDR_HI, 0);
    write(RDMA_RING + DMA_READ_PTR, 0);
    write(RDMA_RING + DMA_READ_PTR_HI, 0);
    write(RDMA_RING + DMA_WRITE_PTR, 0);
    write(RDMA_RING + DMA_WRITE_PTR_HI, 0);
    write(RDMA_RING + DMA_END_ADDR, end);
    write(RDMA_RING + DMA_END_ADDR_HI, 0);
    write(RDMA_RING + DMA_RING_BUF_SIZE_REG, size);
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
    wait_desc_ram(TDMA_REG_BASE);
    // U-Boot's Pi 4 driver: one ring, descriptors 0..255, no high pointer
    // words, no arbiter write. END is the last word (256 * 3 - 1). The
    // size field is the descriptor count in the high half and the buffer
    // length in the low half. START, READ, END, and the consumer are
    // write-once after a BCM2711 reset, so the value that reads back is
    // the one the engine will use.
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
    if got_end != end || got_size != size {
        say("[WARN] GENET TDMA ring end or size did not stick");
    }
    say("[OK] GENET TDMA ring 16 uses 256 descriptors from 0");
}

fn wait_desc_ram(base: usize) {
    let start = crate::timer::millis();
    let mut spins = 0u32;
    while crate::timer::millis().saturating_sub(start) < 20 {
        spins = spins.saturating_add(1);
        if spins > 100_000 {
            break;
        }
        if read(base + DMA_STATUS) & DMA_DESC_RAM_INIT_BUSY == 0 {
            return;
        }
        core::hint::spin_loop();
    }
    say("[WARN] GENET DMA descriptor RAM init still busy");
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
    crate::gic::arm_spi(GENET_IRQ0, 0x80, 1);
    crate::gic::arm_spi(GENET_IRQ1, 0x80, 1);
    say("[OK] GENET IRQ armed, data path is polled");
}

static IRQ_HELD: AtomicBool = AtomicBool::new(false);

/// The handler masks the GIC line. Re-arm it only after the summary has
/// gone quiet, so a level source cannot re-enter before the poll path runs.
pub fn note_irq_held() {
    IRQ_HELD.store(true, Ordering::Relaxed);
}

pub fn rearm_irq() {
    if !PRESENT.load(Ordering::Relaxed) || !IRQ_HELD.load(Ordering::Relaxed) {
        return;
    }
    if irq_pending(INTRL2_0) || irq_pending(INTRL2_1) {
        return;
    }
    crate::gic::enable_id(GENET_IRQ0);
    crate::gic::enable_id(GENET_IRQ1);
    IRQ_HELD.store(false, Ordering::Relaxed);
}

fn irq_pending(base: usize) -> bool {
    let stat = read(base + INTRL2_STAT);
    let mask = read(base + INTRL2_MASK_STATUS);
    stat & !mask != 0
}

/// Clear the GENET interrupt summary. `false` means the line stayed
/// asserted, and the caller should mask it in the GIC.
pub fn ack_irq(which: u32) -> bool {
    let base = if which == 0 { INTRL2_0 } else { INTRL2_1 };
    let stat_addr = (BASE + base + INTRL2_STAT) as *const u32;
    let Some(stat) = crate::exception::read_u32_catching(stat_addr) else {
        return false;
    };
    write(base + INTRL2_CLEAR, stat);
    let Some(left) = crate::exception::read_u32_catching(stat_addr) else {
        return false;
    };
    let mask = read(base + INTRL2_MASK_STATUS);
    left & !mask == 0
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
    let id1 = mdio_read(phy, MII_PHYSID1).unwrap_or(0);
    let id2 = mdio_read(phy, MII_PHYSID2).unwrap_or(0);
    say_hex("[OK] GENET PHY id1", id1 as u32);
    say_hex("[OK] GENET PHY id2", id2 as u32);

    if mdio_write(phy, MII_BMCR, BMCR_RESET).is_none() {
        return false;
    }
    // Eight MDIO reads finish in well under a millisecond. The PHY reset
    // bit stays set for longer than that, and the advertise / autoneg
    // writes that follow are ignored while it is set, so the copper link
    // never restarts and BMSR bit 2 stays clear.
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
    if mdio_write(phy, AUXCTL, AUX_MISC | value | AUX_RGMII_SKEW).is_some() {
        say_hex("[OK] GENET PHY RX skew shadow", (value | AUX_RGMII_SKEW) as u32);
    }
}

/// Shadow 0x03 bit 9 is the PHY's own TX clock delay. rgmii-rxid leaves
/// that to the MAC, so a strap that set it would delay TX twice at 1000.
fn clear_phy_tx_delay(phy: u32) {
    let Some(raw) = shadow_read(phy, 0x03) else {
        say("[WARN] GENET PHY TX clock shadow did not read");
        return;
    };
    let cleared = raw & !(1 << 9);
    if shadow_write(phy, 0x03, cleared).is_some() {
        say_hex("[OK] GENET PHY TX clock shadow", cleared as u32);
    }
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
        LAST_BMSR.store(0xffff_ffff, Ordering::Relaxed);
        return;
    };
    let status = mdio_read(phy, MII_BMSR).unwrap_or(first);
    LAST_BMSR.store(status as u32, Ordering::Relaxed);
    LAST_LPA.store(
        mdio_read(phy, MII_LPA).unwrap_or(0) as u32,
        Ordering::Relaxed,
    );
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
    if up {
        say_hex("[OK] GENET UMAC_CMD", cmd);
    }
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
    if link {
        say_hex("[OK] GENET RGMII OOB", oob);
    }
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
/// had a different address, which is the `16:E7:...` frame on the photo.
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
        msg.size = 32;
        msg.code = 0;
        msg.tag = 0x0001_0003;
        msg.val_len = 6;
        msg.req = 0;
        msg.mac = [0; 6];
        msg.pad = [0; 2];
        msg.end = 0;
        let phys = crate::addr::virt_to_phys(msg as *mut MacMsg as u64) as u32;
        if !mailbox_call(phys | 0xC000_0000) {
            msg.size = 32;
            msg.code = 0;
            msg.tag = 0x0001_0003;
            msg.val_len = 6;
            msg.req = 0;
            msg.mac = [0; 6];
            msg.end = 0;
            if !mailbox_call(phys) {
                return [0; 6];
            }
        }
        if msg.code != 0x8000_0000 {
            return [0; 6];
        }
        msg.mac
    }
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

/// UART, then the heartbeat row, then a data barrier and a clean of that
/// row. Called before the GENET access it names. No log queue and no
/// framebuffer lock: the core that hangs may already hold either.
fn mark_text(text: &str) {
    remember_mark(text.as_bytes());
    raw_uart(text.as_bytes());
    raw_uart(b"\r\n");
    barrier();
    crate::fb_con::raw_mark(text);
    barrier();
}

fn mark_mmio(kind: u8, offset: usize, value: u32) {
    let step = TX_STEP.fetch_add(1, Ordering::Relaxed);
    let mut buf = [0u8; 40];
    let mut n = 0;
    push_str(&mut buf, &mut n, "TX");
    buf[n] = kind;
    n += 1;
    buf[n] = b' ';
    n += 1;
    push_dec(&mut buf, &mut n, step);
    buf[n] = b' ';
    n += 1;
    push_hex32(&mut buf, &mut n, offset as u32);
    buf[n] = b'=';
    n += 1;
    push_hex32(&mut buf, &mut n, value);
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        mark_text(text);
    }
}

fn mark_tx_buf(ptr: *const u8, len: usize, desc: usize) {
    let mut buf = [0u8; 40];
    let mut n = 0;
    push_str(&mut buf, &mut n, "TX buf d");
    push_dec(&mut buf, &mut n, desc as u32);
    push_str(&mut buf, &mut n, " n");
    push_dec(&mut buf, &mut n, len as u32);
    if len >= 14 {
        let bytes = unsafe { core::slice::from_raw_parts(ptr, 14) };
        push_str(&mut buf, &mut n, " et");
        push_hex8(&mut buf, &mut n, bytes[12]);
        push_hex8(&mut buf, &mut n, bytes[13]);
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        mark_text(text);
    }
}

fn mark_rx(frame: &[u8]) {
    let mut buf = [0u8; 24];
    let mut n = 0;
    push_str(&mut buf, &mut n, "RX");
    if frame.len() >= 14 {
        push_str(&mut buf, &mut n, " et");
        push_hex8(&mut buf, &mut n, frame[12]);
        push_hex8(&mut buf, &mut n, frame[13]);
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        mark_text(text);
    }
}

fn remember_mark(bytes: &[u8]) {
    let n = bytes.len().min(48);
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), core::ptr::addr_of_mut!(LAST_MARK).cast(), n);
    }
    LAST_MARK_LEN.store(n, Ordering::Release);
}

fn raw_uart(bytes: &[u8]) {
    let uart = crate::addr::phys_to_virt(crate::board::UART_PHYS) as *mut u32;
    for &byte in bytes {
        unsafe {
            write_volatile(uart, byte as u32);
        }
    }
}

fn push_hex32(buf: &mut [u8], n: &mut usize, value: u32) {
    for shift in (0..8).rev() {
        if *n >= buf.len() {
            return;
        }
        let nibble = ((value >> (shift * 4)) & 0xf) as u8;
        buf[*n] = hex_digit(nibble);
        *n += 1;
    }
}

fn push_hex8(buf: &mut [u8], n: &mut usize, value: u8) {
    if *n + 2 > buf.len() {
        return;
    }
    buf[*n] = hex_digit(value >> 4);
    buf[*n + 1] = hex_digit(value & 0xf);
    *n += 2;
}

fn read(offset: usize) -> u32 {
    if TX_TRACE.load(Ordering::Relaxed) {
        mark_mmio(b'r', offset, 0);
    }
    read_at(BASE + offset)
}

fn write(offset: usize, value: u32) {
    if TX_TRACE.load(Ordering::Relaxed) {
        mark_mmio(b'w', offset, value);
    }
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

fn note_l2(ptr: *const u8, len: usize, tx: bool) {
    if len < 14 {
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, 14) };
    let dest = if tx { &LAST_TX } else { &LAST_RX };
    for (slot, byte) in dest.iter().zip(bytes.iter()) {
        slot.store(*byte, Ordering::Relaxed);
    }
    if tx {
        HAVE_TX_HDR.store(true, Ordering::Relaxed);
    } else {
        HAVE_RX_HDR.store(true, Ordering::Relaxed);
    }
    let ethertype = u16::from_be_bytes([bytes[12], bytes[13]]);
    if ethertype == 0x0806 {
        if tx {
            ARP_TX.fetch_add(1, Ordering::Relaxed);
        } else {
            ARP_RX.fetch_add(1, Ordering::Relaxed);
        }
    } else if ethertype == 0x0800 && len >= 34 && bytes_at(ptr, 23) == 1 {
        if tx {
            ICMP_TX.fetch_add(1, Ordering::Relaxed);
        } else {
            ICMP_RX.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn bytes_at(ptr: *const u8, index: usize) -> u8 {
    unsafe { ptr.add(index).read() }
}

fn note_tx_frame(ptr: *const u8, len: usize, status: u32) {
    let which = TX_LOGS.fetch_add(1, Ordering::Relaxed);
    if which >= 4 {
        return;
    }
    let qtag = (status >> DMA_TX_QTAG_SHIFT) & 0x3f;
    if which == 0 {
        say("[OK] GENET TX length is the buffer, low half is SOP EOP CRC qtag");
    }
    let mut buf = [0u8; 96];
    let mut n = 0;
    push_str(&mut buf, &mut n, "[OK] GENET TX d");
    push_dec(&mut buf, &mut n, which);
    push_str(&mut buf, &mut n, " len ");
    push_dec(&mut buf, &mut n, len as u32);
    push_str(&mut buf, &mut n, " flags");
    if status & DMA_SOP != 0 {
        push_str(&mut buf, &mut n, " SOP");
    }
    if status & DMA_EOP != 0 {
        push_str(&mut buf, &mut n, " EOP");
    }
    if status & DMA_TX_APPEND_CRC != 0 {
        push_str(&mut buf, &mut n, " CRC");
    }
    push_str(&mut buf, &mut n, " qtag ");
    push_dec(&mut buf, &mut n, qtag);
    finish_say(&buf, n);
    say_bytes("[OK] GENET TX hdr", ptr, len.min(16));
}

fn note_rx_frame(ptr: *const u8, len: usize) {
    let which = RX_LOGS.fetch_add(1, Ordering::Relaxed);
    if which >= 4 {
        return;
    }
    let mut buf = [0u8; 24];
    let mut n = 0;
    push_str(&mut buf, &mut n, "[OK] GENET RX");
    push_dec(&mut buf, &mut n, which);
    finish_say(&buf, n);
    say_bytes("[OK] GENET RX hdr", ptr, len.min(16));
}

fn note_rx_drop(raw: u32) {
    if RX_DROP_NOTED.swap(true, Ordering::Relaxed) {
        return;
    }
    say_hex("[WARN] GENET RX drop", raw);
}

fn push_str(buf: &mut [u8], n: &mut usize, text: &str) {
    for byte in text.bytes() {
        if *n >= buf.len() {
            return;
        }
        buf[*n] = byte;
        *n += 1;
    }
}

fn push_dec(buf: &mut [u8], n: &mut usize, mut value: u32) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    if value == 0 {
        push_str(buf, n, "0");
        return;
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    push_str(buf, n, unsafe { core::str::from_utf8_unchecked(&tmp[i..]) });
}

fn finish_say(buf: &[u8], n: usize) {
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        say(text);
    }
}

fn say_bytes(prefix: &str, ptr: *const u8, len: usize) {
    let mut buf = [0u8; 96];
    let mut n = 0;
    push_str(&mut buf, &mut n, prefix);
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    for byte in bytes {
        if n + 3 >= buf.len() {
            break;
        }
        buf[n] = b' ';
        buf[n + 1] = hex_digit(*byte >> 4);
        buf[n + 2] = hex_digit(*byte & 0xf);
        n += 3;
    }
    finish_say(&buf, n);
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
                    probe: true,
                },
                GenetTxToken { device: self },
            ));
        }
        let (index, len) = poll_rx()?;
        Some((
            GenetRxToken {
                index,
                len,
                probe: false,
            },
            GenetTxToken { device: self },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if !LINK_UP.load(Ordering::Relaxed) || TX_STUCK.load(Ordering::Relaxed) {
            return None;
        }
        TX_STEP.store(0, Ordering::Relaxed);
        TX_TRACE.store(true, Ordering::Release);
        let room = tx_ring_has_room();
        TX_TRACE.store(false, Ordering::Release);
        if !room {
            mark_text("TX noroom");
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

pub struct GenetRxToken {
    index: u16,
    len: u16,
    probe: bool,
}

impl RxToken for GenetRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        if self.probe {
            let mut tmp = [0u8; 384];
            let n = crate::dhcp_rx::copy_out(&mut tmp);
            let result = f(&mut tmp[..n]);
            say("[OK] DHCP offer probe delivered");
            return result;
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
            note_l2(copy.as_ptr(), take, false);
            note_rx_frame(copy.as_ptr(), take);
            let slice = &mut copy[..take];
            match crate::dhcp_rx::verdict(slice, &our_mac()) {
                crate::dhcp_rx::Verdict::DropChaddr => {
                    if !DHCP_NOTED.swap(true, Ordering::Relaxed) {
                        say("[WARN] GENET DHCP chaddr is not the board MAC; dropped");
                    }
                    mark_text("RX drop chaddr");
                    f(&mut [])
                }
                crate::dhcp_rx::Verdict::Pass => {
                    if !DHCP_NOTED.swap(true, Ordering::Relaxed) {
                        say("[OK] GENET DHCP reply");
                    }
                    mark_text("RX dhcp");
                    f(slice)
                }
                crate::dhcp_rx::Verdict::Ignore => {
                    mark_rx(slice);
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
    // the consumer. Rewriting the length/status word (OWN | 2048) while the
    // DMA may still be reading descriptor RAM is the other way this engine
    // locks the bus on the buffer that just held the offer.
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
        mark_text("TX tok");
        let mut buf = [0u8; 1514];
        let n = len.min(buf.len());
        let result = f(&mut buf[..n]);
        self.device.transmit_frame(&buf[..n]);
        result
    }
}
