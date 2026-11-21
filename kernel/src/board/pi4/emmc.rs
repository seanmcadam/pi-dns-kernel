//! BCM2711 EMMC2 SD host (Pi 4, MMIO `0xFE340000`).
//!
//! EMMC2 is a standard SDHCI. The firmware may hand the card over already
//! clocked, and sometimes already at 1.8 V. The card-detect bit is not
//! wired (`broken-cd` in the Pi 4 device tree), so a clear bit is not
//! treated as an empty slot. Bring-up is a host reset, 3.3 V power, a
//! 400 kHz identification clock, then CMD0 / CMD8 / ACMD41. The clock
//! source is mailbox clock id 12 (`EMMC2`). A failed first pass power-cycles
//! the SD card through the firmware mailbox and tries once more, which
//! drops a card the firmware left in a high-speed 1.8 V mode back to 3.3 V.
//!
//! The legacy Arasan block at `0xFE300000` is only the fallback. QEMU
//! raspi4b attaches `-drive if=sd` there. On a real Pi 4 that block is
//! the WiFi SDIO, so it is tried only after EMMC2 fails.
//!
//! Writes are CMD24, or CMD25 when the FAT layer hands over more than
//! one contiguous sector. This file never writes a sector on its own
//! during probe.
//!
//! The GIC-400 line is SPI 126 (INTID 158). Transfers poll the status
//! register with IRQs unmasked so the timer still runs. The data line is
//! not what wakes the waiter: a `wfe` with DAIF.I set sleeps through the
//! completion and turns a 25 MHz bus into a few kilobytes per second.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicU64, Ordering};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use spin::Mutex;

use crate::disklog::Buf;

/// Real Pi 4 SD slot.
const EMMC2_PHYS: u64 = 0xFE34_0000;
/// Legacy Arasan. QEMU raspi4b attaches the SD card here.
const EMMC1_PHYS: u64 = 0xFE30_0000;
/// Property channel. Same block the HDMI and tryboot paths use.
const MBOX_BASE: u64 = 0xFE00_B880;
/// `RPI_FIRMWARE_GET_CLOCK_RATE`. Clock id 12 is EMMC2.
const TAG_GET_CLOCK: u32 = 0x0003_0002;
/// `RPI_FIRMWARE_SET_CLOCK_RATE`.
const TAG_SET_CLOCK: u32 = 0x0003_8002;
/// `RPI_FIRMWARE_SET_POWER_STATE`. Device 0 is the SD card.
const TAG_SET_POWER: u32 = 0x0002_8001;
const CLOCK_EMMC2: u32 = 12;
const SD_POWER_ID: u32 = 0;

static HOST_PHYS: AtomicU64 = AtomicU64::new(EMMC2_PHYS);

const PRESENT: usize = 0x24;
const CARD_INSERTED: u32 = 1 << 16;
const CAPABILITIES: usize = 0x40;

/// Same GIC line the board handler matches. EMMC and EMMC2 share it.
const EMMC_IRQ: u32 = 32 + 126;

const BLKSIZECNT: usize = 0x04;
const ARG1: usize = 0x08;
const CMDTM: usize = 0x0c;
const RESP0: usize = 0x10;
const DATA: usize = 0x20;
const CONTROL0: usize = 0x28;
const CONTROL1: usize = 0x2c;
const INTERRUPT: usize = 0x30;
const IRPT_MASK: usize = 0x34;
const IRPT_EN: usize = 0x38;
const SLOTISR_VER: usize = 0xfc;

const POWER: usize = 0x29;
const HOST_CONTROL: usize = 0x28;
const CLOCK: usize = 0x2c;
const TIMEOUT: usize = 0x2e;
const SW_RESET: usize = 0x2f;
const XFER: usize = 0x0c;
const COMMAND: usize = 0x0e;

const INT_CMD_DONE: u32 = 1 << 0;
const INT_DATA_DONE: u32 = 1 << 1;
const INT_WRITE_RDY: u32 = 1 << 4;
const INT_READ_RDY: u32 = 1 << 5;
const INT_ERR: u32 = 1 << 15;
const INT_CMD_TIMEOUT: u32 = 1 << 16;

