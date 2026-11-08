//! BCM2712 SDHCI (`sdio1`) for the Pi 5 SD card.
//!
//! The host block is standard SDHCI at physical `0x1000fff000`. The cfg
//! block is at `0x1000fff400`. Linux describes this as `brcm,bcm2712-sdhci`.
//! The interrupt is SPI 273 (INTID 305). `clk_emmc2` in the device tree is
//! 200 MHz. The board file says SDIO1 drives the SD card.
//!
//! The first register read uses `read_u32_catching`, so a missing window
//! is a failed probe instead of a fatal external abort. Card power is
//! `gio_aon` GPIO 4 and card detect is `gio_aon` GPIO 5. This driver does
//! not program those pins or the pin mux. It assumes the firmware left the
//! slot powered, which is true only if the firmware itself used the card.
//!
//! UHS modes, the 1.8 V switch, and the command-queue engine are not
//! implemented. This driver has not been run on hardware.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use spin::Mutex;

const HOST_PHYS: u64 = 0x1_000_fff000;
const CFG_PHYS: u64 = 0x1_000_fff400;

const CFG_PIN_SEL: u64 = 0x44;
const PIN_SEL_MASK: u32 = 0x3;
const PIN_SEL_SD: u32 = 1 << 1;

/// Present-state bit 16 is card inserted on SDHCI.
const PRESENT: usize = 0x24;
const CARD_INSERTED: u32 = 1 << 16;
const CAPABILITIES: usize = 0x40;
const ARG: usize = 0x08;
const BLK: usize = 0x04;
const DATA: usize = 0x20;
const RESP0: usize = 0x10;
const INT_STATUS: usize = 0x30;
const INT_ENABLE: usize = 0x34;
const INT_SIGNAL: usize = 0x38;
const HOST_VERSION: usize = 0xfc;

const XFER: usize = 0x0c;
const COMMAND: usize = 0x0e;
const CLOCK: usize = 0x2c;
const TIMEOUT: usize = 0x2e;
const SW_RESET: usize = 0x2f;
const POWER: usize = 0x29;
const HOST_CONTROL: usize = 0x28;

const INT_CMD_DONE: u32 = 1 << 0;
const INT_DATA_DONE: u32 = 1 << 1;
const INT_WRITE_RDY: u32 = 1 << 4;
const INT_READ_RDY: u32 = 1 << 5;
const INT_ERR: u32 = 1 << 15;
const INT_CMD_TIMEOUT: u32 = 1 << 16;

const CLK_INT_EN: u16 = 1 << 0;
const CLK_STABLE: u16 = 1 << 1;
const CLK_CARD_EN: u16 = 1 << 2;

const RESET_ALL: u8 = 1 << 0;
const RESET_CMD: u8 = 1 << 1;
const RESET_DATA: u8 = 1 << 2;

const RESP_NONE: u16 = 0;
const RESP_136: u16 = 1;
const RESP_48: u16 = 2;
const RESP_48_BUSY: u16 = 3;

const SD_IRQ: u32 = 32 + 273;

const FALLBACK_BASE_HZ: u32 = 200_000_000;

struct Card {
    rca: u32,
    high_capacity: bool,
    blocks: u32,
}

static CARD: Mutex<Option<Card>> = Mutex::new(None);
static ARMED: AtomicBool = AtomicBool::new(false);

pub struct DiskDev;

pub fn probe() -> bool {
    let version = match catching(HOST_PHYS + HOST_VERSION as u64) {
        Some(value) => value,
        None => {
            log::warn!("SDHCI did not respond");
            return false;
        }
    };
    if version == 0 || version == 0xffff_ffff {
        log::warn!("SDHCI controller is not present ({version:#x})");
        return false;
    }
    log::info!("SDHCI version {version:#x}");
    program_cfg();
    let present = read32(PRESENT) & CARD_INSERTED != 0;
    if !present {
        log::info!("SDHCI card-detect is clear; trying identification anyway");
    }
    match identify() {
        Ok(card) => {
            log::info!("SD card rca {:#x}, {} blocks", card.rca, card.blocks);
            *CARD.lock() = Some(card);
            true
        }
        Err(err) => {
            log::warn!("SD card init failed: {err}");
            false
        }
    }
}

