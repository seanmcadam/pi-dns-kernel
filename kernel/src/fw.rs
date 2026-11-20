//! Staged firmware updates on the boot FAT volume.
//!
//! The live kernel and `config.txt` are never destinations. A kernel upload
//! lands on `KERNEL8.NEW` (Pi 4 and virt) or `K2712.NEW` (Pi 5). The Pi 4
//! reboot that should try it asks the VideoCore firmware to load
//! `tryboot.txt` once. That flag is cleared by the bootloader before
//! `start.elf`, so a failed boot falls back to `config.txt` and the
//! untouched `kernel8.img`. A rename of `kernel8.img` itself has a window
//! with no kernel, so this path does not do that and it does not promote
//! the staged file over the live one.
//!
//! Pi 5 can stage a short name. Its tryboot mailbox is not wired, and the
//! real `kernel_2712.img` name does not fit 8.3, so that file is never
//! opened. Virt stages the Pi 4 names. QEMU boots with `-kernel`, so a
//! virt reboot does not execute the staged file. The files are still on
//! the FAT volume for the test to read back.

use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

use fwupd::{self, Card, CommitError};

const CHUNK: usize = 1024;
const TEST_MAX: u32 = 64 * 1024;
const KERNEL_MAX: u32 = 8 * 1024 * 1024;
const STATUS_LEN: usize = 160;

const KIND_TEST: u64 = 1;
const KIND_KERNEL: u64 = 2;
const KIND_BOOT: u64 = 3;
const KIND_JSON: u64 = 4;
const KIND_LOG: u64 = 5;

struct Names {
    dest: &'static str,
    temp: &'static str,
    side: &'static str,
    live: &'static str,
    config_kernel: &'static str,
    address: u32,
    uart: bool,
}

fn kernel_names() -> Names {
    if cfg!(feature = "board-pi5") {
        Names {
            dest: "K2712.NEW",
            temp: "K2712.TMP",
            side: "K2712.OLD",
            live: "K2712.IMG",
            config_kernel: "k2712.new",
            address: 0x200000,
            uart: false,
        }
    } else {
        Names {
            dest: "KERNEL8.NEW",
            temp: "KN8.TMP",
            side: "KN8.OLD",
            live: "KERNEL8.IMG",
            config_kernel: "kernel8.new",
            address: 0x80000,
            uart: true,
        }
    }
}

struct Xfer {
    kind: u8,
    xfer: fwupd::Transfer,
}

struct Note {
    text: [u8; 47],
    len: u8,
}

static XFER: Mutex<Option<Xfer>> = Mutex::new(None);
static RESULT: Mutex<Note> = Mutex::new(Note {
    text: [0; 47],
    len: 0,
});
static FRESH: AtomicBool = AtomicBool::new(false);

struct Fat;

impl Card for Fat {
    type Error = crate::fs::FsError;

    fn remove(&mut self, name: &str) -> Result<(), Self::Error> {
        crate::fs::delete_file(name)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), Self::Error> {
        crate::fs::rename_file(from, to)
    }

    fn exists(&mut self, name: &str) -> Result<bool, Self::Error> {
        crate::fs::file_exists(name)
    }

    fn digest(&mut self, name: &str) -> Result<fwupd::Digest, Self::Error> {
        crate::fs::digest(name)
    }
}

/// Drop temp names left by a power loss. Boot files are not in this list.
pub fn scrub() {
    for name in [
        "FWTEST.TMP",
        "FW.OLD",
        "KN8.TMP",
        "KN8.OLD",
        "K2712.TMP",
        "K2712.OLD",
        "TRYB.TMP",
        "TRYB.OLD",
    ] {
        let _ = crate::fs::delete_file(name);
    }
    if let Ok(text) = crate::fs::read_text("FWRES.TXT") {
        if text.trim() == "receiving" {
            note("interrupted");
        }
    }
}

