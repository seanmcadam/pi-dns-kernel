//! Pi 4 HDMI controller, PHY, and DDC.
//!
//! Register bases are the `bcm2711.dtsi` bus addresses translated by the
//! soc `ranges` entry `<0x7e000000 0x0 0xfe000000 0x01800000>`. That is the
//! same window as the mailbox at `0xFE00B880`, which this board answers.
//! `0x7ef00700` is HDMI0, `0x7ef00300` its DVP, `0x7ef00f00` its PHY,
//! `0x7ef20000` the HD block, and `0x7ef00000` the DVP clock/reset. HDMI1
//! is `0x7ef05700`. The DT lists HDMI1's `hd` region at the same
//! `0x7ef20000` as HDMI0; HDMI0 video control is `HD+0x44` and HDMI1's is
//! `HD+0x48`. HVS is `0x7e400000` and HDMI0's pixel valve is pixelvalve2
//! at `0x7e20a000` (HDMI1 is pixelvalve4 at `0x7e216000`). The page tables
//! map `0xFC000000..0x100000000` as Device-nGnRE, which covers every one
//! of those ARM addresses. One extra 2 MiB device block maps the 35-bit
//! alias `0x4_7EE00000`, where the same bus `0x7EFxxxxx` sits in full
//! 35-bit mode.
//!
//! `snapshot` only reads. It records which port has `HDMI_HOTPLUG` bit 0
//! set, and if neither does, which pixel valve has `PV_CONTROL` bit 0 set.
//! Pixelvalve2 is HDMI0 and pixelvalve4 is HDMI1 (`vc4_crtc`
//! `bcm2711_pv2_data` / `bcm2711_pv4_data`). Shutdown and recovery then
//! touch only those ports.
//!
//! `raspberrypi-power.h` has one HDMI domain, `RPI_POWER_DOMAIN_HDMI`
//! (DT index 5, firmware id 6). HDMI0 and HDMI1 share it. There is no
//! HDMI1 id. Firmware clocks 13 (`RPI_FIRMWARE_M2MC_CLK_ID`, the HSM
//! clock) and 14 (`RPI_FIRMWARE_PIXEL_BVB_CLK_ID`) are also shared. The
//! per-port gate is the DVP 108 MHz bit: bit 3 for HDMI0, bit 4 for
//! HDMI1 (`bcm2711-rpi.dtsi` `<&dvp 0>` / `<&dvp 1>`). Reset lines are
//! the same bits in `DVP_HT_RPI_SW_INIT`.
//!
//! A working HDMI1 picture on this board reads PHY reset `0x0f` and PHY
//! power `0x400` (bit 10). After a soft reboot the power word was
//! `0x7ff` and hot-plug was gone. The reset pulses the active port, then
//! writes `0x0f` and `0x400` again. It never writes `0x7ff`. The 35-bit
//! alias is used for a port only when that port's hot-plug bit is set
//! there and clear in the low-peripheral register. The HSM clock
//! (`CM_HSMCTL` at `0xFE101088`, enable bit 4) is turned on first if it
//! was clear. A read that faults is skipped and reported as `ffffffff`.
//! Virt and Pi 5 do not build this module.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::exception::read_u32_catching;
use crate::timer::{frequency, get_ticks};

const MISS: u32 = 0xffff_ffff;

/// Set when either active port's hot-plug bit is live only at the 35-bit alias.
static USE_HI: AtomicBool = AtomicBool::new(false);
/// Bit 0 is HDMI0, bit 1 is HDMI1. Zero means no port looked connected.
static PORT_MASK: AtomicU8 = AtomicU8::new(0);
static PORT0_HI: AtomicBool = AtomicBool::new(false);
static PORT1_HI: AtomicBool = AtomicBool::new(false);