const CTRL1_CLK_INTLEN: u32 = 1 << 0;
const CTRL1_CLK_STABLE: u32 = 1 << 1;
const CTRL1_CLK_EN: u32 = 1 << 2;
const CTRL1_SRST_HC: u32 = 1 << 24;
const CTRL1_SRST_CMD: u32 = 1 << 25;
const CTRL1_SRST_DATA: u32 = 1 << 26;

const CLK_INT_EN: u16 = 1 << 0;
const CLK_STABLE: u16 = 1 << 1;
const CLK_CARD_EN: u16 = 1 << 2;
const RESET_ALL: u8 = 1 << 0;
const RESET_CMD: u8 = 1 << 1;
const RESET_DATA: u8 = 1 << 2;
/// Power on, 3.3 V. SDHCI POWER_CONTROL.
const POWER_3V3: u8 = 0x0f;

const RESP_NONE: u32 = 0;
const RESP_136: u32 = 1;
const RESP_48: u32 = 2;
const RESP_48_BUSY: u32 = 3;

const FALLBACK_HZ: u32 = 100_000_000;
const ID_HZ: u32 = 400_000;
const DATA_HZ: u32 = 25_000_000;

struct Card {
    high_capacity: bool,
    blocks: u32,
}

static CARD: Mutex<Option<Card>> = Mutex::new(None);

pub struct DiskDev;

pub fn probe() -> bool {
    *CARD.lock() = None;
    match bring_up_emmc2() {
        Ok(()) => return true,
        Err(err) => {
            let mut line = Buf::new();
            line.push("sd emmc2 failed ");
            line.push(err);
            line.emit();
        }
    }
    match bring_up_legacy() {
        Ok(()) => true,
        Err(err) => {
            crate::disklog::set_error("absent", err);
            let mut line = Buf::new();
            line.push("sd no card ");
            line.push(err);
            line.emit();
            false
        }
    }
}

fn bring_up_emmc2() -> Result<(), &'static str> {
    let version = match catching(EMMC2_PHYS + SLOTISR_VER as u64) {
        Some(value) => value,
        None => {
            crate::disklog::line("sd emmc2 bus fault");
            return Err("bus fault");
        }
    };
    if version == 0 || version == 0xffff_ffff {
        let mut line = Buf::new();
        line.push("sd emmc2 absent ver=");
        line.hex(version);
        line.emit();
        return Err("absent");
    }
    HOST_PHYS.store(EMMC2_PHYS, Ordering::Relaxed);
    let present = read(PRESENT);
    let mut line = Buf::new();
    line.push("sd emmc2 ver=");
    line.hex(version);
    line.push(" present=");
    line.hex(present);
    line.push(" cd=");
    line.push(if present & CARD_INSERTED != 0 {
        "1"
    } else {
        "0"
    });
    line.emit();
    // The detect bit is not a reason to stop. The Pi 4 slot does not
    // drive it reliably, and the firmware may leave it clear.
    let hz = ensure_clock();
    if let Err(err) = identify_sdhci(hz) {
        let mut line = Buf::new();
        line.push("sd emmc2 power cycle after ");
        line.push(err);
        line.emit();
        if !cycle_sd_power() {
            crate::disklog::line("sd emmc2 power refused");
        }
        let hz = ensure_clock();
        identify_sdhci(hz)?;
    }
    Ok(())
}

fn usable_clock(hz: u32) -> bool {
    (20_000_000..400_000_000).contains(&hz)
}

fn ensure_clock() -> u32 {
    let mailbox = mailbox_clock();
    let cap_mhz = (read(CAPABILITIES) >> 8) & 0xff;
    let (hz, source) = if usable_clock(mailbox) {
        (mailbox, "mbox")
    } else {
        let programmed = mailbox_set_clock(FALLBACK_HZ);
        if usable_clock(programmed) {
            (programmed, "set")
        } else if cap_mhz > 0 {
            (cap_mhz.saturating_mul(1_000_000), "cap")
        } else {
            (FALLBACK_HZ, "fallback")
        }
    };
    let mut line = Buf::new();
    line.push("sd emmc2 clock ");
    line.push(source);
    line.push(" ");
    line.dec(hz);
    line.emit();
    hz
}

