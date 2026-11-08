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

#[derive(Debug)]
pub enum FsError {
    NotInitialized,
    Io { op: &'static str, detail: String },
}

impl core::fmt::Display for FsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FsError::NotInitialized => write!(f, "filesystem not mounted"),
            FsError::Io { op, detail } => write!(f, "{op}: {detail}"),
        }
    }
}

fn fs_err(op: &'static str, err: impl core::fmt::Debug) -> FsError {
    let detail = alloc::format!("{err:?}");
    log::warn!("FAT {op} failed: {detail}");
    FsError::Io { op, detail }
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
            // Prove the volume can be opened, then put the slot back. A later
            // save needs that only slot. release() always drops it.
            if let Err(err) = release(&mut mgr, volume, None, None) {
                log::warn!("FAT mount close failed: {err}");
            }
            *FS.lock() = Some(mgr);
            log::info!("FAT32 filesystem mounted");
            append_log("boot");
            Ok(())
        }
        Err(err) => {
            log::warn!("FAT32 mount failed: {err:?}");
            Err(FsError::NotInitialized)
        }
    }
}

pub fn read_config() -> Result<String, FsError> {
    let bytes = read_file(CONFIG_NAME)?;
    String::from_utf8(bytes).map_err(|err| fs_err("config encoding", err))
}

pub fn write_config(data: &str) -> Result<(), String> {
    write_file(CONFIG_NAME, data.as_bytes()).map_err(|err| alloc::format!("save failed: {err}"))
}

pub fn read_named(name: &str, out: &mut [u8]) -> Result<usize, FsError> {
    if out.is_empty() {
        return Ok(0);
    }
    with_file(name, Mode::ReadOnly, |mgr, file| {
        let mut got = 0;
        let mut buf = [0u8; 512];
        loop {
            if got >= out.len() {
                break;
            }
            let n = mgr.read(file, &mut buf).map_err(|err| fs_err("read", err))?;
            if n == 0 {
                break;
            }
            let copy = n.min(out.len() - got);
            out[got..got + copy].copy_from_slice(&buf[..copy]);
            got += copy;
        }
        Ok(got)
    })
}

pub fn write_named(name: &str, data: &[u8], append: bool) -> Result<(), FsError> {
    let mode = if append {
        Mode::ReadWriteCreateOrAppend
    } else {
        Mode::ReadWriteCreateOrTruncate
    };
    with_file(name, mode, |mgr, file| {
        if data.is_empty() {
            return Ok(());
        }
        mgr.write(file, data).map_err(|err| fs_err("write", err))?;
        Ok(())
    })
}

pub fn append_log(message: &str) {
    if FS.lock().is_none() {
        return;
    }
    if crate::task::log_live() {
        crate::logger::post(1, 1, message.as_bytes());
        return;
    }
    append_log_direct(message);
}

fn append_log_direct(message: &str) {
    let _ = rotate_log();
    let mut line = alloc::string::String::from(message);
    while line.len() < 72 {
        line.push(' ');
    }
    line.push('\n');
    let _ = with_file(LOG_NAME, Mode::ReadWriteCreateOrAppend, |mgr, file| {
        mgr.write(file, line.as_bytes())
            .map_err(|err| fs_err("append log", err))
    });
}

fn rotate_log() -> Result<(), FsError> {
    let len = match with_file(LOG_NAME, Mode::ReadOnly, |mgr, file| {
        mgr.file_length(file).map_err(|err| fs_err("log length", err))
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
    with_file(name, Mode::ReadOnly, |mgr, file| {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = mgr.read(file, &mut buf).map_err(|err| fs_err("read", err))?;
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
    with_file(name, Mode::ReadWriteCreateOrTruncate, move |mgr, file| {
        mgr.write(file, &data).map_err(|err| fs_err("write", err))?;
        Ok(())
    })
}

/// One open volume, directory, and file, closed on every return path.
///
/// `VolumeManager` is built with a single volume slot. Dropping a `RawVolume`
/// does not free it. `release` is the only place that closes, so a missing
/// `CONFIG.TXT`, a failed log write, or an error from the callback cannot
/// leave the slot taken.
fn with_file<R>(
    name: &str,
    mode: Mode,
    f: impl FnOnce(&mut Mgr, RawFile) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let mut guard = FS.lock();
    let mgr = guard.as_mut().ok_or(FsError::NotInitialized)?;
    let volume = match mgr.open_raw_volume(VolumeIdx(0)) {
        Ok(volume) => volume,
        Err(err) => return Err(fs_err("open volume", err)),
    };
    let dir = match mgr.open_root_dir(volume) {
        Ok(dir) => dir,
        Err(err) => {
            let _ = release(mgr, volume, None, None);
            return Err(fs_err("open root", err));
        }
    };
    let file = match mgr.open_file_in_dir(dir, name, mode) {
        Ok(file) => file,
        Err(err) => {
            let closed = release(mgr, volume, Some(dir), None);
            // A missing CONFIG.TXT on a fresh volume is normal.
            if matches!(err, embedded_sdmmc::Error::NotFound) {
                return closed.and(Err(FsError::Io {
                    op: "open file",
                    detail: String::from("NotFound"),
                }));
            }
            return Err(closed.err().unwrap_or_else(|| fs_err("open file", err)));
        }
    };
    let result = f(mgr, file);
    let closed = release(mgr, volume, Some(dir), Some(file));
    match (result, closed) {
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Ok(value), Ok(())) => Ok(value),
    }
}

fn release(
    mgr: &mut Mgr,
    volume: embedded_sdmmc::RawVolume,
    dir: Option<embedded_sdmmc::RawDirectory>,
    file: Option<RawFile>,
) -> Result<(), FsError> {
    let mut error = None;
    if let Some(file) = file {
        if let Err(err) = mgr.close_file(file) {
            error = Some(fs_err("close file", err));
        }
    }
    if let Some(dir) = dir {
        if let Err(err) = mgr.close_dir(dir) {
            error.get_or_insert_with(|| fs_err("close dir", err));
        }
    }
    if let Err(err) = mgr.close_volume(volume) {
        error.get_or_insert_with(|| fs_err("close volume", err));
    }
    match error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