pub fn syscall(args: [u64; 6]) -> i64 {
    match args[0] {
        1 => chunk(args[1], args[2], args[3], args[4], args[5]),
        2 => finish_user(args[1], args[2]),
        9 => finish_running(args[1]),
        3 => status_out(args[1], args[2]),
        4 => read_out(args[1], args[2], args[3], args[4]),
        5 => config(args[1], args[2], args[3], args[4], args[5]),
        6 => reboot(args[1]),
        7 => hash_out(args[1], args[2]),
        8 => preset(args[1]),
        _ => -4,
    }
}

fn chunk(kind: u64, offset: u64, total: u64, ptr: u64, len: u64) -> i64 {
    let Some(kind) = upload_kind(kind) else {
        return -4;
    };
    if len == 0 || len > CHUNK as u64 || offset > u32::MAX as u64 || total > u32::MAX as u64 {
        return -4;
    }
    let max = if kind == 1 { TEST_MAX } else { KERNEL_MAX };
    if total == 0 || total > max as u64 {
        note("too large");
        return -11;
    }
    let mut data = [0u8; CHUNK];
    if !copy_in(ptr, &mut data[..len as usize]) {
        return -2;
    }
    let names = upload_names(kind);
    let mut slot = XFER.lock();
    if offset == 0 {
        let _ = crate::fs::delete_file("FWTEST.TMP");
        let _ = crate::fs::delete_file(kernel_names().temp);
        match fwupd::Transfer::start(total as u32) {
            Ok(xfer) => *slot = Some(Xfer { kind, xfer }),
            Err(_) => {
                *slot = None;
                return -4;
            }
        }
        note("receiving");
    }
    let ready = match slot.as_ref() {
        Some(active) => active.kind == kind && active.xfer.got() == offset as u32,
        None => false,
    };
    if !ready {
        let _ = crate::fs::delete_file(names.temp);
        *slot = None;
        note("bad offset");
        return -10;
    }
    if crate::fs::write_named(names.temp, &data[..len as usize], offset != 0).is_err() {
        let _ = crate::fs::delete_file(names.temp);
        *slot = None;
        note("no card");
        return -5;
    }
    let accepted = slot
        .as_mut()
        .map(|active| active.xfer.accept(offset as u32, &data[..len as usize]).is_ok())
        .unwrap_or(false);
    if !accepted {
        let _ = crate::fs::delete_file(names.temp);
        *slot = None;
        note("bad offset");
        return -10;
    }
    0
}

fn finish_user(kind: u64, sha_ptr: u64) -> i64 {
    let mut expect = [0u8; 32];
    if !copy_in(sha_ptr, &mut expect) {
        return -2;
    }
    finish(kind, &expect)
}

fn finish_running(kind: u64) -> i64 {
    let Some(kind_u) = upload_kind(kind) else {
        return -4;
    };
    let sha = {
        let slot = XFER.lock();
        match slot.as_ref() {
            Some(active) if active.kind == kind_u => active.xfer.running_sha(),
            _ => return -10,
        }
    };
    finish(kind, &sha)
}

fn finish(kind: u64, expect: &[u8; 32]) -> i64 {
    let Some(kind) = upload_kind(kind) else {
        return -4;
    };
    let names = upload_names(kind);
    let mut slot = XFER.lock();
    let (active_kind, got, total, running) = match slot.as_ref() {
        Some(active) => (
            active.kind,
            active.xfer.got(),
            active.xfer.total(),
            active.xfer.running_sha(),
        ),
        None => {
            note("bad offset");
            return -10;
        }
    };
    if active_kind != kind || got != total {
        let _ = crate::fs::delete_file(names.temp);
        *slot = None;
        note("bad offset");
        return -10;
    }
    if running != *expect {
        let _ = crate::fs::delete_file(names.temp);
        *slot = None;
        note("checksum mismatch");
        return -6;
    }
    let dig = match crate::fs::digest(names.temp) {
        Ok(dig) => dig,
        Err(_) => {
            let _ = crate::fs::delete_file(names.temp);
            *slot = None;
            note("no card");
            return -5;
        }
    };
    if dig.len != total || dig.sha != *expect {
        let _ = crate::fs::delete_file(names.temp);
        *slot = None;
        note("checksum mismatch");
        return -6;
    }
    let image = kind == KIND_KERNEL as u8;
    match fwupd::commit(
        &mut Fat,
        names.dest,
        names.temp,
        names.side,
        total,
        expect,
        image,
    ) {
        Ok(()) => {}
        Err(CommitError::Image) => {
            *slot = None;
            note("image rejected");
            return -7;
        }
        Err(CommitError::Checksum) => {
            *slot = None;
            note("checksum mismatch");
            return -6;
        }
        Err(_) => {
            *slot = None;
            note("no card");
            return -5;
        }
    }
    *slot = None;
    if image && write_kstat(total, expect).is_err() {
        let _ = crate::fs::delete_file(names.dest);
        note("no card");
        return -5;
    }
    note("staged");
    0
}