fn program_cfg() {
    // Select the SD pin mode. The cfg block is the second `reg` of sdio1.
    // A fault here is non-fatal: the host may still answer commands.
    let Some(mut pin) = catching(CFG_PHYS + CFG_PIN_SEL) else {
        log::warn!("SDHCI cfg block did not respond");
        return;
    };
    pin &= !PIN_SEL_MASK;
    pin |= PIN_SEL_SD;
    write32_phys(CFG_PHYS + CFG_PIN_SEL, pin);
}

impl BlockDevice for DiskDev {
    type Error = &'static str;

    fn read(
        &self,
        blocks: &mut [Block],
        start_block_idx: BlockIdx,
        _reason: &str,
    ) -> Result<(), Self::Error> {
        for (index, block) in blocks.iter_mut().enumerate() {
            let lba = start_block_idx.0 as u64 + index as u64;
            read_sector(lba, &mut block.contents)?;
        }
        Ok(())
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        for (index, block) in blocks.iter().enumerate() {
            let lba = start_block_idx.0 as u64 + index as u64;
            write_sector(lba, &block.contents)?;
        }
        Ok(())
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let card = CARD.lock();
        let card = card.as_ref().ok_or("no card")?;
        Ok(BlockCount(card.blocks.max(1)))
    }
}

fn identify() -> Result<Card, &'static str> {
    reset_host()?;
    set_clock(400_000)?;
    command(0, 0, RESP_NONE, false, false)?;
    let _if_cond = command(8, 0x1aa, RESP_48, false, false);
    let mut ocr = 0u32;
    let mut high_capacity = false;
    for _ in 0..200 {
        command(55, 0, RESP_48, false, false)?;
        ocr = command(41, 0x50ff_8000, RESP_48, false, false)?;
        if ocr & (1 << 31) != 0 {
            high_capacity = ocr & (1 << 30) != 0;
            break;
        }
        spin(50_000);
    }
    if ocr & (1 << 31) == 0 {
        return Err("card did not leave idle");
    }
    command(2, 0, RESP_136, false, false)?;
    let rca_resp = command(3, 0, RESP_48, false, false)?;
    let rca = rca_resp & 0xffff_0000;
    let blocks = read_capacity(rca).unwrap_or(0);
    command(7, rca, RESP_48_BUSY, false, false)?;
    // 4-bit bus. ACMD6 failing leaves the card in 1-bit mode.
    if command(55, rca, RESP_48, false, false).is_ok() {
        let _ = command(6, 2, RESP_48, false, false);
    }
    command(16, 512, RESP_48, false, false)?;
    write8(HOST_CONTROL, read8(HOST_CONTROL) | (1 << 1));
    set_clock(25_000_000)?;
    Ok(Card {
        rca,
        high_capacity,
        blocks,
    })
}

fn read_capacity(rca: u32) -> Option<u32> {
    command(9, rca, RESP_136, false, false).ok()?;
    let words = [
        read32(RESP0),
        read32(RESP0 + 4),
        read32(RESP0 + 8),
        read32(RESP0 + 12),
    ];
    let mut csd = [0u8; 16];
    csd[0] = (words[3] >> 16) as u8;
    csd[1] = (words[3] >> 8) as u8;
    csd[2] = words[3] as u8;
    let w2 = words[2].to_be_bytes();
    let w1 = words[1].to_be_bytes();
    let w0 = words[0].to_be_bytes();
    csd[3] = w2[0];
    csd[4] = w2[1];
    csd[5] = w2[2];
    csd[6] = w2[3];
    csd[7] = w1[0];
    csd[8] = w1[1];
    csd[9] = w1[2];
    csd[10] = w1[3];
    csd[11] = w0[0];
    csd[12] = w0[1];
    csd[13] = w0[2];
    csd[14] = w0[3];
    let structure = csd[0] >> 6;
    if structure == 1 {
        let c_size = ((csd[7] as u32) << 16) | ((csd[8] as u32) << 8) | csd[9] as u32;
        Some((c_size + 1).saturating_mul(1024))
    } else if structure == 0 {
        let read_bl_len = (csd[5] & 0x0f) as u32;
        let c_size = (((csd[6] & 0x03) as u32) << 10)
            | ((csd[7] as u32) << 2)
            | ((csd[8] as u32) >> 6);
        let c_size_mult = (((csd[9] & 0x03) as u32) << 1) | ((csd[10] as u32) >> 7);
        let block_len = 1u32 << read_bl_len;
        let mult = 1u32 << (c_size_mult + 2);
        let bytes = (c_size + 1).saturating_mul(mult).saturating_mul(block_len);
        Some(bytes / 512)
    } else {
        None
    }
}