fn identify_sdhci(base_hz: u32) -> Result<(), &'static str> {
    sd_reset()?;
    crate::disklog::line("sd emmc2 reset ok");
    write8(POWER, POWER_3V3);
    write8(TIMEOUT, 0x0e);
    sd_set_clock(base_hz, ID_HZ)?;
    delay_ms(2);
    log_cmd("cmd0", &sd_command(0, 0, RESP_NONE, false, false, 200))?;
    let cmd8 = sd_command(8, 0x1aa, RESP_48, false, false, 200);
    let _ = log_cmd("cmd8", &cmd8);
    let cmd8_ok = matches!(cmd8, Ok(value) if value & 0xff == 0xaa);
    if cmd8.is_err() && !cmd8_ok {
        // A timeout here is "no electrical card" or a 1.8 V bus the
        // 3.3 V host cannot see. The caller power-cycles and retries.
        return cmd8.map(|_| ());
    }
    let ocr_arg = 0x00ff_8000 | if cmd8_ok { 1 << 30 } else { 0 };
    let mut ocr = 0u32;
    let mut high_capacity = false;
    for _ in 0..200 {
        sd_command(55, 0, RESP_48, false, false, 200)?;
        // ACMD41 is R3: no CRC and no command index. CRC checking on
        // that response is a false error.
        ocr = sd_command(41, ocr_arg, RESP_48, false, false, 200)?;
        if ocr & (1 << 31) != 0 {
            high_capacity = ocr & (1 << 30) != 0;
            break;
        }
        delay_ms(5);
    }
    if ocr & (1 << 31) == 0 {
        let mut line = Buf::new();
        line.push("sd emmc2 acmd41 idle ");
        line.hex(ocr);
        line.emit();
        return Err("card did not leave idle");
    }
    let mut line = Buf::new();
    line.push("sd emmc2 acmd41 ");
    line.hex(ocr);
    line.push(if high_capacity { " hc" } else { " sc" });
    line.emit();
    sd_command(2, 0, RESP_136, false, false, 200)?;
    let rca_resp = sd_command(3, 0, RESP_48, false, false, 200)?;
    let rca = rca_resp & 0xffff_0000;
    let blocks = read_capacity(rca, true).unwrap_or(0);
    let mut line = Buf::new();
    line.push("sd emmc2 rca=");
    line.hex(rca);
    line.push(" blocks=");
    line.dec(blocks);
    line.emit();
    sd_command(7, rca, RESP_48_BUSY, false, false, 200)?;
    // 4-bit bus. A failure leaves the card in 1-bit mode, which still
    // reads and writes.
    let wide = sd_command(55, rca, RESP_48, false, false, 200).is_ok()
        && sd_command(6, 2, RESP_48, false, false, 200).is_ok();
    if wide {
        write8(HOST_CONTROL, read8(HOST_CONTROL) | (1 << 1));
        crate::disklog::line("sd emmc2 4bit");
    } else {
        crate::disklog::line("sd emmc2 1bit");
    }
    sd_command(16, 512, RESP_48, false, false, 200)?;
    sd_set_clock(base_hz, DATA_HZ)?;
    log_data_clock(base_hz);
    *CARD.lock() = Some(Card {
        high_capacity,
        blocks,
    });
    crate::disklog::set_card("emmc2", blocks);
    crate::gic::arm_spi(EMMC_IRQ, 0x80, crate::gic::online_cpu_mask());
    crate::gic::disable_id(EMMC_IRQ);
    Ok(())
}

fn log_cmd(name: &str, result: &Result<u32, &'static str>) -> Result<(), &'static str> {
    let mut line = Buf::new();
    line.push("sd emmc2 ");
    line.push(name);
    match result {
        Ok(value) => {
            line.push(" ");
            line.hex(*value);
            line.emit();
            Ok(())
        }
        Err(err) => {
            line.push(" ");
            line.push(err);
            line.emit();
            Err(*err)
        }
    }
}

fn bring_up_legacy() -> Result<(), &'static str> {
    HOST_PHYS.store(EMMC1_PHYS, Ordering::Relaxed);
    let version = match catching(EMMC1_PHYS + SLOTISR_VER as u64) {
        Some(value) => value,
        None => {
            crate::disklog::line("sd emmc1 bus fault");
            return Err("bus fault");
        }
    };
    if version == 0 || version == 0xffff_ffff {
        crate::disklog::line("sd emmc1 absent");
        return Err("absent");
    }
    let present = read(PRESENT);
    let mut line = Buf::new();
    line.push("sd emmc1 ver=");
    line.hex(version);
    line.push(" present=");
    line.hex(present);
    line.push(" cd=");
    line.push(if present & CARD_INSERTED != 0 {
        "1"
    } else {
        "0"
    });
    line.emit();
    // QEMU's legacy controller does drive this bit. An empty WiFi SDIO
    // slot on the Pi 4 must not spend the identification budget.
    if present & CARD_INSERTED == 0 {
        return Err("no card");
    }
    identify_legacy()?;
    Ok(())
}