fn status_out(ptr: u64, cap: u64) -> i64 {
    if cap < STATUS_LEN as u64 {
        return -2;
    }
    let mut buf = [0u8; STATUS_LEN];
    fill_status(&mut buf);
    if copy_out(ptr, &buf) {
        STATUS_LEN as i64
    } else {
        -2
    }
}

fn fill_status(out: &mut [u8]) {
    let names = kernel_names();
    let staged = exists(names.dest);
    let bytes = if staged {
        crate::fs::file_len(names.dest).unwrap_or(0)
    } else {
        0
    };
    let backup = exists(names.live);
    let tryboot = exists("TRYBOOT.TXT");
    let temp = exists("FWTEST.TMP") || exists(names.temp) || XFER.lock().is_some();
    put_u32(out, 0, u32::from(staged));
    put_u32(out, 4, bytes);
    put_u32(out, 8, u32::from(backup));
    put_u32(out, 12, u32::from(tryboot));
    put_u32(out, 16, u32::from(cfg!(feature = "board-pi4")));
    put_u32(out, 20, u32::from(temp));
    if staged {
        if let Some((len, sha)) = read_kstat() {
            if len == bytes {
                out[24..56].copy_from_slice(&sha);
                out[104] = 1;
            }
        }
    }
    let result = result_bytes();
    let n = result.iter().position(|byte| *byte == 0).unwrap_or(result.len());
    let n = n.min(47);
    out[56..56 + n].copy_from_slice(&result[..n]);
    copy_name(out, 105, names.dest);
    copy_name(out, 121, names.live);
}

fn read_out(kind: u64, offset: u64, ptr: u64, cap: u64) -> i64 {
    if cap == 0 || cap > CHUNK as u64 || offset > u32::MAX as u64 {
        return -4;
    }
    let Some(name) = read_name(kind) else {
        return -4;
    };
    let mut buf = [0u8; CHUNK];
    let n = match crate::fs::read_at(name, offset as u32, &mut buf[..cap as usize]) {
        Ok(n) => n,
        Err(_) => return -4,
    };
    if n == 0 {
        return 0;
    }
    if copy_out(ptr, &buf[..n]) {
        n as i64
    } else {
        -2
    }
}

fn hash_out(kind: u64, ptr: u64) -> i64 {
    let Some(name) = read_name(kind) else {
        return -4;
    };
    if kind == KIND_KERNEL {
        if let Some((len, sha)) = read_kstat() {
            if crate::fs::file_len(name).ok() == Some(len) {
                return if copy_out(ptr, &sha) { len as i64 } else { -2 };
            }
        }
        let len = crate::fs::file_len(name).unwrap_or(u32::MAX);
        if len > TEST_MAX {
            return -11;
        }
    }
    let dig = match crate::fs::digest(name) {
        Ok(dig) => dig,
        Err(_) => return -4,
    };
    if copy_out(ptr, &dig.sha) {
        dig.len as i64
    } else {
        -2
    }
}

