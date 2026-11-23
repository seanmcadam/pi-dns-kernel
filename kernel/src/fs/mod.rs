//! FAT32 config and log storage.
//!
//! The board supplies the block device: virtio-blk on QEMU virt, EMMC on
//! the Pi 4 image. `CONFIG.JSN` is the main config. A leftover `CONFIG.TXT`
//! is migrated once. `LOG.JSN` is the log service config. `LOG.TXT` is the
//! active log; when it reaches the configured size it is renamed to
//! `LOG1.TXT`, and older archives shift up to `LOG16.TXT`.

use alloc::string::String;
use alloc::vec::Vec;
use embedded_sdmmc::{Mode, RawFile, TimeSource, Timestamp, VolumeIdx, VolumeManager};
use spin::Mutex;

use crate::board::DiskDev;

pub const CONFIG_NAME: &str = "CONFIG.JSN";
pub const LEGACY_CONFIG: &str = "CONFIG.TXT";
pub const LOG_CONFIG_NAME: &str = "LOG.JSN";
pub const LOG_NAME: &str = "LOG.TXT";

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
    let found = crate::board::probe_disk();
    if !found {
        log::info!("No block device, filesystem not mounted");
        crate::disklog::line("sd no block device");
        crate::disklog::fail_if_empty("no block device");
        crate::disklog::set_mounted(false);
        return Err(FsError::NotInitialized);
    }

    let mut mgr = VolumeManager::new(crate::board::disk(), Clock);
    match mgr.open_raw_volume(VolumeIdx(0)) {
        Ok(volume) => {
            // Prove the volume can be opened, then put the slot back. A later
            // save needs that only slot. release() always drops it.
            if let Err(err) = release(&mut mgr, volume, None, None) {
                log::warn!("FAT mount close failed: {err}");
            }
            *FS.lock() = Some(mgr);
            log::info!("FAT32 filesystem mounted");
            crate::disklog::line("sd fat mounted");
            crate::disklog::set_mounted(true);
            append_log("boot");
            Ok(())
        }
        Err(err) => {
            log::warn!("FAT32 mount failed: {err:?}");
            crate::disklog::line("sd fat mount failed");
            crate::disklog::note_error("fat mount");
            crate::disklog::set_mounted(false);
            Err(FsError::NotInitialized)
        }
    }
}

pub fn read_text(name: &str) -> Result<String, FsError> {
    let bytes = read_file(name)?;
    String::from_utf8(bytes).map_err(|err| fs_err("config encoding", err))
}

pub fn write_config(data: &str) -> Result<(), String> {
    replace_file(CONFIG_NAME, data.as_bytes()).map_err(|err| alloc::format!("save failed: {err}"))
}

