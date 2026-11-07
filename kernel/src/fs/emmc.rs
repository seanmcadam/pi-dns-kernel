//! BCM2711 EMMC2 SD host (Pi 4, MMIO 0xFE340000).
//!
//! The register map matches the Arasan controller used by the Pi 4 SD slot:
//! argument, block count, command/transfer mode, response, data FIFO, and
//! the control/interrupt registers. The card is brought up with the standard
//! SD identification sequence (CMD0, CMD8, ACMD41, CMD2, CMD3, CMD7) and then
//! read or written one 512-byte sector at a time.
//!
//! This driver is compiled into the Pi 4 image and has not been run on hardware.

use core::ptr::{read_volatile, write_volatile};
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};
use spin::Mutex;

const BASE: usize = crate::addr::phys_to_virt(0xFE34_0000) as usize;

const BLKSIZECNT: usize = 0x04;
const ARG1: usize = 0x08;
const CMDTM: usize = 0x0c;
const RESP0: usize = 0x10;
const DATA: usize = 0x20;
const CONTROL1: usize = 0x2c;
const INTERRUPT: usize = 0x30;
const IRPT_MASK: usize = 0x34;
const IRPT_EN: usize = 0x38;
const SLOTISR_VER: usize = 0xfc;

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

const RESP_NONE: u32 = 0;
const RESP_136: u32 = 1;
const RESP_48: u32 = 2;
const RESP_48_BUSY: u32 = 3;

struct Card {
    rca: u32,
    high_capacity: bool,
    blocks: u32,
}

static CARD: Mutex<Option<Card>> = Mutex::new(None);

pub struct DiskDev;

pub fn probe() -> bool {
    let version = read(SLOTISR_VER);
    if version == 0 || version == 0xffff_ffff {
        log::warn!("EMMC2 controller is not present");
        return false;
    }
    log::info!("EMMC2 slot version {:#x}", version);
    match identify() {
        Ok(card) => {
            log::info!(
                "EMMC2 card rca {:#x}, {} blocks",
                card.rca,
                card.blocks
            );
            *CARD.lock() = Some(card);
            true
        }
        Err(err) => {
            log::warn!("EMMC2 card init failed: {}", err);
            false
        }
    }
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
    set_clock(0x80)?;
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
    command(7, rca, RESP_48_BUSY, false, false)?;
    command(16, 512, RESP_48, false, false)?;
    set_clock(0x02)?;
    let blocks = read_capacity(rca).unwrap_or(0);
    Ok(Card {
        rca,
        high_capacity,
        blocks,
    })
}

fn read_capacity(rca: u32) -> Option<u32> {
    command(9, rca, RESP_136, false, false).ok()?;
    let words = [read(RESP0), read(RESP0 + 4), read(RESP0 + 8), read(RESP0 + 12)];
    // CSD structure is the top two bits of the 128-bit register. RESP0 holds
    // the last word shifted out, so the structure field is in the top bits of
    // the last response word.
    let structure = words[3] >> 30;
    if structure == 1 {
        let c_size = ((words[2] & 0x3f) << 16) | (words[1] >> 16);
        Some((c_size + 1).saturating_mul(1024))
    } else {
        None
    }
}

fn read_sector(lba: u64, out: &mut [u8; 512]) -> Result<(), &'static str> {
    let arg = sector_arg(lba)?;
    write(BLKSIZECNT, (1 << 16) | 512);
    command(17, arg, RESP_48, true, true)?;
    wait_interrupt(INT_READ_RDY)?;
    for word_index in 0..128 {
        let word = read(DATA);
        let offset = word_index * 4;
        out[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    wait_interrupt(INT_DATA_DONE)?;
    Ok(())
}

fn write_sector(lba: u64, data: &[u8; 512]) -> Result<(), &'static str> {
    let arg = sector_arg(lba)?;
    write(BLKSIZECNT, (1 << 16) | 512);
    command(24, arg, RESP_48, true, false)?;
    wait_interrupt(INT_WRITE_RDY)?;
    for word_index in 0..128 {
        let offset = word_index * 4;
        let word = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        write(DATA, word);
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
    write(CONTROL1, read(CONTROL1) | CTRL1_SRST_HC);
    for _ in 0..100_000 {
        if read(CONTROL1) & CTRL1_SRST_HC == 0 {
            write(IRPT_EN, 0xffff_ffff);
            write(IRPT_MASK, 0xffff_ffff);
            write(INTERRUPT, 0xffff_ffff);
            return Ok(());
        }
        spin(100);
    }
    Err("emmc reset timeout")
}

fn set_clock(divisor: u32) -> Result<(), &'static str> {
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
    Err("emmc clock timeout")
}

fn command(index: u32, arg: u32, resp: u32, data: bool, read_data: bool) -> Result<u32, &'static str> {
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
    let done = wait_interrupt(INT_CMD_DONE)?;
    if done & INT_ERR != 0 || done & INT_CMD_TIMEOUT != 0 {
        return Err("emmc command error");
    }
    Ok(read(RESP0))
}

fn wait_interrupt(bit: u32) -> Result<u32, &'static str> {
    for _ in 0..200_000 {
        let status = read(INTERRUPT);
        if status & (bit | INT_ERR | INT_CMD_TIMEOUT) != 0 {
            write(INTERRUPT, status);
            return Ok(status);
        }
        spin(50);
    }
    Err("emmc timeout")
}

fn read(offset: usize) -> u32 {
    unsafe { read_volatile((BASE + offset) as *const u32) }
}

fn write(offset: usize, value: u32) {
    unsafe { write_volatile((BASE + offset) as *mut u32, value) }
}

fn spin(count: u32) {
    for _ in 0..count {
        core::hint::spin_loop();
    }
}