fn identify_legacy() -> Result<(), &'static str> {
    legacy_reset()?;
    legacy_clock(0x80)?;
    legacy_command(0, 0, RESP_NONE, false, false)?;
    let _if_cond = legacy_command(8, 0x1aa, RESP_48, false, false);
    let mut ocr = 0u32;
    let mut high_capacity = false;
    for _ in 0..200 {
        legacy_command(55, 0, RESP_48, false, false)?;
        ocr = legacy_command(41, 0x50ff_8000, RESP_48, false, false)?;
        if ocr & (1 << 31) != 0 {
            high_capacity = ocr & (1 << 30) != 0;
            break;
        }
        spin(50_000);
    }
    if ocr & (1 << 31) == 0 {
        return Err("card did not leave idle");
    }
    legacy_command(2, 0, RESP_136, false, false)?;
    let rca_resp = legacy_command(3, 0, RESP_48, false, false)?;
    let rca = rca_resp & 0xffff_0000;
    let blocks = read_capacity(rca, false).unwrap_or(0);
    legacy_command(7, rca, RESP_48_BUSY, false, false)?;
    legacy_command(16, 512, RESP_48, false, false)?;
    legacy_clock(0x02)?;
    let mut line = Buf::new();
    line.push("sd emmc1 rca=");
    line.hex(rca);
    line.push(" blocks=");
    line.dec(blocks);
    line.emit();
    *CARD.lock() = Some(Card {
        high_capacity,
        blocks,
    });
    crate::disklog::set_card("emmc1", blocks);
    Ok(())
}

impl BlockDevice for DiskDev {
    type Error = &'static str;

    fn read(
        &self,
        blocks: &mut [Block],
        start_block_idx: BlockIdx,
        _reason: &str,
    ) -> Result<(), Self::Error> {
        crate::interrupts::with_irqs(|| {
            if blocks.len() > 1 && emmc2() && read_multi(start_block_idx.0 as u64, blocks).is_ok() {
                return Ok(());
            }
            if blocks.len() > 1 && emmc2() {
                let _ = stop_transfer();
            }
            for (index, block) in blocks.iter_mut().enumerate() {
                let lba = start_block_idx.0 as u64 + index as u64;
                read_sector(lba, &mut block.contents)?;
            }
            Ok(())
        })
    }

    fn write(&self, blocks: &[Block], start_block_idx: BlockIdx) -> Result<(), Self::Error> {
        crate::interrupts::with_irqs(|| {
            if blocks.len() > 1 && emmc2() && write_multi(start_block_idx.0 as u64, blocks).is_ok()
            {
                return Ok(());
            }
            if blocks.len() > 1 && emmc2() {
                let _ = stop_transfer();
            }
            for (index, block) in blocks.iter().enumerate() {
                let lba = start_block_idx.0 as u64 + index as u64;
                write_sector(lba, &block.contents)?;
            }
            Ok(())
        })
    }

    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        let card = CARD.lock();
        let card = card.as_ref().ok_or("no card")?;
        Ok(BlockCount(card.blocks.max(1)))
    }
}

