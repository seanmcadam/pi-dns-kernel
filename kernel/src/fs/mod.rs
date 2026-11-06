//! FAT32 config and log storage.
//!
//! QEMU uses a virtio-blk disk. The Pi 4 image uses the EMMC2 driver.
//! `CONFIG.TXT` holds records, network settings, and upstream resolvers.
//! `LOG.TXT` is appended until it passes `ROTATE_AT`, then renamed in spirit
//! by copying it to `LOG.OLD` and starting a fresh log.

use alloc::string::String;
use alloc::vec::Vec;
use embedded_sdmmc::{Mode, RawFile, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use spin::Mutex;

#[cfg(feature = "pi4")]
mod emmc;
#[cfg(not(feature = "pi4"))]
mod virtio_blk;

#[cfg(feature = "pi4")]
use emmc::DiskDev;
#[cfg(not(feature = "pi4"))]
use virtio_blk::DiskDev;

const ROTATE_AT: u32 = 512;
const CONFIG_NAME: &str = "CONFIG.TXT";
const LOG_NAME: &str = "LOG.TXT";
const LOG_OLD: &str = "LOG.OLD";

#[derive(Debug, Clone, Copy)]
pub enum FsError {
    NotInitialized,
    ReadError,
    WriteError,
}

struct Clock;

impl TimeSource for Clock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 10,
            zero_indexed_day: 4,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

type Mgr = VolumeManager<DiskDev, Clock, 4, 4, 1>;

static FS: Mutex<Option<Mgr>> = Mutex::new(None);

pub fn init() -> Result<(), FsError> {
    let found = {
        #[cfg(feature = "pi4")]
        {
            emmc::probe()
        }
        #[cfg(not(feature = "pi4"))]
        {
            virtio_blk::probe()
        }
    };
    if !found {
        log::info!("No block device, filesystem not mounted");
        return Err(FsError::NotInitialized);
    }

    let mut mgr = VolumeManager::new(DiskDev, Clock);
    match mgr.open_raw_volume(VolumeIdx(0)) {
        Ok(volume) => {
            mgr.close_volume(volume).ok();
            *FS.lock() = Some(mgr);
            log::info!("FAT32 filesystem mounted");
            append_log("boot");
            Ok(())
        }
        Err(err) => {
            log::warn!("FAT32 mount failed: {:?}", err);
            Err(FsError::NotInitialized)
        }
    }
}

pub fn read_config() -> Result<String, FsError> {
    let bytes = read_file(CONFIG_NAME)?;
    String::from_utf8(bytes).map_err(|_| FsError::ReadError)
}

pub fn write_config(data: &str) -> Result<(), FsError> {
    write_file(CONFIG_NAME, data.as_bytes())
}

pub fn append_log(message: &str) {
    if FS.lock().is_none() {
        return;
    }
    let _ = rotate_log();
    let mut line = alloc::string::String::from(message);
    while line.len() < 72 {
        line.push(' ');
    }
    line.push('\n');
    let _ = with_file(LOG_NAME, Mode::ReadWriteCreateOrAppend, true, |mgr, file| {
        mgr.write(file, line.as_bytes())
            .map_err(|_| FsError::WriteError)
    });
}

fn rotate_log() -> Result<(), FsError> {
    let len = match with_file(LOG_NAME, Mode::ReadOnly, false, |mgr, file| {
        mgr.file_length(file).map_err(|_| FsError::ReadError)
    }) {
        Ok(len) => len,
        Err(_) => return Ok(()),
    };
    if len < ROTATE_AT {
        return Ok(());
    }
    let data = read_file(LOG_NAME)?;
    write_file(LOG_OLD, &data)?;
    write_file(LOG_NAME, b"")?;
    log::info!("Rotated log to LOG.OLD ({} bytes)", data.len());
    Ok(())
}

fn read_file(name: &str) -> Result<Vec<u8>, FsError> {
    with_file(name, Mode::ReadOnly, false, |mgr, file| {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = mgr.read(file, &mut buf).map_err(|_| FsError::ReadError)?;
            if n == 0 || out.len() >= 8192 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    })
}

fn write_file(name: &str, data: &[u8]) -> Result<(), FsError> {
    let data = data.to_vec();
    with_file(
        name,
        Mode::ReadWriteCreateOrTruncate,
        true,
        move |mgr, file| {
            mgr.write(file, &data).map_err(|_| FsError::WriteError)?;
            Ok(())
        },
    )
}

fn with_file<R>(
    name: &str,
    mode: Mode,
    writing: bool,
    f: impl FnOnce(&mut Mgr, RawFile) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let fail = if writing {
        FsError::WriteError
    } else {
        FsError::ReadError
    };
    let mut guard = FS.lock();
    let mgr = guard.as_mut().ok_or(FsError::NotInitialized)?;
    let volume = mgr
        .open_raw_volume(VolumeIdx(0))
        .map_err(|_| fail)?;
    let dir = match mgr.open_root_dir(volume) {
        Ok(dir) => dir,
        Err(_) => {
            mgr.close_volume(volume).ok();
            return Err(fail);
        }
    };
    let file = match mgr.open_file_in_dir(dir, name, mode) {
        Ok(file) => file,
        Err(_) => {
            mgr.close_dir(dir).ok();
            mgr.close_volume(volume).ok();
            return Err(fail);
        }
    };
    let result = f(mgr, file);
    mgr.close_file(file).ok();
    mgr.close_dir(dir).ok();
    mgr.close_volume(volume).ok();
    result
}