fn config(kind: u64, ptr: u64, len: u64, commit: u64, err_ptr: u64) -> i64 {
    if len > 4096 {
        note("too large");
        return -11;
    }
    let mut raw = [0u8; 4096];
    if len > 0 && !copy_in(ptr, &mut raw[..len as usize]) {
        return -2;
    }
    let Some(text) = core::str::from_utf8(&raw[..len as usize]).ok() else {
        write_err(err_ptr, "encoding");
        note("config rejected");
        return -4;
    };
    match kind {
        KIND_BOOT => match fwupd::validate_boot(text) {
            Ok(()) => {
                if commit == 0 {
                    note("validated");
                    0
                } else {
                    publish_boot(text)
                }
            }
            Err(msg) => {
                write_err(err_ptr, msg);
                note("config rejected");
                -4
            }
        },
        KIND_JSON => store_checked(0, text, commit != 0, err_ptr),
        KIND_LOG => store_checked(1, text, commit != 0, err_ptr),
        _ => -4,
    }
}

fn preset(commit: u64) -> i64 {
    let names = kernel_names();
    let mut buf = [0u8; 256];
    let n = match fwupd::fill_tryboot(&mut buf, names.config_kernel, names.address, names.uart) {
        Ok(n) => n,
        Err(_) => {
            note("config rejected");
            return -4;
        }
    };
    let Some(text) = core::str::from_utf8(&buf[..n]).ok() else {
        return -4;
    };
    if commit == 0 {
        note("validated");
        return 0;
    }
    publish_boot(text)
}

fn reboot(tryboot: u64) -> i64 {
    if tryboot == 1 {
        if !cfg!(feature = "board-pi4") {
            note("tryboot unsupported");
            return -5;
        }
        if !tryboot_ready() {
            note("tryboot not staged");
            return -4;
        }
        note("reboot requested");
        return crate::power::request(3);
    }
    if tryboot != 0 {
        return -4;
    }
    note("reboot requested");
    crate::power::request(2)
}

fn store_checked(which: u8, text: &str, commit: bool, err_ptr: u64) -> i64 {
    if let Err(msg) = crate::config_manager::check_config_text(which, text) {
        write_err_string(err_ptr, &msg);
        note("config rejected");
        return -4;
    }
    if !commit {
        note("validated");
        return 0;
    }
    match crate::config_manager::store_config_file(which, text) {
        Ok(_) => {
            note("config stored");
            0
        }
        Err(msg) => {
            write_err_string(err_ptr, &msg);
            note("config rejected");
            -4
        }
    }
}

fn publish_boot(text: &str) -> i64 {
    let sha = fwupd::Sha256::hash(text.as_bytes());
    if crate::fs::write_named("TRYB.TMP", text.as_bytes(), false).is_err() {
        let _ = crate::fs::delete_file("TRYB.TMP");
        note("no card");
        return -5;
    }
    match fwupd::commit(
        &mut Fat,
        "TRYBOOT.TXT",
        "TRYB.TMP",
        "TRYB.OLD",
        text.len() as u32,
        &sha,
        false,
    ) {
        Ok(()) => {
            note("config stored");
            0
        }
        Err(_) => {
            note("no card");
            -5
        }
    }
}

fn tryboot_ready() -> bool {
    let names = kernel_names();
    let Ok(len) = crate::fs::file_len(names.dest) else {
        return false;
    };
    if len < 4096 {
        return false;
    }
    let Some((klen, _)) = read_kstat() else {
        return false;
    };
    if klen != len {
        return false;
    }
    let Ok(text) = crate::fs::read_text("TRYBOOT.TXT") else {
        return false;
    };
    points_at(&text, names.config_kernel)
}

fn points_at(text: &str, kernel: &str) -> bool {
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim().eq_ignore_ascii_case("kernel") {
            return value.trim().eq_ignore_ascii_case(kernel);
        }
    }
    false
}

fn write_kstat(len: u32, sha: &[u8; 32]) -> Result<(), ()> {
    let mut line = [0u8; 74];
    fwupd::hex_encode(&len.to_be_bytes(), &mut line[..8]).ok_or(())?;
    line[8] = b' ';
    fwupd::hex_encode(sha, &mut line[9..73]).ok_or(())?;
    line[73] = b'\n';
    crate::fs::write_named("KSTAT.TXT", &line, false).map_err(|_| ())
}

