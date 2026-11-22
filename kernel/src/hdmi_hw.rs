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
//! `snapshot` only reads. Shutdown and boot recovery call it first, then
//! follow `vc5_hdmi_phy_disable` and `vc5_hdmi_reset` on the low-peripheral
//! bases. The 35-bit alias is used for those writes only when its hot-plug
//! bit is set and the low-peripheral bit is not. The HSM clock
//! (`CM_HSMCTL` at `0xFE101088`, enable bit 4) is turned on then, and only
//! if that bit was clear, so a gated register file can take the PHY write.
//! The 108 MHz DVP gates stay clear: setting them stops the firmware from
//! programming the PHY. A read that faults is skipped and reported as
//! `ffffffff`. Virt and Pi 5 do not build this module.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::exception::read_u32_catching;
use crate::timer::{frequency, get_ticks};

const MISS: u32 = 0xffff_ffff;

/// Set when the 35-bit alias is the aperture whose hot-plug bit is live.
static USE_HI: AtomicBool = AtomicBool::new(false);

const DVP_SW_INIT: u64 = 0x04;
const DVP_MISC: u64 = 0x08;
/// Bits 3 and 4 gate the HDMI 108 MHz clocks. Set means the clock is off.
const DVP_CLK_GATE: u32 = (1 << 3) | (1 << 4);
/// Reset lines 0 and 1 are HDMI0 and HDMI1.
const DVP_HDMI_RST: u32 = (1 << 0) | (1 << 1);

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
/// Linux `HDMI_TX_PHY_POWERDOWN_CTL` bit 10, held across `phy_disable`.
const PHY_POWERDOWN: u32 = 1 << 10;
/// Randomizer power-down. Linux clears the lane power-down and leaves this.
const PHY_RNG_OFF: u32 = 1 << 4;

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
    pub pv4: u32,
    pub hc0: u32,
    pub hc2: u32,
    pub hsm: u32,
    pub hsd: u32,
    pub aon: u32,
}

pub struct Ports {
    pub h0: u32,
    pub h1: u32,
    pub legacy0: u32,
    pub phy0: u32,
    pub dvp: u32,
    pub clk: u32,
    pub edid_w: u32,
    pub edid_h: u32,
}

/// Read every diagnostic register. No writes. Picks the 35-bit alias for
/// later writes only when that alias shows hot-plug and the low-peripheral
/// register does not.
pub fn snapshot() -> Snap {
    let snap = Snap {
        hot: read(LOW.hdmi0 + HOTPLUG),
        sch: read(LOW.hdmi0 + SCHEDULER),
        vid: read(LOW.hd + VID_CTL0),
        phy: read(LOW.phy0 + PHY_RESET),
        pwr: read(LOW.phy0 + PHY_POWER),
        sw: read(LOW.dvp_clk + DVP_SW_INIT),
        clk: read(LOW.dvp_clk + DVP_MISC),
        hvs: read(HVS),
        pv: read(PV2),
        h1: read(LOW.hdmi1 + HOTPLUG),
        hi: read(HI.hdmi0 + HOTPLUG),
        his: read(HI.hdmi0 + SCHEDULER),
        leg: read(LOW.hdmi0 + HOTPLUG_LEGACY),
        old: read(LOW.hdmi0 + HOTPLUG_2835),
        ctl: read(LOW.hd),
        cst: read(LOW.dvp + CLOCK_STOP),
        pv4: read(PV4),
        hc0: read(HVS_CH0),
        hc2: read(HVS_CH2),
        hsm: read(CM_HSMCTL),
        hsd: read(CM_HSMDIV),
        aon: read(AON),
    };
    USE_HI.store(prefer_hi(&snap), Ordering::Relaxed);
    snap
}

pub fn using_hi() -> bool {
    USE_HI.load(Ordering::Relaxed)
}