const DVP_SW_INIT: u64 = 0x04;
const DVP_MISC: u64 = 0x08;
/// Bit 3 gates the HDMI0 108 MHz clock. Set means the clock is off.
const DVP_CLK0: u32 = 1 << 3;
/// Bit 4 gates the HDMI1 108 MHz clock.
const DVP_CLK1: u32 = 1 << 4;
/// `resets = <&dvp 0>` and `<&dvp 1>`.
const DVP_RST0: u32 = 1 << 0;
const DVP_RST1: u32 = 1 << 1;

/// Linux `vc5` `HDMI_HOTPLUG`. Bit 0 is connected.
const HOTPLUG: u64 = 0x1a8;
/// BCM2835 `HDMI_HOTPLUG`. Logged beside the vc5 register.
const HOTPLUG_2835: u64 = 0x0c;
/// Offset this kernel used before the vc5 map. Logged, not written.
const HOTPLUG_LEGACY: u64 = 0x1c;
/// Linux `vc5` `HDMI_SCHEDULER_CONTROL`. Bit 1 is `HDMI_ACTIVE`.
const SCHEDULER: u64 = 0xe0;
const CLOCK_STOP: u64 = 0xbc;

const PHY_RESET: u64 = 0x00;
const PHY_POWER: u64 = 0x04;
/// Lane resets. Linux `vc5_hdmi_reset_phy` writes this and leaves it.
const PHY_RESET_HOLD: u32 = 0x0f;
/// Linux `HDMI_TX_PHY_POWERDOWN_CTL` bit 10. A working picture reads this
/// alone (`0x400`). The failed post-reboot word `0x7ff` is never written.
const PHY_POWERDOWN: u32 = 1 << 10;

const VID_CTL0: u64 = 0x44;
const VID_CTL1: u64 = 0x48;
const VID_ENABLE: u32 = 1 << 31;

const AUTO_CTRL0: u64 = 0x26c;
const AUTO_RELEASE: u32 = 1 << 1;

const BSC_ADDR: u64 = 0x00;
const BSC_DATA_IN: u64 = 0x04;
const BSC_CNT: u64 = 0x24;
const BSC_CTL: u64 = 0x28;
const BSC_EN: u64 = 0x2c;
const BSC_DATA_OUT: u64 = 0x30;
const BSC_CTLHI: u64 = 0x50;
const BSC_SPEED: u32 = 0x90;
const BSC_INT_EN: u32 = 1 << 6;
const BSC_DTF_RD: u32 = 1;
const BSC_ENABLE: u32 = 1;
const BSC_INTRP: u32 = 1 << 1;
const BSC_NOACK: u32 = 1 << 2;
const BSC_NOSTOP: u32 = 1 << 4;
const BSC_NOSTART: u32 = 1 << 5;
const BSC_RESTART: u32 = 1 << 6;
const BSC_FOUR_BYTE: u32 = 1 << 6;
const EDID_ADDR_WR: u32 = 0xa0;
const EDID_ADDR_RD: u32 = 0xa1;

/// HVS `SCALER_DISPCTRL`. Bit 31 is the scaler enable.
const HVS: u64 = 0xFE40_0000;
/// Channel 0 and channel 2 display control. Bit 31 enables that channel.
const HVS_CH0: u64 = HVS + 0x40;
const HVS_CH2: u64 = HVS + 0x60;
/// pixelvalve2, the HDMI0 valve. `PV_CONTROL` bit 0 is enable.
const PV2: u64 = 0xFE20_A000;
/// pixelvalve4, the HDMI1 valve.
const PV4: u64 = 0xFE21_6000;
/// Power manager HDMI control. Always readable; not the HDMI register file.
const PM_HDMI: u64 = 0xFE10_0058;
/// CPRMAN `CM_HSMCTL` / `CM_HSMDIV`. Enable is bit 4. Password is separate.
const CM_HSMCTL: u64 = 0xFE10_1088;
const CM_HSMDIV: u64 = 0xFE10_108c;
const CM_PASSWORD: u32 = 0x5a00_0000;
const CM_ENABLE: u32 = 1 << 4;
/// AON interrupt controller. HDMI0 `hpd-connected` is interrupt 4.
const AON: u64 = 0xFEF0_0100;