fn read_capacity(rca: u32, sdhci: bool) -> Option<u32> {
    let send = |index, arg, resp, data, read_data| {
        if sdhci {
            sd_command(index, arg, resp, data, read_data, 200)
        } else {
            legacy_command(index, arg, resp, data, read_data)
        }
    };
    send(9, rca, RESP_136, false, false).ok()?;
    let words = [
        read(RESP0),
        read(RESP0 + 4),
        read(RESP0 + 8),
        read(RESP0 + 12),
    ];
    // SDHCI stores an R2 response with the first CSD byte in bits 23:16 of
    // the last response register. The CRC byte is not stored.
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
        let c_size =
            (((csd[6] & 0x03) as u32) << 10) | ((csd[7] as u32) << 2) | ((csd[8] as u32) >> 6);
        let c_size_mult = (((csd[9] & 0x03) as u32) << 1) | ((csd[10] as u32) >> 7);
        let block_len = 1u32 << read_bl_len;
        let mult = 1u32 << (c_size_mult + 2);
        let bytes = (c_size + 1).saturating_mul(mult).saturating_mul(block_len);
        Some(bytes / 512)
    } else {
        None
    }
}

fn emmc2() -> bool {
    HOST_PHYS.load(Ordering::Relaxed) == EMMC2_PHYS
}

fn read_sector(lba: u64, out: &mut [u8; 512]) -> Result<(), &'static str> {
    let arg = sector_arg(lba)?;
    write(BLKSIZECNT, (1 << 16) | 512);
    issue(17, arg, RESP_48, true, true)?;
    wait_interrupt(INT_READ_RDY, 500)?;
    copy_words_in(out);
    wait_interrupt(INT_DATA_DONE, 500)?;
    Ok(())
}

fn write_sector(lba: u64, data: &[u8; 512]) -> Result<(), &'static str> {
    let arg = sector_arg(lba)?;
    write(BLKSIZECNT, (1 << 16) | 512);
    issue(24, arg, RESP_48, true, false)?;
    wait_interrupt(INT_WRITE_RDY, 500)?;
    copy_words_out(data);
    wait_interrupt(INT_DATA_DONE, 500)?;
    Ok(())
}

/// CMD18 / CMD25 with the SDHCI block count and auto CMD12. One command
/// covers the whole slice instead of a stop-and-start per sector.
fn read_multi(lba: u64, blocks: &mut [Block]) -> Result<(), &'static str> {
    let count = blocks.len() as u32;
    let arg = sector_arg(lba)?;
    write(BLKSIZECNT, (count << 16) | 512);
    issue_multi(18, arg, true)?;
    for block in blocks.iter_mut() {
        wait_interrupt(INT_READ_RDY, 500)?;
        copy_words_in(&mut block.contents);
    }
    wait_interrupt(INT_DATA_DONE, 500)?;
    note_multi();
    Ok(())
}

fn write_multi(lba: u64, blocks: &[Block]) -> Result<(), &'static str> {
    let count = blocks.len() as u32;
    let arg = sector_arg(lba)?;
    write(BLKSIZECNT, (count << 16) | 512);
    issue_multi(25, arg, false)?;
    for block in blocks {
        wait_interrupt(INT_WRITE_RDY, 500)?;
        copy_words_out(&block.contents);
    }
    wait_interrupt(INT_DATA_DONE, 500)?;
    note_multi();
    Ok(())
}

fn copy_words_in(out: &mut [u8; 512]) {
    for word_index in 0..128 {
        let word = read(DATA);
        let offset = word_index * 4;
        out[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
}

fn copy_words_out(data: &[u8; 512]) {
    for word_index in 0..128 {
        let offset = word_index * 4;
        let word = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        write(DATA, word);
    }
}

fn note_multi() {
    use core::sync::atomic::AtomicBool;
    static ONCE: AtomicBool = AtomicBool::new(false);
    if ONCE.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::disklog::line("sd emmc2 multiblock");
}

fn stop_transfer() -> Result<(), &'static str> {
    let _ = sd_command(12, 0, RESP_48_BUSY, false, false, 200);
    write8(SW_RESET, RESET_CMD | RESET_DATA);
    if !wait_until(20, || read8(SW_RESET) & (RESET_CMD | RESET_DATA) == 0) {
        return Err("reset timeout");
    }
    Ok(())
}

fn issue(
    index: u32,
    arg: u32,
    resp: u32,
    data: bool,
    read_data: bool,
) -> Result<u32, &'static str> {
    if HOST_PHYS.load(Ordering::Relaxed) == EMMC2_PHYS {
        sd_command(index, arg, resp, data, read_data, 500)
    } else {
        legacy_command(index, arg, resp, data, read_data)
    }
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

fn sd_reset() -> Result<(), &'static str> {
    write8(SW_RESET, RESET_ALL);
    if !wait_until(100, || read8(SW_RESET) & RESET_ALL == 0) {
        return Err("reset timeout");
    }
    write(IRPT_EN, 0xffff_ffff);
    write(IRPT_MASK, 0xffff_ffff);
    write(INTERRUPT, 0xffff_ffff);
    Ok(())
}