/// Write `data` to a temp name, then swing the directory entry onto `name`.
/// A power loss leaves either the previous file or `CFG.OLD` / `LOGC.OLD`.
pub fn replace_file(name: &str, data: &[u8]) -> Result<(), FsError> {
    let (tmp, old) = scratch(name).ok_or_else(|| FsError::Io {
        op: "replace",
        detail: String::from("unsupported name"),
    })?;
    write_file(tmp, data)?;
    let _ = delete_file(old);
    if file_exists(name)? {
        rename_file(name, old)?;
    }
    match rename_file(tmp, name) {
        Ok(()) => {
            let _ = delete_file(old);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

/// `CRASH/CRASHnnn.TXT`. Creates the directory when it is missing.
pub fn save_crash(name: &str, data: &[u8]) -> Result<(), FsError> {
    with_crash_file(name, Mode::ReadWriteCreateOrTruncate, true, |mgr, file| {
        if !data.is_empty() {
            mgr.write(file, data).map_err(|err| fs_err("write", err))?;
        }
        Ok(())
    })
}

pub fn crash_exists(name: &str) -> bool {
    with_crash_file(name, Mode::ReadOnly, false, |mgr, file| {
        mgr.file_length(file)
            .map_err(|err| fs_err("file length", err))?;
        Ok(())
    })
    .is_ok()
}

pub fn read_crash(name: &str, out: &mut [u8]) -> Result<usize, FsError> {
    with_crash_file(name, Mode::ReadOnly, false, |mgr, file| {
        let n = mgr.read(file, out).map_err(|err| fs_err("read", err))?;
        Ok(n)
    })
}

fn with_crash_file<R>(
    name: &str,
    mode: Mode,
    create_dir: bool,
    f: impl FnOnce(&mut Mgr, RawFile) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let mut guard = FS.lock();
    let mgr = guard.as_mut().ok_or(FsError::NotInitialized)?;
    let volume = match mgr.open_raw_volume(VolumeIdx(0)) {
        Ok(volume) => volume,
        Err(err) => return Err(fs_err("open volume", err)),
    };
    let root = match mgr.open_root_dir(volume) {
        Ok(dir) => dir,
        Err(err) => {
            let _ = mgr.close_volume(volume);
            return Err(fs_err("open root", err));
        }
    };
    let crash = match mgr.open_dir(root, "CRASH") {
        Ok(dir) => dir,
        Err(err) => {
            if !create_dir {
                let _ = mgr.close_dir(root);
                let _ = mgr.close_volume(volume);
                return Err(fs_err("open crash", err));
            }
            if let Err(err) = mgr.make_dir_in_dir(root, "CRASH") {
                let _ = mgr.close_dir(root);
                let _ = mgr.close_volume(volume);
                return Err(fs_err("make crash", err));
            }
            match mgr.open_dir(root, "CRASH") {
                Ok(dir) => dir,
                Err(err) => {
                    let _ = mgr.close_dir(root);
                    let _ = mgr.close_volume(volume);
                    return Err(fs_err("open crash", err));
                }
            }
        }
    };
    let file = match mgr.open_file_in_dir(crash, name, mode) {
        Ok(file) => file,
        Err(err) => {
            let _ = mgr.close_dir(crash);
            let _ = mgr.close_dir(root);
            let _ = mgr.close_volume(volume);
            return Err(fs_err("open file", err));
        }
    };
    let result = f(mgr, file);
    let _ = mgr.close_file(file);
    let _ = mgr.close_dir(crash);
    let _ = mgr.close_dir(root);
    let _ = mgr.close_volume(volume);
    result
}

pub fn file_exists(name: &str) -> Result<bool, FsError> {
    match file_len(name) {
        Ok(_) => Ok(true),
        Err(FsError::Io { detail, .. }) if detail == "NotFound" => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn file_len(name: &str) -> Result<u32, FsError> {
    with_file(name, Mode::ReadOnly, |mgr, file| {
        mgr.file_length(file)
            .map_err(|err| fs_err("file length", err))
    })
}

pub fn delete_file(name: &str) -> Result<(), FsError> {
    match with_dir(|mgr, dir| {
        mgr.delete_file_in_dir(dir, name).map_err(|err| {
            // A missing file is the successful outcome of delete. Scrub of
            // leftover temp names hits this on every boot.
            if matches!(err, embedded_sdmmc::Error::NotFound) {
                FsError::Io {
                    op: "delete",
                    detail: String::from("NotFound"),
                }
            } else {
                fs_err("delete", err)
            }
        })
    }) {
        Err(FsError::Io { detail, .. }) if detail.contains("NotFound") => Ok(()),
        other => other,
    }
}

pub fn rename_file(from: &str, to: &str) -> Result<(), FsError> {
    with_dir(|mgr, dir| {
        mgr.rename_file_in_dir(dir, from, to)
            .map_err(|err| fs_err("rename", err))
    })
}

pub fn digest(name: &str) -> Result<fwupd::Digest, FsError> {
    with_file(name, Mode::ReadOnly, |mgr, file| {
        let mut sha = fwupd::Sha256::new();
        let mut head = [0u8; 64];
        let mut len = 0u32;
        let mut buf = [0u8; 512];
        loop {
            let n = mgr
                .read(file, &mut buf)
                .map_err(|err| fs_err("read", err))?;
            if n == 0 {
                break;
            }
            if len < 64 {
                let want = (64 - len as usize).min(n);
                head[len as usize..len as usize + want].copy_from_slice(&buf[..want]);
            }
            sha.update(&buf[..n]);
            len = len.saturating_add(n as u32);
            if len > 8 * 1024 * 1024 {
                return Err(fs_err("read", "oversized"));
            }
        }
        Ok(fwupd::Digest {
            len,
            sha: sha.finish(),
            head,
        })
    })
}

/// Read `out.len()` bytes starting at `offset`. A short file returns a short
/// slice. The file is not loaded into the heap.
pub fn read_at(name: &str, offset: u32, out: &mut [u8]) -> Result<usize, FsError> {
    if out.is_empty() {
        return Ok(0);
    }
    with_file(name, Mode::ReadOnly, |mgr, file| {
        if offset > 0 {
            mgr.file_seek_from_start(file, offset)
                .map_err(|err| fs_err("seek", err))?;
        }
        mgr.read(file, out).map_err(|err| fs_err("read", err))
    })
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
            let n = mgr
                .read(file, &mut buf)
                .map_err(|err| fs_err("read", err))?;
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
    if append {
        maybe_rotate(name);
    }
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

pub fn append_log_direct(message: &str) {
    let mut line = alloc::string::String::from(message);
    while line.len() < 72 {
        line.push(' ');
    }
    line.push('\n');
    let _ = write_named(LOG_NAME, line.as_bytes(), true);
}

fn maybe_rotate(name: &str) {
    if name != LOG_NAME {
        return;
    }
    let _ = rotate_log();
}

fn rotate_log() -> Result<(), FsError> {
    let len = match file_len(LOG_NAME) {
        Ok(len) => len,
        Err(_) => return Ok(()),
    };
    let (max, archives) = crate::config_manager::log_limits();
    if len < max {
        return Ok(());
    }
    if archives == 0 {
        write_file(LOG_NAME, b"")?;
        log::info!("Truncated log at {len} bytes");
        return Ok(());
    }
    let _ = delete_archive(archives);
    let mut index = archives;
    while index > 1 {
        let from = archive_name(index - 1);
        let to = archive_name(index);
        if file_exists(from)? {
            let _ = delete_file(to);
            rename_file(from, to)?;
        }
        index -= 1;
    }
    if file_exists(LOG_NAME)? {
        let to = archive_name(1);
        let _ = delete_file(to);
        rename_file(LOG_NAME, to)?;
    }
    log::info!("Rotated log ({len} bytes, {archives} archives)");
    Ok(())
}

fn delete_archive(index: u32) -> Result<(), FsError> {
    delete_file(archive_name(index))
}

fn archive_name(index: u32) -> &'static str {
    match index {
        1 => "LOG1.TXT",
        2 => "LOG2.TXT",
        3 => "LOG3.TXT",
        4 => "LOG4.TXT",
        5 => "LOG5.TXT",
        6 => "LOG6.TXT",
        7 => "LOG7.TXT",
        8 => "LOG8.TXT",
        9 => "LOG9.TXT",
        10 => "LOG10.TXT",
        11 => "LOG11.TXT",
        12 => "LOG12.TXT",
        13 => "LOG13.TXT",
        14 => "LOG14.TXT",
        15 => "LOG15.TXT",
        _ => "LOG16.TXT",
    }
}

fn scratch(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        CONFIG_NAME => Some(("CFG.TMP", "CFG.OLD")),
        LOG_CONFIG_NAME => Some(("LOGC.TMP", "LOGC.OLD")),
        _ => None,
    }
}

fn read_file(name: &str) -> Result<Vec<u8>, FsError> {
    with_file(name, Mode::ReadOnly, |mgr, file| {
        let mut out = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let n = mgr
                .read(file, &mut buf)
                .map_err(|err| fs_err("read", err))?;
            if n == 0 || out.len() >= 16384 {
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

fn with_dir<R>(
    f: impl FnOnce(&mut Mgr, embedded_sdmmc::RawDirectory) -> Result<R, FsError>,
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
    let result = f(mgr, dir);
    let closed = release(mgr, volume, Some(dir), None);
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