struct Block {
    hdmi0: u64,
    hdmi1: u64,
    phy0: u64,
    phy1: u64,
    hd: u64,
    dvp_clk: u64,
    dvp: u64,
    bsc0: u64,
    bsc1: u64,
    auto0: u64,
    auto1: u64,
}

/// Low-peripheral view. This is what Linux programs on a Pi 4.
const LOW: Block = Block {
    hdmi0: 0xFEF0_0700,
    hdmi1: 0xFEF0_5700,
    phy0: 0xFEF0_0F00,
    phy1: 0xFEF0_5F00,
    hd: 0xFEF2_0000,
    dvp_clk: 0xFEF0_0000,
    dvp: 0xFEF0_0300,
    bsc0: 0xFEF0_4500,
    bsc1: 0xFEF0_9500,
    auto0: 0xFEF0_0B00,
    auto1: 0xFEF0_5B00,
};

/// Full 35-bit view of the same bus addresses (`0x4_0000_0000 | 0x7EFxxxxx`).
const HI: Block = Block {
    hdmi0: 0x4_7EF0_0700,
    hdmi1: 0x4_7EF0_5700,
    phy0: 0x4_7EF0_0F00,
    phy1: 0x4_7EF0_5F00,
    hd: 0x4_7EF2_0000,
    dvp_clk: 0x4_7EF0_0000,
    dvp: 0x4_7EF0_0300,
    bsc0: 0x4_7EF0_4500,
    bsc1: 0x4_7EF0_9500,
    auto0: 0x4_7EF0_0B00,
    auto1: 0x4_7EF0_5B00,
};

/// One read-only sample. `ffffffff` means the load faulted.
///
/// `hot` is HDMI0 `HDMI_HOTPLUG` (`+0x1a8`). `sch` is the scheduler.
/// `vid` is HD `HDMI_VID_CTL` (`+0x44`). `sw` and `clk` are the DVP reset
/// and the 108 MHz gate. `hvs` and `pv` are the scaler and HDMI0 pixel
/// valve. `hi` is the same hot-plug register in the 35-bit alias.
pub struct Snap {
    pub hot: u32,
    pub sch: u32,
    pub vid: u32,
    pub phy: u32,
    pub pwr: u32,
    pub sw: u32,
    pub clk: u32,
    pub hvs: u32,
    pub pv: u32,
    pub h1: u32,
    pub hi: u32,
    pub his: u32,
    pub leg: u32,
    pub old: u32,
    pub ctl: u32,
    pub cst: u32,
    pub phy1: u32,
    pub pwr1: u32,
    pub hi1: u32,
    pub pv4: u32,
    pub hc0: u32,
    pub hc2: u32,
    pub hsm: u32,
    pub hsd: u32,
    pub aon: u32,
    pub pm: u32,
}

pub struct Ports {
    pub h0: u32,
    pub h1: u32,
    pub legacy0: u32,
    pub phy0: u32,
    pub pwr0: u32,
    pub phy1: u32,
    pub pwr1: u32,
    pub dvp: u32,
    pub clk: u32,
    pub edid_w: u32,
    pub edid_h: u32,
}