fn sd_set_clock(base_hz: u32, target_hz: u32) -> Result<(), &'static str> {
    let mut clk = read16(CLOCK);
    clk &= !CLK_CARD_EN;
    write16(CLOCK, clk);
    delay_ms(1);
    let div = clock_div(base_hz, target_hz);
    write16(CLOCK, div | CLK_INT_EN);
    if !wait_until(100, || read16(CLOCK) & CLK_STABLE != 0) {
        return Err("clock timeout");
    }
    write16(CLOCK, read16(CLOCK) | CLK_CARD_EN);
    Ok(())
}

/// SDHCI 10-bit divisor. The card clock is `base / (2 * n)` for n > 0,
/// and `base` when n is 0.
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
    // Bits 15:8 are the low 8 divisor bits. Bits 7:6 are divisor bits 9:8.
    let low = ((n & 0xff) as u16) << 8;
    let high = ((n & 0x300) >> 8) as u16;
    low | (high << 6)
}

fn sd_command(
    index: u32,
    arg: u32,
    resp: u32,
    data: bool,
    read_data: bool,
    budget_ms: u64,
) -> Result<u32, &'static str> {
    // A reset before every sector dominates the transfer. Reset only when
    // the previous command left the inhibit bits set.
    if read(PRESENT) & 3 != 0 {
        write8(SW_RESET, RESET_CMD | RESET_DATA);
        if !wait_until(20, || read8(SW_RESET) & (RESET_CMD | RESET_DATA) == 0) {
            return Err("reset timeout");
        }
    }
    write(INTERRUPT, 0xffff_ffff);
    write(ARG1, arg);
    let mut mode = 0u16;
    if data {
        mode |= 1 << 1;
        if read_data {
            mode |= 1 << 4;
        }
    }
    write16(XFER, mode);
    let mut cmd = ((index as u16) & 0x3f) << 8;
    cmd |= (resp as u16) & 3;
    // R3 (ACMD41, command 41) has no CRC and no index. R2 has CRC but
    // no index. Every other response has both.
    if resp != RESP_NONE && index != 41 {
        cmd |= 1 << 3;
        if resp != RESP_136 {
            cmd |= 1 << 4;
        }
    }
    if data {
        cmd |= 1 << 5;
    }
    write16(COMMAND, cmd);
    let done = wait_interrupt(INT_CMD_DONE, budget_ms)?;
    if done & INT_ERR != 0 || done & INT_CMD_TIMEOUT != 0 {
        return Err(cmd_error(done));
    }
    Ok(read(RESP0))
}

fn cmd_error(status: u32) -> &'static str {
    if status & INT_CMD_TIMEOUT != 0 {
        "timeout"
    } else if status & (1 << 17) != 0 {
        "crc"
    } else if status & (1 << 19) != 0 {
        "index"
    } else if status & (1 << 18) != 0 {
        "endbit"
    } else {
        "cmd error"
    }
}

fn legacy_reset() -> Result<(), &'static str> {
    write(CONTROL1, read(CONTROL1) | CTRL1_SRST_HC);
    for _ in 0..100_000 {
        if read(CONTROL1) & CTRL1_SRST_HC == 0 {
            write(IRPT_EN, 0xffff_ffff);
            write(IRPT_MASK, 0xffff_ffff);
            write(INTERRUPT, 0xffff_ffff);
            crate::gic::arm_spi(EMMC_IRQ, 0x80, crate::gic::online_cpu_mask());
            crate::gic::disable_id(EMMC_IRQ);
            return Ok(());
        }
        spin(100);
    }
    Err("reset timeout")
}