/// Why hot-plug bit 0 is or is not set. The picture can be up while
/// `HDMI_HOTPLUG` stays 0: the firmware scanout enables HVS and the pixel
/// valve without programming the KMS hot-plug bit, or the HSM clock that
/// clocks the HDMI register file is off.
pub fn hpd_line(snap: &Snap) -> &'static str {
    if connected(snap.hot) {
        return "hdmi hpd connected";
    }
    if connected(snap.hi) {
        return "hdmi hpd on 35bit";
    }
    if connected(snap.leg) || connected(snap.old) {
        return "hdmi hpd at legacy";
    }
    if snap.hot == MISS {
        return "hdmi hpd fault";
    }
    if pipe_live(snap) && snap.hsm != MISS && snap.hsm & CM_ENABLE == 0 {
        return "hdmi hpd 0 hsm off";
    }
    if pipe_live(snap) {
        return "hdmi hpd 0 pipe live";
    }
    "hdmi hpd 0"
}

/// Put both HDMI controllers in the state Linux leaves before a restart.
/// The PHY stays in reset. The 108 MHz clocks stay on. Call `snapshot`
/// first: this writes.
pub fn quiesce() -> Ports {
    let block = active();
    enable_hsm();
    ungate(&block);
    video_off(&block);
    phy_hold(&block);
    pulse_dvp(&block);
    phy_hold(&block);
    ungate(&block);
    sample(&block, 0, 0)
}

/// `PM_RSTS` as it stands before this kernel writes the reboot pattern.
/// The watchdog reason bits are set by the reset itself, not by this read.
pub fn pm_rsts() -> u32 {
    read(0xFE10_0020)
}

/// Same reset, then release the PHY and try DDC. Only for a boot that
/// already has no firmware size. Call `snapshot` first.
pub fn boot_recover() -> Ports {
    let block = active();
    enable_hsm();
    ungate(&block);
    video_off(&block);
    phy_hold(&block);
    pulse_dvp(&block);
    phy_release(&block);
    ungate(&block);
    wait_ms(20);
    let mut ports = sample(&block, 0, 0);
    if let Some((w, h)) = read_edid(&block) {
        ports.edid_w = w;
        ports.edid_h = h;
    }
    ports
}

fn prefer_hi(snap: &Snap) -> bool {
    connected(snap.hi) && !connected(snap.hot)
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

fn active() -> Block {
    if USE_HI.load(Ordering::Relaxed) {
        HI
    } else {
        LOW
    }
}

fn sample(block: &Block, edid_w: u32, edid_h: u32) -> Ports {
    Ports {
        h0: read(block.hdmi0 + HOTPLUG),
        h1: read(block.hdmi1 + HOTPLUG),
        legacy0: read(block.hdmi0 + HOTPLUG_LEGACY),
        phy0: read(block.phy0 + PHY_RESET),
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

fn ungate(block: &Block) {
    let pa = block.dvp_clk + DVP_MISC;
    let Some(raw) = read_ok(pa) else {
        return;
    };
    let _ = write(pa, raw & !DVP_CLK_GATE);
}

fn video_off(block: &Block) {
    for off in [VID_CTL0, VID_CTL1] {
        let pa = block.hd + off;
        let Some(raw) = read_ok(pa) else {
            continue;
        };
        let _ = write(pa, raw & !VID_ENABLE);
    }
}

fn phy_hold(block: &Block) {
    for base in [block.phy0, block.phy1] {
        if read_ok(base + PHY_RESET).is_none() {
            continue;
        }
        let _ = write(base + PHY_RESET, PHY_RESET_HOLD);
        let _ = write(base + PHY_POWER, PHY_POWERDOWN);
    }
}

fn phy_release(block: &Block) {
    for base in [block.phy0, block.phy1] {
        if read_ok(base + PHY_RESET).is_none() {
            continue;
        }
        let _ = write(base + PHY_RESET, 0);
        let _ = write(base + PHY_POWER, PHY_RNG_OFF);
    }
}

fn pulse_dvp(block: &Block) {
    let pa = block.dvp_clk + DVP_SW_INIT;
    let Some(raw) = read_ok(pa) else {
        return;
    };
    let _ = write(pa, raw | DVP_HDMI_RST);
    wait_ms(1);
    let _ = write(pa, raw & !DVP_HDMI_RST);
}

fn read_edid(block: &Block) -> Option<(u32, u32)> {
    if let Some(size) = edid_port(block.bsc0, block.auto0) {
        return Some(size);
    }
    edid_port(block.bsc1, block.auto1)
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