/// Read the diagnostics. No writes. The HDMI register file and its PHY
/// are read only when `CM_HSMCTL` enable (bit 4) is set and that port's
/// DVP 108 MHz gate is clear. A gate bit set means the clock is stopped,
/// and a register access then can lock the bus. `PM_HDMI`, the DVP
/// block, and the HVS are still read. The 35-bit alias is used for a
/// later write only when that alias shows hot-plug and the
/// low-peripheral register does not.
pub fn snapshot() -> Snap {
    let hsm = read(CM_HSMCTL);
    let hsd = read(CM_HSMDIV);
    let sw = read(LOW.dvp_clk + DVP_SW_INIT);
    let clk = read(LOW.dvp_clk + DVP_MISC);
    let pm = read(PM_HDMI);
    let allow0 = port_clock(hsm, clk, 0);
    let allow1 = port_clock(hsm, clk, 1);
    let mut snap = Snap {
        hot: 0,
        sch: 0,
        vid: 0,
        phy: 0,
        pwr: 0,
        sw,
        clk,
        hvs: read(HVS),
        pv: 0,
        h1: 0,
        phy1: 0,
        pwr1: 0,
        hi: 0,
        hi1: 0,
        his: 0,
        leg: 0,
        old: 0,
        ctl: 0,
        cst: 0,
        pv4: 0,
        hc0: read(HVS_CH0),
        hc2: read(HVS_CH2),
        hsm,
        hsd,
        aon: 0,
        pm,
    };
    if allow0 {
        snap.hot = read(LOW.hdmi0 + HOTPLUG);
        snap.sch = read(LOW.hdmi0 + SCHEDULER);
        snap.vid = read(LOW.hd + VID_CTL0);
        snap.phy = read(LOW.phy0 + PHY_RESET);
        snap.pwr = read(LOW.phy0 + PHY_POWER);
        snap.hi = read(HI.hdmi0 + HOTPLUG);
        snap.his = read(HI.hdmi0 + SCHEDULER);
        snap.leg = read(LOW.hdmi0 + HOTPLUG_LEGACY);
        snap.old = read(LOW.hdmi0 + HOTPLUG_2835);
        snap.ctl = read(LOW.hd);
        snap.cst = read(LOW.dvp + CLOCK_STOP);
        snap.pv = read(PV2);
        snap.aon = read(AON);
    }
    if allow1 {
        snap.h1 = read(LOW.hdmi1 + HOTPLUG);
        snap.phy1 = read(LOW.phy1 + PHY_RESET);
        snap.pwr1 = read(LOW.phy1 + PHY_POWER);
        snap.hi1 = read(HI.hdmi1 + HOTPLUG);
        snap.pv4 = read(PV4);
    }
    let mask = active_ports(&snap);
    PORT0_HI.store(use_hi_port(snap.hot, snap.hi), Ordering::Relaxed);
    PORT1_HI.store(use_hi_port(snap.h1, snap.hi1), Ordering::Relaxed);
    PORT_MASK.store(mask, Ordering::Relaxed);
    USE_HI.store(
        (mask & 1 != 0 && PORT0_HI.load(Ordering::Relaxed))
            || (mask & 2 != 0 && PORT1_HI.load(Ordering::Relaxed)),
        Ordering::Relaxed,
    );
    snap
}

pub fn using_hi() -> bool {
    USE_HI.load(Ordering::Relaxed)
}

/// Bit 0 HDMI0, bit 1 HDMI1, from the last `snapshot`.
pub fn port_mask() -> u8 {
    PORT_MASK.load(Ordering::Relaxed)
}

/// `hdmi0`, `hdmi1`, `hdmi0+hdmi1`, or `none`.
pub fn port_text() -> &'static str {
    match port_mask() {
        1 => "hdmi0",
        2 => "hdmi1",
        3 => "hdmi0+hdmi1",
        _ => "none",
    }
}

/// Hot-plug bit 0 wins. A valve with `PV_CONTROL` enable set is the
/// fallback when neither hot-plug bit is set, so a picture that is up
/// without a KMS hot-plug bit still names its port.
pub fn active_ports(snap: &Snap) -> u8 {
    let mut mask = 0u8;
    if connected(snap.hot) || connected(snap.hi) {
        mask |= 1;
    }
    if connected(snap.h1) || connected(snap.hi1) {
        mask |= 2;
    }
    if mask == 0 {
        if flag(snap.pv, 1) {
            mask |= 1;
        }
        if flag(snap.pv4, 1) {
            mask |= 2;
        }
    }
    mask
}