fn legacy_clock(divisor: u32) -> Result<(), &'static str> {
    let mut ctrl = read(CONTROL1);
    ctrl &= !CTRL1_CLK_EN;
    write(CONTROL1, ctrl);
    ctrl = (ctrl & 0xffff_00ff) | ((divisor & 0xff) << 8) | CTRL1_CLK_INTLEN | (0xe << 16);
    write(CONTROL1, ctrl);
    for _ in 0..100_000 {
        if read(CONTROL1) & CTRL1_CLK_STABLE != 0 {
            write(CONTROL1, read(CONTROL1) | CTRL1_CLK_EN);
            return Ok(());
        }
        spin(100);
    }
    Err("clock timeout")
}

fn legacy_command(
    index: u32,
    arg: u32,
    resp: u32,
    data: bool,
    read_data: bool,
) -> Result<u32, &'static str> {
    write(CONTROL1, read(CONTROL1) | CTRL1_SRST_CMD | CTRL1_SRST_DATA);
    spin(100);
    write(INTERRUPT, 0xffff_ffff);
    write(ARG1, arg);
    let mut tm = (index & 0x3f) << 24;
    tm |= (resp & 3) << 16;
    if data {
        tm |= 1 << 21;
        tm |= 1 << 1;
        if read_data {
            tm |= 1 << 4;
        }
    }
    write(CMDTM, tm);
    let done = wait_interrupt(INT_CMD_DONE, 500)?;
    if done & INT_ERR != 0 || done & INT_CMD_TIMEOUT != 0 {
        return Err("cmd error");
    }
    Ok(read(RESP0))
}

fn wait_interrupt(bit: u32, budget_ms: u64) -> Result<u32, &'static str> {
    // The checkpoint is what the hang check prints when this core's timer
    // is not running. A transfer that polls for a command must keep it fresh.
    crate::act::note_core(crate::percpu::id());
    let start = crate::timer::get_ticks();
    let budget = ticks_for_ms(budget_ms);
    let mut spins = 0u32;
    loop {
        let status = read(INTERRUPT);
        // Clear only the bits this wait consumed. QEMU's SDHCI sets the
        // buffer-ready bit in the same status word as command-complete.
        // Clearing the whole word there drops the data interrupt, and the
        // following read then waits until the counter expires.
        let interesting = bit | INT_ERR | INT_CMD_TIMEOUT;
        if status & interesting != 0 {
            write(INTERRUPT, status & interesting);
            return Ok(status);
        }
        let timed_out = if budget == 0 {
            spins > 50_000_000
        } else {
            crate::timer::get_ticks().wrapping_sub(start) >= budget
        };
        if timed_out {
            return Err("timeout");
        }
        spins = spins.wrapping_add(1);
        if spins & 63 == 0 {
            core::hint::spin_loop();
            crate::act::note_core(crate::percpu::id());
        }
    }
}

fn issue_multi(index: u32, arg: u32, read_data: bool) -> Result<(), &'static str> {
    if read(PRESENT) & 3 != 0 {
        write8(SW_RESET, RESET_CMD | RESET_DATA);
        if !wait_until(20, || read8(SW_RESET) & (RESET_CMD | RESET_DATA) == 0) {
            return Err("reset timeout");
        }
    }
    write(INTERRUPT, 0xffff_ffff);
    write(ARG1, arg);
    // Block-count enable, auto CMD12, multi-block, and the direction.
    let mut mode = (1 << 1) | (1 << 2) | (1 << 5);
    if read_data {
        mode |= 1 << 4;
    }
    write16(XFER, mode);
    let mut cmd = ((index as u16) & 0x3f) << 8;
    cmd |= RESP_48 as u16;
    cmd |= (1 << 3) | (1 << 4) | (1 << 5);
    write16(COMMAND, cmd);
    let done = wait_interrupt(INT_CMD_DONE, 500)?;
    if done & INT_ERR != 0 || done & INT_CMD_TIMEOUT != 0 {
        return Err(cmd_error(done));
    }
    Ok(())
}

fn log_data_clock(base_hz: u32) {
    let reg = read16(CLOCK);
    let n = ((reg >> 8) & 0xff) as u32 | (((reg as u32) >> 6) & 0x3) << 8;
    let card = if n == 0 {
        base_hz
    } else {
        base_hz / (2 * n.max(1))
    };
    let mut line = Buf::new();
    line.push("sd emmc2 data hz ");
    line.dec(card);
    line.push(" base ");
    line.dec(base_hz);
    line.emit();
}