fn read_sector(lba: u64, out: &mut [u8; 512]) -> Result<(), &'static str> {
    let arg = sector_arg(lba)?;
    write32(BLK, (1 << 16) | 512);
    command(17, arg, RESP_48, true, true)?;
    wait_interrupt(INT_READ_RDY)?;
    for word_index in 0..128 {
        let word = read32(DATA);
        let offset = word_index * 4;
        out[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    wait_interrupt(INT_DATA_DONE)?;
    Ok(())
}

fn write_sector(lba: u64, data: &[u8; 512]) -> Result<(), &'static str> {
    let arg = sector_arg(lba)?;
    write32(BLK, (1 << 16) | 512);
    command(24, arg, RESP_48, true, false)?;
    wait_interrupt(INT_WRITE_RDY)?;
    for word_index in 0..128 {
        let offset = word_index * 4;
        let word = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        write32(DATA, word);
    }
    wait_interrupt(INT_DATA_DONE)?;
    Ok(())
}

fn sector_arg(lba: u64) -> Result<u32, &'static str> {
    let card = CARD.lock();
    let card = card.as_ref().ok_or("no card")?;
    if card.high_capacity {
        Ok(lba as u32)
    } else {
        Ok((lba as u32).saturating_mul(512))
    }
}

fn reset_host() -> Result<(), &'static str> {
    write8(SW_RESET, RESET_ALL);
    wait_reset_clear(RESET_ALL)?;
    write8(POWER, 0x0f);
    write8(TIMEOUT, 0x0e);
    write32(INT_ENABLE, 0xffff_ffff);
    write32(INT_SIGNAL, 0xffff_ffff);
    write32(INT_STATUS, 0xffff_ffff);
    if !ARMED.swap(true, Ordering::Relaxed) {
        crate::gic::arm_spi(SD_IRQ, 0x80, crate::gic::online_cpu_mask());
        crate::gic::disable_id(SD_IRQ);
    }
    Ok(())
}

fn set_clock(target_hz: u32) -> Result<(), &'static str> {
    let mut clk = read16(CLOCK);
    clk &= !CLK_CARD_EN;
    write16(CLOCK, clk);
    let div = clock_div(base_hz(), target_hz);
    clk = div | CLK_INT_EN;
    write16(CLOCK, clk);
    wait_clock_stable()?;
    write16(CLOCK, read16(CLOCK) | CLK_CARD_EN);
    Ok(())
}

fn base_hz() -> u32 {
    let mhz = (read32(CAPABILITIES) >> 8) & 0xff;
    if mhz == 0 {
        FALLBACK_BASE_HZ
    } else {
        mhz.saturating_mul(1_000_000)
    }
}

/// SDHCI 10-bit divisor. Frequency is `base / (2 * n)` for n > 0.
fn clock_div(base_hz: u32, target_hz: u32) -> u16 {
    if target_hz == 0 || base_hz <= target_hz {
        return 0;
    }
    let step = target_hz.saturating_mul(2).max(1);
    let mut n = base_hz.saturating_add(step - 1) / step;
    if n == 0 {
        n = 1;
    }
    if n > 0x3ff {
        n = 0x3ff;
    }
    let low = ((n & 0xff) as u16) << 8;
    let high = (((n & 0x300) >> 2) as u16) << 6;
    low | high
}