/// Why hot-plug bit 0 is or is not set. The picture can be up while
/// `HDMI_HOTPLUG` stays 0: the firmware scanout enables HVS and the pixel
/// valve without programming the KMS hot-plug bit, or the HSM clock that
/// clocks the HDMI register file is off.
pub fn hpd_line(snap: &Snap) -> &'static str {
    match active_ports(snap) {
        1 => "hdmi hpd hdmi0",
        2 => "hdmi hpd hdmi1",
        3 => "hdmi hpd both",
        _ => {
            if snap.hot == MISS && snap.h1 == MISS {
                return "hdmi hpd fault";
            }
            if connected(snap.leg) || connected(snap.old) {
                return "hdmi hpd at legacy";
            }
            if pipe_live(snap) && snap.hsm != MISS && snap.hsm & CM_ENABLE == 0 {
                return "hdmi hpd 0 hsm off";
            }
            if pipe_live(snap) {
                return "hdmi hpd 0 pipe live";
            }
            "hdmi hpd 0"
        }
    }
}

/// Reset the active port or ports, then put each PHY back to the power
/// word measured while a picture was up (`0x0f` / `0x400`, never `0x7ff`).
/// A mask of zero writes nothing. Call `snapshot` first.
pub fn quiesce() -> Ports {
    let mask = port_mask();
    if mask != 0 {
        enable_hsm();
        if mask & 1 != 0 {
            settle_port(0);
        }
        if mask & 2 != 0 {
            settle_port(1);
        }
    }
    sample(0, 0)
}

/// `PM_RSTS` as it stands before this kernel writes the reboot pattern.
/// The watchdog reason bits are set by the reset itself, not by this read.
pub fn pm_rsts() -> u32 {
    read(0xFE10_0020)
}

/// Same per-port reset used by the optional shutdown path. A mask of
/// zero writes nothing: a missing hot-plug is not a reason to reset both
/// ports. Call `snapshot` first.
pub fn boot_recover() -> Ports {
    let mask = port_mask();
    if mask == 0 {
        return sample(0, 0);
    }
    let mut ports = quiesce();
    wait_ms(20);
    if let Some((w, h)) = read_edid(mask) {
        ports.edid_w = w;
        ports.edid_h = h;
    }
    ports
}

fn use_hi_port(low: u32, hi: u32) -> bool {
    connected(hi) && !connected(low)
}

/// HSM enable is bit 4. A DVP gate bit set means that port's 108 MHz
/// clock is stopped (`CLK_GATE_SET_TO_DISABLE`). Either one blocks a
/// read of that port's HDMI register file.
fn port_clock(hsm: u32, clk: u32, port: u8) -> bool {
    if hsm == MISS || hsm & CM_ENABLE == 0 || clk == MISS {
        return false;
    }
    let bit = if port == 0 { DVP_CLK0 } else { DVP_CLK1 };
    clk & bit == 0
}

fn connected(value: u32) -> bool {
    value != MISS && value & 1 != 0
}

fn pipe_live(snap: &Snap) -> bool {
    flag(snap.hvs, 1 << 31)
        || flag(snap.hc0, 1 << 31)
        || flag(snap.hc2, 1 << 31)
        || flag(snap.pv, 1)
        || flag(snap.pv4, 1)
        || flag(snap.sch, 1 << 1)
        || flag(snap.vid, VID_ENABLE)
}

fn flag(value: u32, mask: u32) -> bool {
    value != MISS && value & mask != 0
}

fn block_for(port: u8) -> Block {
    let hi = if port == 0 {
        PORT0_HI.load(Ordering::Relaxed)
    } else {
        PORT1_HI.load(Ordering::Relaxed)
    };
    if hi {
        HI
    } else {
        LOW
    }
}

fn settle_port(port: u8) {
    let block = block_for(port);
    let (rst, clk, vid, phy) = if port == 0 {
        (DVP_RST0, DVP_CLK0, VID_CTL0, block.phy0)
    } else {
        (DVP_RST1, DVP_CLK1, VID_CTL1, block.phy1)
    };
    ungate_bit(&block, clk);
    video_off(&block, vid);
    pulse_phy(phy);
    phy_ready(phy);
    pulse_dvp_bit(&block, rst);
    phy_ready(phy);
    ungate_bit(&block, clk);
}