fn read_kstat() -> Option<(u32, [u8; 32])> {
    let mut buf = [0u8; 80];
    let n = crate::fs::read_at("KSTAT.TXT", 0, &mut buf).ok()?;
    if n < 73 || buf[8] != b' ' {
        return None;
    }
    let mut lenb = [0u8; 4];
    let mut sha = [0u8; 32];
    if fwupd::hex_decode(&buf[..8], &mut lenb)? != 4 {
        return None;
    }
    if fwupd::hex_decode(&buf[9..73], &mut sha)? != 32 {
        return None;
    }
    Some((u32::from_be_bytes(lenb), sha))
}

fn upload_kind(kind: u64) -> Option<u8> {
    match kind {
        KIND_TEST => Some(1),
        KIND_KERNEL => Some(2),
        _ => None,
    }
}

fn upload_names(kind: u8) -> Names {
    if kind == 1 {
        Names {
            dest: "FWTEST.BIN",
            temp: "FWTEST.TMP",
            side: "FW.OLD",
            live: "KERNEL8.IMG",
            config_kernel: "kernel8.new",
            address: 0,
            uart: false,
        }
    } else {
        kernel_names()
    }
}

fn read_name(kind: u64) -> Option<&'static str> {
    match kind {
        KIND_TEST => Some("FWTEST.BIN"),
        KIND_KERNEL => Some(kernel_names().dest),
        KIND_BOOT => Some("TRYBOOT.TXT"),
        _ => None,
    }
}

fn exists(name: &str) -> bool {
    crate::fs::file_exists(name).unwrap_or(false)
}

fn note(text: &str) {
    let bytes = text.as_bytes();
    let n = bytes.len().min(47);
    {
        let mut slot = RESULT.lock();
        slot.text = [0; 47];
        slot.text[..n].copy_from_slice(&bytes[..n]);
        slot.len = n as u8;
    }
    FRESH.store(true, Ordering::Release);
    let mut line = [0u8; 48];
    line[..n].copy_from_slice(&bytes[..n]);
    line[n] = b'\n';
    let _ = crate::fs::write_named("FWRES.TXT", &line[..=n], false);
}

fn result_bytes() -> [u8; 48] {
    if FRESH.load(Ordering::Acquire) {
        let slot = RESULT.lock();
        let mut out = [0u8; 48];
        let n = slot.len as usize;
        out[..n].copy_from_slice(&slot.text[..n]);
        return out;
    }
    let mut buf = [0u8; 48];
    if let Ok(n) = crate::fs::read_at("FWRES.TXT", 0, &mut buf) {
        let end = buf[..n]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == 0)
            .unwrap_or(n)
            .min(47);
        let mut out = [0u8; 48];
        out[..end].copy_from_slice(&buf[..end]);
        return out;
    }
    let mut out = [0u8; 48];
    out[..4].copy_from_slice(b"idle");
    out
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn copy_name(out: &mut [u8], at: usize, name: &str) {
    let n = name.len().min(15);
    out[at..at + n].copy_from_slice(&name.as_bytes()[..n]);
}

fn write_err(ptr: u64, msg: &str) {
    write_err_string(ptr, msg);
}

fn write_err_string(ptr: u64, msg: &str) {
    if ptr == 0 {
        return;
    }
    let mut buf = [0u8; 160];
    let n = msg.len().min(159);
    buf[..n].copy_from_slice(&msg.as_bytes()[..n]);
    let _ = copy_out(ptr, &buf);
}

fn copy_in(ptr: u64, dst: &mut [u8]) -> bool {
    if dst.is_empty() || !crate::mmu::user_slice_ok(ptr, dst.len() as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(ptr as *const u8, dst.as_mut_ptr(), dst.len());
    }
    crate::mmu::set_pan(true);
    true
}

fn copy_out(ptr: u64, src: &[u8]) -> bool {
    if src.is_empty() || !crate::mmu::user_write_ok(ptr, src.len() as u64) {
        return false;
    }
    crate::mmu::set_pan(false);
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), ptr as *mut u8, src.len());
    }
    crate::mmu::set_pan(true);
    true
}