fn command(
    index: u32,
    arg: u32,
    resp: u16,
    data: bool,
    read_data: bool,
) -> Result<u32, &'static str> {
    write8(SW_RESET, RESET_CMD | RESET_DATA);
    wait_reset_clear(RESET_CMD | RESET_DATA)?;
    write32(INT_STATUS, 0xffff_ffff);
    write32(ARG, arg);
    let mut mode = 0u16;
    if data {
        mode |= 1 << 1;
        if read_data {
            mode |= 1 << 4;
        }
    }
    write16(XFER, mode);
    let mut cmd = ((index as u16) & 0x3f) << 8;
    cmd |= resp & 3;
    if resp != RESP_NONE {
        cmd |= 1 << 3;
        if resp != RESP_136 {
            cmd |= 1 << 4;
        }
    }
    if data {
        cmd |= 1 << 5;
    }
    write16(COMMAND, cmd);
    let done = wait_interrupt(INT_CMD_DONE)?;
    if done & INT_ERR != 0 || done & INT_CMD_TIMEOUT != 0 {
        return Err("sdhci command error");
    }
    Ok(read32(RESP0))
}

fn wait_reset_clear(mask: u8) -> Result<(), &'static str> {
    let start = crate::timer::get_ticks();
    let budget = half_second();
    let mut spins = 0u32;
    loop {
        if read8(SW_RESET) & mask == 0 {
            return Ok(());
        }
        if timed_out(start, budget, &mut spins) {
            return Err("sdhci reset timeout");
        }
        wfe();
    }
}

fn wait_clock_stable() -> Result<(), &'static str> {
    let start = crate::timer::get_ticks();
    let budget = half_second();
    let mut spins = 0u32;
    loop {
        if read16(CLOCK) & CLK_STABLE != 0 {
            return Ok(());
        }
        if timed_out(start, budget, &mut spins) {
            return Err("sdhci clock timeout");
        }
        wfe();
    }
}

fn wait_interrupt(bit: u32) -> Result<u32, &'static str> {
    crate::gic::enable_id(SD_IRQ);
    let start = crate::timer::get_ticks();
    let budget = half_second();
    let mut spins = 0u32;
    loop {
        let status = read32(INT_STATUS);
        let interesting = bit | INT_ERR | INT_CMD_TIMEOUT;
        if status & interesting != 0 {
            write32(INT_STATUS, status & interesting);
            return Ok(status);
        }
        if timed_out(start, budget, &mut spins) {
            return Err("sdhci timeout");
        }
        wfe();
    }
}

fn half_second() -> u64 {
    cntfrq() / 2
}

fn timed_out(start: u64, budget: u64, spins: &mut u32) -> bool {
    let expired = if budget == 0 {
        *spins > 50_000_000
    } else {
        crate::timer::get_ticks().wrapping_sub(start) >= budget
    };
    *spins = spins.wrapping_add(1);
    expired
}

fn cntfrq() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {freq}, cntfrq_el0", freq = out(reg) freq);
    }
    freq
}

fn wfe() {
    unsafe {
        core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
    }
}

fn host() -> u64 {
    crate::addr::phys_to_virt(HOST_PHYS)
}

fn catching(phys: u64) -> Option<u32> {
    crate::exception::read_u32_catching(crate::addr::phys_to_virt(phys) as *const u32)
}

fn read32(offset: usize) -> u32 {
    unsafe { read_volatile((host() + offset as u64) as *const u32) }
}

fn write32(offset: usize, value: u32) {
    unsafe { write_volatile((host() + offset as u64) as *mut u32, value) }
}

fn write32_phys(phys: u64, value: u32) {
    unsafe { write_volatile(crate::addr::phys_to_virt(phys) as *mut u32, value) }
}

fn read16(offset: usize) -> u16 {
    unsafe { read_volatile((host() + offset as u64) as *const u16) }
}

fn write16(offset: usize, value: u16) {
    unsafe { write_volatile((host() + offset as u64) as *mut u16, value) }
}

fn read8(offset: usize) -> u8 {
    unsafe { read_volatile((host() + offset as u64) as *const u8) }
}

fn write8(offset: usize, value: u8) {
    unsafe { write_volatile((host() + offset as u64) as *mut u8, value) }
}

fn spin(count: u32) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}