fn sample(edid_w: u32, edid_h: u32) -> Ports {
    let block = LOW;
    Ports {
        h0: read(block.hdmi0 + HOTPLUG),
        h1: read(block.hdmi1 + HOTPLUG),
        legacy0: read(block.hdmi0 + HOTPLUG_LEGACY),
        phy0: read(block.phy0 + PHY_RESET),
        pwr0: read(block.phy0 + PHY_POWER),
        phy1: read(block.phy1 + PHY_RESET),
        pwr1: read(block.phy1 + PHY_POWER),
        dvp: read(block.dvp_clk + DVP_SW_INIT),
        clk: read(block.dvp_clk + DVP_MISC),
        edid_w,
        edid_h,
    }
}

/// The HDMI state-machine clock. Linux enables it before any HDMI register
/// access. Leave a running clock alone, and do not invent a divider.
fn enable_hsm() {
    let Some(raw) = read_ok(CM_HSMCTL) else {
        return;
    };
    if raw & CM_ENABLE != 0 {
        return;
    }
    let _ = write(CM_HSMCTL, CM_PASSWORD | raw | CM_ENABLE);
}

fn ungate_bit(block: &Block, bit: u32) {
    let pa = block.dvp_clk + DVP_MISC;
    let Some(raw) = read_ok(pa) else {
        return;
    };
    let _ = write(pa, raw & !bit);
}

fn video_off(block: &Block, off: u64) {
    let pa = block.hd + off;
    let Some(raw) = read_ok(pa) else {
        return;
    };
    if raw & VID_ENABLE == 0 {
        return;
    }
    let _ = write(pa, raw & !VID_ENABLE);
}

/// Pulse the lane resets, then restore the power word from a working
/// picture. `0x7ff` is the failed post-reboot value and is never written.
fn pulse_phy(phy: u64) {
    if read_ok(phy + PHY_RESET).is_none() {
        return;
    }
    let _ = write(phy + PHY_RESET, PHY_RESET_HOLD);
    wait_ms(1);
    let _ = write(phy + PHY_RESET, 0);
    wait_ms(1);
}

fn phy_ready(phy: u64) {
    if read_ok(phy + PHY_RESET).is_none() {
        return;
    }
    let _ = write(phy + PHY_RESET, PHY_RESET_HOLD);
    let _ = write(phy + PHY_POWER, PHY_POWERDOWN);
}

fn pulse_dvp_bit(block: &Block, bit: u32) {
    let pa = block.dvp_clk + DVP_SW_INIT;
    let Some(raw) = read_ok(pa) else {
        return;
    };
    let _ = write(pa, raw | bit);
    wait_ms(1);
    let _ = write(pa, raw & !bit);
}

fn read_edid(mask: u8) -> Option<(u32, u32)> {
    if mask & 1 != 0 {
        let block = block_for(0);
        if let Some(size) = edid_port(block.bsc0, block.auto0) {
            return Some(size);
        }
    }
    if mask & 2 != 0 {
        let block = block_for(1);
        if let Some(size) = edid_port(block.bsc1, block.auto1) {
            return Some(size);
        }
    }
    None
}

fn edid_port(bsc: u64, auto: u64) -> Option<(u32, u32)> {
    if read_ok(bsc + BSC_EN).is_none() {
        return None;
    }
    let start = get_ticks();
    let budget = frequency().max(1) / 25;
    if read_ok(auto + AUTO_CTRL0).is_some() {
        let _ = write(auto + AUTO_CTRL0, AUTO_RELEASE);
    }
    let _ = write(bsc + BSC_EN, 0);
    let offset = pack(&[0]);
    if !bsc_cmd(
        bsc,
        EDID_ADDR_WR,
        BSC_RESTART | BSC_NOSTOP,
        false,
        &[offset],
        1,
        &mut [],
        start,
        budget,
    ) {
        return None;
    }
    let mut block = [0u8; 128];
    let mut filled = 0;
    let mut cond = BSC_RESTART | BSC_NOSTOP;
    while filled < block.len() {
        if expired(start, budget) {
            return None;
        }
        let n = (block.len() - filled).min(32);
        let flags = if filled + n == block.len() {
            cond & !(BSC_RESTART | BSC_NOSTOP)
        } else {
            (cond & !BSC_RESTART) | BSC_NOSTOP
        };
        let mut words = [0u32; 8];
        if !bsc_cmd(
            bsc,
            EDID_ADDR_RD,
            flags,
            true,
            &[],
            n as u32,
            &mut words,
            start,
            budget,
        ) {
            return None;
        }
        let mut i = 0;
        while i < n {
            let byte = (words[i / 4] >> ((i % 4) * 8)) as u8;
            block[filled + i] = byte;
            i += 1;
        }
        filled += n;
        cond = BSC_NOSTART | BSC_NOSTOP;
    }
    edid::preferred(&block).map(|timing| (u32::from(timing.width), u32::from(timing.height)))
}