fn wait_until(budget_ms: u64, ready: impl Fn() -> bool) -> bool {
    let start = crate::timer::get_ticks();
    let budget = ticks_for_ms(budget_ms);
    let mut spins = 0u32;
    loop {
        if ready() {
            return true;
        }
        let timed_out = if budget == 0 {
            spins > 5_000_000
        } else {
            crate::timer::get_ticks().wrapping_sub(start) >= budget
        };
        if timed_out {
            return false;
        }
        spins = spins.wrapping_add(1);
        core::hint::spin_loop();
    }
}

fn ticks_for_ms(ms: u64) -> u64 {
    let freq = cntfrq();
    if freq == 0 {
        return 0;
    }
    freq.saturating_mul(ms) / 1000
}

fn delay_ms(ms: u64) {
    let start = crate::timer::get_ticks();
    let budget = ticks_for_ms(ms);
    if budget == 0 {
        spin(50_000);
        return;
    }
    while crate::timer::get_ticks().wrapping_sub(start) < budget {
        core::hint::spin_loop();
    }
}

fn mailbox_clock() -> u32 {
    // GET_CLOCK_RATE returns the id and the rate. A 4-byte buffer only
    // echoes the id, which is why the previous boot logged the capability
    // register instead of the mailbox.
    let mut words = [0u32; 8];
    words[0] = 8 * 4;
    words[1] = 0;
    words[2] = TAG_GET_CLOCK;
    words[3] = 8;
    words[4] = 0;
    words[5] = CLOCK_EMMC2;
    words[6] = 0;
    words[7] = 0;
    if !mailbox(&mut words) {
        return 0;
    }
    words[6]
}

fn mailbox_set_clock(hz: u32) -> u32 {
    let mut words = [0u32; 10];
    words[0] = 9 * 4;
    words[1] = 0;
    words[2] = TAG_SET_CLOCK;
    words[3] = 12;
    words[4] = 0;
    words[5] = CLOCK_EMMC2;
    words[6] = hz;
    words[7] = 1;
    words[8] = 0;
    if !mailbox(&mut words) {
        return 0;
    }
    words[6]
}

fn cycle_sd_power() -> bool {
    let _ = set_sd_power(0);
    delay_ms(20);
    let on = set_sd_power(3);
    delay_ms(20);
    if !on {
        let _ = set_sd_power(3);
        delay_ms(20);
    }
    on
}

fn set_sd_power(state: u32) -> bool {
    let mut words = [0u32; 8];
    words[0] = 8 * 4;
    words[1] = 0;
    words[2] = TAG_SET_POWER;
    words[3] = 8;
    words[4] = 0;
    words[5] = SD_POWER_ID;
    words[6] = state;
    words[7] = 0;
    mailbox(&mut words)
}

fn mailbox(words: &mut [u32]) -> bool {
    if super::mbox::run(MBOX_BASE, words) != 0 {
        return false;
    }
    words[1] == 0x8000_0000 && words.len() > 4 && words[4] & 0x8000_0000 != 0
}

fn cntfrq() -> u64 {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {freq}, cntfrq_el0", freq = out(reg) freq);
    }
    freq
}

fn catching(phys: u64) -> Option<u32> {
    crate::exception::read_u32_catching(crate::addr::phys_to_virt(phys) as *const u32)
}

fn host() -> usize {
    crate::addr::phys_to_virt(HOST_PHYS.load(Ordering::Relaxed)) as usize
}

fn read(offset: usize) -> u32 {
    unsafe { read_volatile((host() + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    unsafe { write_volatile((host() + offset) as *mut u32, value) }
}

fn read16(offset: usize) -> u16 {
    unsafe { read_volatile((host() + offset) as *const u16) }
}

fn write16(offset: usize, value: u16) {
    unsafe { write_volatile((host() + offset) as *mut u16, value) }
}

fn read8(offset: usize) -> u8 {
    unsafe { read_volatile((host() + offset) as *const u8) }
}

fn write8(offset: usize, value: u8) {
    unsafe { write_volatile((host() + offset) as *mut u8, value) }
}

fn spin(count: u32) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}

#[allow(dead_code)]
const _LAYOUT: () = {
    let _ = CONTROL0;
};
