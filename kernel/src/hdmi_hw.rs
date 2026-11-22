//! Pi 4 HDMI controller, PHY, and DDC.
//!
//! A watchdog reset does not reset this block. `PM_RSTC` full reset
//! (Linux `WRCFG` `0x20`, halt partition bits `0x555` cleared) reboots
//! the ARM and the VPU, and the hardware itself sets the watchdog reason
//! bits in `PM_RSTS` as that happens. Nothing in that register makes the
//! next `start4.elf` treat the boot as a power-on. The DVP clock gate at
//! `0xFEF00000` is also outside the HDMI power domain: leaving the 108 MHz
//! clock gated stops the firmware from programming the PHY, which is the
//! failure Linux hit when the DVP driver came up under the firmware display.
//!
//! Shutdown follows `vc5_hdmi_phy_disable` and `vc5_hdmi_reset`: hold the
//! PHY in reset, drop video enable, pulse the DVP reset line, and leave
//! the 108 MHz clocks running. Boot recovery, used only after the firmware
//! already reported no size, does that and then lets the PHY out of reset
//! so a direct DDC read can run. Register access is skipped when a read
//! faults. Virt and Pi 5 do not build this module.

use crate::exception::read_u32_catching;
use crate::timer::{frequency, get_ticks};

const MISS: u32 = 0xffff_ffff;

const DVP_BASE: u64 = 0xFEF0_0000;
const DVP_SW_INIT: u64 = DVP_BASE + 0x04;
const DVP_MISC: u64 = DVP_BASE + 0x08;
/// Bits 3 and 4 gate the HDMI 108 MHz clocks. Set means the clock is off.
const DVP_CLK_GATE: u32 = (1 << 3) | (1 << 4);
/// Reset lines 0 and 1 are HDMI0 and HDMI1.
const DVP_HDMI_RST: u32 = (1 << 0) | (1 << 1);

const HDMI0: u64 = 0xFEF0_0700;
const HDMI1: u64 = 0xFEF0_5700;
/// Linux `vc5` `HDMI_HOTPLUG`. Bit 0 is connected.
const HOTPLUG: u64 = 0x1a8;
/// Older offset this kernel used to sample. Logged so a board can show
/// which one moves when the cable is plugged in.
const HOTPLUG_LEGACY: u64 = 0x1c;

const PHY0: u64 = 0xFEF0_0F00;
const PHY1: u64 = 0xFEF0_5F00;
const PHY_RESET: u64 = 0x00;
const PHY_POWER: u64 = 0x04;
/// Lane resets. Linux `vc5_hdmi_reset_phy` writes this and leaves it.
const PHY_RESET_HOLD: u32 = 0x0f;
/// Linux `HDMI_TX_PHY_POWERDOWN_CTL` bit 10, held across `phy_disable`.
const PHY_POWERDOWN: u32 = 1 << 10;
/// Randomizer power-down. Linux clears the lane power-down and leaves this.
const PHY_RNG_OFF: u32 = 1 << 4;

const HD_BASE: u64 = 0xFEF2_0000;
const VID_CTL0: u64 = HD_BASE + 0x44;
const VID_CTL1: u64 = HD_BASE + 0x48;
const VID_ENABLE: u32 = 1 << 31;

const BSC0: u64 = 0xFEF0_4500;
const BSC1: u64 = 0xFEF0_9500;
const AUTO0: u64 = 0xFEF0_0B00;
const AUTO1: u64 = 0xFEF0_5B00;
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

/// Put both HDMI controllers in the state Linux leaves before a restart.
/// The PHY stays in reset. The 108 MHz clocks stay on.
pub fn quiesce() -> Ports {
    ungate();
    video_off();
    phy_hold();
    pulse_dvp();
    phy_hold();
    ungate();
    sample(0, 0)
}

/// `PM_RSTS` as it stands before this kernel writes the reboot pattern.
/// The watchdog reason bits are set by the reset itself, not by this read.
pub fn pm_rsts() -> u32 {
    read(0xFE10_0020)
}

/// Same reset, then release the PHY and try DDC. Only for a boot that
/// already has no firmware size.

pub fn boot_recover() -> Ports {
    ungate();
    video_off();
    phy_hold();
    pulse_dvp();
    phy_release();
    ungate();
    wait_ms(20);
    let mut ports = sample(0, 0);
    if let Some((w, h)) = read_edid() {
        ports.edid_w = w;
        ports.edid_h = h;
    }
    ports
}

fn sample(edid_w: u32, edid_h: u32) -> Ports {
    Ports {
        h0: read(HDMI0 + HOTPLUG),
        h1: read(HDMI1 + HOTPLUG),
        legacy0: read(HDMI0 + HOTPLUG_LEGACY),
        phy0: read(PHY0 + PHY_RESET),
        dvp: read(DVP_SW_INIT),
        clk: read(DVP_MISC),
        edid_w,
        edid_h,
    }
}

fn ungate() {
    let Some(raw) = read_ok(DVP_MISC) else {
        return;
    };
    let _ = write(DVP_MISC, raw & !DVP_CLK_GATE);
}

fn video_off() {
    for pa in [VID_CTL0, VID_CTL1] {
        let Some(raw) = read_ok(pa) else {
            continue;
        };
        let _ = write(pa, raw & !VID_ENABLE);
    }
}

fn phy_hold() {
    for base in [PHY0, PHY1] {
        if read_ok(base + PHY_RESET).is_none() {
            continue;
        }
        let _ = write(base + PHY_RESET, PHY_RESET_HOLD);
        let _ = write(base + PHY_POWER, PHY_POWERDOWN);
    }
}

fn phy_release() {
    for base in [PHY0, PHY1] {
        if read_ok(base + PHY_RESET).is_none() {
            continue;
        }
        let _ = write(base + PHY_RESET, 0);
        let _ = write(base + PHY_POWER, PHY_RNG_OFF);
    }
}

fn pulse_dvp() {
    let Some(raw) = read_ok(DVP_SW_INIT) else {
        return;
    };
    let _ = write(DVP_SW_INIT, raw | DVP_HDMI_RST);
    wait_ms(1);
    let _ = write(DVP_SW_INIT, raw & !DVP_HDMI_RST);
}

fn read_edid() -> Option<(u32, u32)> {
    if let Some(size) = edid_port(BSC0, AUTO0) {
        return Some(size);
    }
    edid_port(BSC1, AUTO1)
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
    if !bsc_cmd(bsc, EDID_ADDR_WR, BSC_RESTART | BSC_NOSTOP, false, &[offset], 1, &mut [], start, budget) {
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
        if !bsc_cmd(bsc, EDID_ADDR_RD, flags, true, &[], n as u32, &mut words, start, budget) {
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