fn bsc_cmd(
    bsc: u64,
    addr: u32,
    flags: u32,
    is_read: bool,
    data_in: &[u32],
    nbytes: u32,
    data_out: &mut [u32],
    start: u64,
    budget: u64,
) -> bool {
    if !wait_intr(bsc, false, start, budget) {
        return false;
    }
    let dtf = if is_read { BSC_DTF_RD } else { 0 };
    if !write(bsc + BSC_CTLHI, BSC_FOUR_BYTE) {
        return false;
    }
    let _ = write(bsc + BSC_CTL, BSC_SPEED | BSC_INT_EN | dtf);
    if flags & BSC_NOSTART == 0 && !write(bsc + BSC_ADDR, addr) {
        return false;
    }
    let _ = write(bsc + BSC_CNT, nbytes & 0x3f);
    let mut i = 0;
    while i < data_in.len() {
        let _ = write(bsc + BSC_DATA_IN + (i as u64) * 4, data_in[i]);
        i += 1;
    }
    if !write(bsc + BSC_EN, flags | BSC_ENABLE) {
        return false;
    }
    if !wait_intr(bsc, true, start, budget) {
        let _ = write(bsc + BSC_EN, 0);
        return false;
    }
    let status = read(bsc + BSC_EN);
    if is_read {
        let mut n = 0;
        while n < data_out.len() {
            data_out[n] = read(bsc + BSC_DATA_OUT + (n as u64) * 4);
            n += 1;
        }
    }
    let _ = write(bsc + BSC_EN, 0);
    let _ = write(bsc + BSC_CTL, BSC_SPEED | dtf);
    status != MISS && status & BSC_NOACK == 0
}

fn pack(bytes: &[u8]) -> u32 {
    let mut word = 0u32;
    for byte in bytes {
        word = (word >> 8) | (u32::from(*byte) << 24);
    }
    word
}

fn wait_intr(bsc: u64, want_set: bool, start: u64, budget: u64) -> bool {
    loop {
        if expired(start, budget) {
            return false;
        }
        let Some(raw) = read_ok(bsc + BSC_EN) else {
            return false;
        };
        if (raw & BSC_INTRP != 0) == want_set {
            return true;
        }
    }
}

fn wait_ms(ms: u64) {
    let freq = frequency().max(1);
    let start = get_ticks();
    let need = (freq / 1000).saturating_mul(ms);
    while get_ticks().wrapping_sub(start) < need {
        core::hint::spin_loop();
    }
}

fn expired(start: u64, budget: u64) -> bool {
    get_ticks().wrapping_sub(start) > budget
}

fn read(pa: u64) -> u32 {
    read_ok(pa).unwrap_or(MISS)
}

fn read_ok(pa: u64) -> Option<u32> {
    read_u32_catching(reg(pa))
}

fn write(pa: u64, value: u32) -> bool {
    if read_ok(pa).is_none() {
        return false;
    }
    unsafe {
        core::ptr::write_volatile(reg(pa), value);
    }
    true
}

fn reg(pa: u64) -> *mut u32 {
    crate::addr::phys_to_virt(pa) as *mut u32
}
