//! Staged firmware updates on the boot FAT volume.
//!
//! The live kernel and `config.txt` are never destinations. A kernel upload
//! lands on `KERNEL8.NEW` (Pi 4 and virt) or `K2712.NEW` (Pi 5). The Pi 4
//! reboot that should try it asks the VideoCore firmware to load
//! `tryboot.txt` once. The bootloader is supposed to clear that flag
//! before `start.elf`. Pi 4 rev 1.0 and 1.1 keep it in EEPROM, because
//! their reset drops `PM_RSTS`, so a kernel that crashes first leaves
//! the next power cycle on `tryboot.txt`. This kernel samples the flag
//! and clears it before it reads the firmware log. `fw_tryboot` is 1
//! only when this boot was a tryboot. `fw_tryboot_hw` is the sampled
//! mailbox bit, so a normal boot reports 0 even while `TRYBOOT.TXT`
//! is still on the card. An upload never renames onto the live file.
//! Promote does, and only after the previous kernel is already stored as
//! `KERNEL8.BAK` and the new bytes are in a temp file.
//!
//! Pi 5 can stage a short name. Its tryboot mailbox is not wired, and the
//! real `kernel_2712.img` name does not fit 8.3, so promote and rollback
//! are refused there. Virt stages the Pi 4 names. QEMU boots with
//! `-kernel`, so a virt reboot does not execute the staged file. Promote
//! still copies the FAT files, and the test reads them back. A promote
//! moves the live kernel aside, installs the staged file from a temp name,
//! and puts the old file back if that rename fails.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use spin::Mutex;

use fwupd::{self, Card, CommitError};

/// Raw firmware chunks are 16 KiB. The hex form used by the small tests
/// stays under the 4096 byte body limit at 1024 bytes of payload.
const CHUNK: usize = 16 * 1024;
/// Read-back work one syscall will do. Small files finish in one call.
/// A multi-megabyte kernel takes several calls so `/status` keeps answering.
const PUMP_BYTES: usize = 64 * 1024;
const PUMP_MS: u64 = 40;
const TEST_MAX: u32 = 64 * 1024;
const KERNEL_MAX: u32 = 8 * 1024 * 1024;
const STATUS_LEN: usize = 224;

const KIND_TEST: u64 = 1;
const KIND_KERNEL: u64 = 2;
const KIND_BOOT: u64 = 3;
const KIND_JSON: u64 = 4;
const KIND_LOG: u64 = 5;
const KIND_LIVE: u64 = 6;
const KIND_BAK: u64 = 7;

const BOOT_UNKNOWN: u8 = 0;
const BOOT_IMG: u8 = 1;
const BOOT_NEW: u8 = 2;

const CP_COPY: u8 = 1;
const CP_VERIFY: u8 = 2;
const CP_RENAME: u8 = 3;
const OP_BACKUP: u8 = 1;
const OP_INSTALL: u8 = 2;
const OP_ROLLBACK: u8 = 3;

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

const PH_IDLE: u8 = 0;
const PH_RECV: u8 = 1;
const PH_VERIFY: u8 = 2;
const PH_STAGING: u8 = 3;
const PH_STAGED: u8 = 4;
const PH_COPY: u8 = 5;

struct Job {
    kind: u8,
    xfer: fwupd::Transfer,
    phase: u8,
    verified: u32,
    verify: fwupd::Sha256,
    head: [u8; 64],
    expect: [u8; 32],
}

static PHASE: AtomicU8 = AtomicU8::new(PH_IDLE);
static PROG: AtomicU32 = AtomicU32::new(0);
static TOTAL: AtomicU32 = AtomicU32::new(0);

/// Phase name for the stall line and `/status`. Lock-free.
pub fn phase_word() -> &'static str {
    match PHASE.load(Ordering::Acquire) {
        PH_RECV => "receiving",
        PH_VERIFY => "verifying",
        PH_STAGING => "staging",
        PH_STAGED => "staged",
        PH_COPY => "copying",
        _ => "idle",
    }
}

/// Bytes finished in the current phase, and the file size.
pub fn progress_pair() -> (u32, u32) {
    (PROG.load(Ordering::Acquire), TOTAL.load(Ordering::Acquire))
}

fn publish(phase: u8, prog: u32, total: u32) {
    PHASE.store(phase, Ordering::Release);
    PROG.store(prog, Ordering::Release);
    TOTAL.store(total, Ordering::Release);
}

struct Note {
    text: [u8; 47],
    len: u8,
}

static JOB: Mutex<Option<Job>> = Mutex::new(None);
static COPY: Mutex<Option<CopyJob>> = Mutex::new(None);
static COPY_BUSY: AtomicBool = AtomicBool::new(false);
static BOOT_SHA: Mutex<Option<[u8; 32]>> = Mutex::new(None);
static BOOTED: AtomicU8 = AtomicU8::new(BOOT_UNKNOWN);
/// This boot loaded through tryboot (device tree or the mailbox bit).
static BOOT_TRY: AtomicBool = AtomicBool::new(false);
/// Mailbox tryboot bit at the sample taken before it was cleared.
static HW_TRY: AtomicBool = AtomicBool::new(false);
static HASH: Mutex<Option<HashJob>> = Mutex::new(None);

struct CopyJob {
    op: u8,
    phase: u8,
    src: &'static str,
    temp: &'static str,
    dest: &'static str,
    side: &'static str,
    offset: u32,
    total: u32,
    image: bool,
    sha: fwupd::Sha256,
    expect: [u8; 32],
    head: [u8; 64],
}

struct HashJob {
    name: &'static str,
    offset: u32,
    total: u32,
    sha: fwupd::Sha256,
}

extern "C" {
    static BOOT_DTB: u64;
}
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
        "K2712.TMP",
        "K2712.OLD",
        "TRYB.TMP",
        "TRYB.OLD",
        "KN8B.TMP",
    ] {
        let _ = crate::fs::delete_file(name);
    }
    // A promote that died between the two renames left the live kernel
    // under the side name. Put it back before that name is dropped.
    restore_live("KERNEL8.IMG", "KN8.OLD");
    restore_live("KERNEL8.BAK", "KN8B.OLD");
    if let Ok(text) = crate::fs::read_text("FWRES.TXT") {
        if text.trim() == "receiving" || text.trim() == "verifying" {
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
        10 => promote(args[1] == 1),
        11 => rollback(),
        _ => -4,
    }
}

fn chunk(kind: u64, offset: u64, total: u64, ptr: u64, len: u64) -> i64 {
    if COPY.lock().is_some() {
        return -3;
    }
    let Some(kind) = upload_kind(kind) else {
        return -4;
    };
    if len == 0 || len > CHUNK as u64 || offset > u32::MAX as u64 || total > u32::MAX as u64 {
        return -4;
    }
    let max = if kind == 1 { TEST_MAX } else { KERNEL_MAX };
    if total == 0 || total > max as u64 {
        note("too large");
        publish(PH_IDLE, 0, 0);
        return -11;
    }
    let offset = offset as u32;
    let total = total as u32;
    let len_n = len as u32;
    let mut data = [0u8; CHUNK];
    if !copy_in(ptr, &mut data[..len as usize]) {
        return -2;
    }
    if offset == 0 {
        let names = upload_names(kind);
        io(|| {
            let _ = crate::fs::delete_file(names.temp);
            let _ = crate::fs::delete_file(kernel_names().temp);
            let _ = crate::fs::delete_file("FWTEST.TMP");
        });
        match fwupd::Transfer::start(total) {
            Ok(xfer) => {
                *JOB.lock() = Some(Job {
                    kind,
                    xfer,
                    phase: PH_RECV,
                    verified: 0,
                    verify: fwupd::Sha256::new(),
                    head: [0; 64],
                    expect: [0; 32],
                });
                publish(PH_RECV, 0, total);
                note("receiving");
            }
            Err(_) => {
                *JOB.lock() = None;
                return -4;
            }
        }
    }
    enum Step {
        Write,
        Skip,
        Reject,
    }
    let step = {
        let slot = JOB.lock();
        match slot.as_ref() {
            Some(job) if job.kind == kind && job.xfer.total() == total && job.phase != PH_RECV => {
                Step::Skip
            }
            Some(job)
                if job.kind == kind
                    && job.phase == PH_RECV
                    && job.xfer.total() == total
                    && job.xfer.got() == offset =>
            {
                Step::Write
            }
            Some(job)
                if job.kind == kind
                    && job.phase == PH_RECV
                    && job.xfer.total() == total
                    && offset < job.xfer.got()
                    && offset.saturating_add(len_n) == job.xfer.got() =>
            {
                Step::Skip
            }
            // The job is gone because the rename already finished. A retry of
            // the last chunk must not look like a fresh gap, and it must not
            // append the bytes again. `finish` reports the staged state.
            None if offset != 0
                && offset.saturating_add(len_n) == total
                && file_len_or_zero(upload_names(kind).dest) == total =>
            {
                Step::Skip
            }
            _ => Step::Reject,
        }
    };
    match step {
        Step::Reject => {
            note("bad offset");
            return -10;
        }
        Step::Skip => return 0,
        Step::Write => {}
    }
    let names = upload_names(kind);
    let wrote = io(|| crate::fs::write_named(names.temp, &data[..len as usize], offset != 0));
    if wrote.is_err() {
        io(|| {
            let _ = crate::fs::delete_file(names.temp);
        });
        *JOB.lock() = None;
        publish(PH_IDLE, 0, 0);
        note("no card");
        return -5;
    }
    let accepted = {
        let mut slot = JOB.lock();
        match slot.as_mut() {
            Some(job) if job.kind == kind && job.phase == PH_RECV && job.xfer.got() == offset => {
                job.xfer.accept(offset, &data[..len as usize]).is_ok()
            }
            Some(job) if job.kind == kind && job.xfer.got() == offset.saturating_add(len_n) => true,
            _ => false,
        }
    };
    if !accepted {
        note("bad offset");
        return -10;
    }
    let got = JOB.lock().as_ref().map(|job| job.xfer.got()).unwrap_or(0);
    publish(PH_RECV, got, total);
    0
}

fn finish_user(kind: u64, sha_ptr: u64) -> i64 {
    let mut expect = [0u8; 32];
    if !copy_in(sha_ptr, &mut expect) {
        return -2;
    }
    finish(kind, Some(expect))
}

fn finish_running(kind: u64) -> i64 {
    finish(kind, None)
}

/// One slice of read-back, then the rename when the slice finishes the file.
/// `1` means the check is still running. A repeat of the final chunk calls
/// this again and gets the same real state.
fn finish(kind: u64, expect: Option<[u8; 32]>) -> i64 {
    let Some(kind) = upload_kind(kind) else {
        return -4;
    };
    let names = upload_names(kind);
    let opened = {
        let mut slot = JOB.lock();
        match slot.as_mut() {
            Some(job) if job.kind != kind => return -10,
            Some(job) if job.phase == PH_STAGING => return 1,
            Some(job) if job.phase == PH_STAGED => return 0,
            Some(job) if job.phase == PH_VERIFY => {
                if expect.is_some_and(|sha| sha != job.expect) {
                    VerifyStart::Mismatch
                } else {
                    VerifyStart::Continue
                }
            }
            Some(job) if job.xfer.got() != job.xfer.total() => return -10,
            Some(job) => {
                let running = job.xfer.running_sha();
                let expect = expect.unwrap_or(running);
                if running != expect {
                    VerifyStart::Mismatch
                } else {
                    job.phase = PH_VERIFY;
                    job.expect = expect;
                    job.verified = 0;
                    job.verify = fwupd::Sha256::new();
                    job.head = [0; 64];
                    let total = job.xfer.total();
                    publish(PH_VERIFY, 0, total);
                    VerifyStart::Began
                }
            }
            None => VerifyStart::Absent,
        }
    };
    match opened {
        VerifyStart::Mismatch => {
            drop_job(names.temp);
            note("checksum mismatch");
            return -6;
        }
        VerifyStart::Absent => {
            return if staged_match(&names, expect) {
                let len = file_len_or_zero(names.dest);
                publish(PH_STAGED, len, len);
                note("staged");
                0
            } else {
                note("bad offset");
                -10
            };
        }
        VerifyStart::Began => note("verifying"),
        VerifyStart::Continue => {}
    }
    pump(kind, &names)
}

enum VerifyStart {
    Began,
    Continue,
    Mismatch,
    Absent,
}

fn pump(kind: u8, names: &Names) -> i64 {
    let start = crate::timer::millis();
    let mut moved = 0usize;
    let mut buf = [0u8; 4096];
    while moved < PUMP_BYTES && crate::timer::millis().saturating_sub(start) < PUMP_MS {
        let (offset, total) = {
            let slot = JOB.lock();
            let Some(job) = slot.as_ref() else {
                return -10;
            };
            if job.kind != kind || job.phase != PH_VERIFY {
                return -10;
            }
            if job.verified >= job.xfer.total() {
                break;
            }
            (job.verified, job.xfer.total())
        };
        let want = (total - offset) as usize;
        let want = want.min(buf.len()).min(PUMP_BYTES - moved);
        if want == 0 {
            break;
        }
        let read = io(|| crate::fs::read_at(names.temp, offset, &mut buf[..want]));
        let n = match read {
            Ok(0) => {
                drop_job(names.temp);
                note("checksum mismatch");
                return -6;
            }
            Ok(n) => n,
            Err(_) => {
                drop_job(names.temp);
                note("no card");
                return -5;
            }
        };
        let mut slot = JOB.lock();
        let Some(job) = slot.as_mut() else {
            return -10;
        };
        if job.verified != offset {
            return 1;
        }
        if job.verified < 64 {
            let have = job.verified as usize;
            let take = (64 - have).min(n);
            job.head[have..have + take].copy_from_slice(&buf[..take]);
        }
        job.verify.update(&buf[..n]);
        job.verified = job.verified.saturating_add(n as u32);
        publish(PH_VERIFY, job.verified, total);
        moved += n;
        if job.verified >= total {
            break;
        }
    }
    let complete = JOB
        .lock()
        .as_ref()
        .map(|job| job.kind == kind && job.phase == PH_VERIFY && job.verified >= job.xfer.total())
        .unwrap_or(false);
    if !complete {
        return 1;
    }
    commit_verified(kind, names)
}

fn commit_verified(kind: u8, names: &Names) -> i64 {
    let (total, expect, sha, head) = {
        let mut slot = JOB.lock();
        let Some(job) = slot.as_mut() else {
            return -10;
        };
        if job.phase != PH_VERIFY {
            return 1;
        }
        job.phase = PH_STAGING;
        (job.xfer.total(), job.expect, job.verify.finish(), job.head)
    };
    publish(PH_STAGING, total, total);
    if sha != expect {
        drop_job(names.temp);
        note("checksum mismatch");
        return -6;
    }
    let dig = fwupd::Digest {
        len: total,
        sha,
        head,
    };
    let image = kind == KIND_KERNEL as u8;
    let committed = io(|| {
        fwupd::commit_digest(
            &mut Fat, names.dest, names.temp, names.side, total, &expect, image, dig,
        )
    });
    match committed {
        Ok(()) => {}
        Err(CommitError::Image) => {
            *JOB.lock() = None;
            publish(PH_IDLE, 0, 0);
            note("image rejected");
            return -7;
        }
        Err(CommitError::Checksum) => {
            *JOB.lock() = None;
            publish(PH_IDLE, 0, 0);
            note("checksum mismatch");
            return -6;
        }
        Err(_) => {
            *JOB.lock() = None;
            publish(PH_IDLE, 0, 0);
            note("no card");
            return -5;
        }
    }
    *JOB.lock() = None;
    if image && io(|| write_kstat(total, &expect)).is_err() {
        io(|| {
            let _ = crate::fs::delete_file(names.dest);
        });
        publish(PH_IDLE, 0, 0);
        note("no card");
        return -5;
    }
    publish(PH_STAGED, total, total);
    note("staged");
    0
}

fn staged_match(names: &Names, expect: Option<[u8; 32]>) -> bool {
    let Ok(len) = io(|| crate::fs::file_len(names.dest)) else {
        return false;
    };
    if len == 0 {
        return false;
    }
    if names.dest == kernel_names().dest {
        let Some((klen, sha)) = io(read_kstat) else {
            return false;
        };
        if klen != len {
            return false;
        }
        return expect.map(|want| want == sha).unwrap_or(true);
    }
    let Some(expect) = expect else {
        return true;
    };
    match io(|| crate::fs::digest(names.dest)) {
        Ok(dig) => dig.len == len && dig.sha == expect,
        Err(_) => false,
    }
}

fn file_len_or_zero(name: &str) -> u32 {
    io(|| crate::fs::file_len(name)).unwrap_or(0)
}

fn drop_job(temp: &str) {
    *JOB.lock() = None;
    publish(PH_IDLE, 0, 0);
    io(|| {
        let _ = crate::fs::delete_file(temp);
    });
}

fn io<R>(body: impl FnOnce() -> R) -> R {
    #[cfg(feature = "board-pi4")]
    crate::act::note_core(crate::percpu::id());
    crate::interrupts::with_irqs(body)
}

fn status_out(ptr: u64, cap: u64) -> i64 {
    if cap < STATUS_LEN as u64 {
        return -2;
    }
    advance();
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
    let tryboot_file = exists("TRYBOOT.TXT");
    let temp = exists("FWTEST.TMP") || exists(names.temp) || JOB.lock().is_some();
    put_u32(out, 0, u32::from(staged));
    put_u32(out, 4, bytes);
    put_u32(out, 8, u32::from(backup));
    put_u32(out, 12, u32::from(BOOT_TRY.load(Ordering::Relaxed)));
    put_u32(out, 16, u32::from(HW_TRY.load(Ordering::Relaxed)));
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
    let n = result
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(result.len());
    let n = n.min(47);
    out[56..56 + n].copy_from_slice(&result[..n]);
    copy_name(out, 105, names.dest);
    copy_name(out, 121, names.live);
    let mut phase = phase_word();
    let result_staged = result.starts_with(b"staged");
    if phase == "idle" && result_staged {
        phase = "staged";
    }
    let n = phase.len().min(14);
    out[137..137 + n].copy_from_slice(&phase.as_bytes()[..n]);
    let (mut prog, mut total) = progress_pair();
    if phase == "staged" && total == 0 {
        prog = bytes;
        total = bytes;
    }
    put_u32(out, 152, prog);
    put_u32(out, 156, total);
    copy_name(out, 160, booted_name());
    if let Some(sha) = *BOOT_SHA.lock() {
        out[176] = 1;
        out[177..209].copy_from_slice(&sha);
    }
    put_u32(
        out,
        212,
        u32::from(exists("KERNEL8.BAK") || exists("K2712.BAK")),
    );
    put_u32(out, 216, u32::from(tryboot_file));
}

/// Keep a verify or a promote moving when the page polls status.
fn advance() {
    if COPY.lock().is_some() {
        pump_copy();
        return;
    }
    pump_boot_hash();
    let kind = {
        let slot = JOB.lock();
        match slot.as_ref() {
            Some(job) if job.phase == PH_VERIFY => job.kind,
            _ => return,
        }
    };
    let names = upload_names(kind);
    let _ = pump(kind, &names);
}

/// One slice of promote, rollback, or the boot hash. The idle loop calls
/// this so the copy does not depend on the next HTTP poll, and each slice
/// stays inside the stall budget.
pub fn poll() {
    if COPY.lock().is_some() {
        pump_copy();
    } else {
        pump_boot_hash();
    }
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
    if kind == KIND_KERNEL || kind == KIND_LIVE {
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
    if kind == KIND_BAK {
        if let Some((len, sha)) = read_bstat() {
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
        KIND_LIVE => Some(kernel_names().live),
        KIND_BAK => Some(bak_name()),
        _ => None,
    }
}

fn exists(name: &str) -> bool {
    crate::fs::file_exists(name).unwrap_or(false)
}

/// The one-shot flag could not be cleared. The bootloader, with that flag
/// set and no `tryboot.txt`, stays on the rainbow and never loads a kernel.
/// Rewrite the file in place so it boots `kernel8.img`. The name stays.
fn install_safe_tryboot() -> bool {
    let mut raw = [0u8; 512];
    if let Ok(n) = crate::fs::read_at("CONFIG.TXT", 0, &mut raw) {
        if (1..=512).contains(&n) {
            if let Ok(text) = core::str::from_utf8(&raw[..n]) {
                if points_at(text, "kernel8.img") {
                    return crate::fs::write_named("TRYBOOT.TXT", &raw[..n], false).is_ok();
                }
            }
        }
    }
    let mut out = [0u8; 96];
    let Ok(n) = fwupd::fill_tryboot(&mut out, "kernel8.img", 0x80000, true) else {
        return false;
    };
    crate::fs::write_named("TRYBOOT.TXT", &out[..n], false).is_ok()
}

fn restore_live(live: &str, side: &str) {
    if exists(live) {
        let _ = crate::fs::delete_file(side);
        return;
    }
    if exists(side) {
        let _ = crate::fs::rename_file(side, live);
    }
}

fn bak_name() -> &'static str {
    if cfg!(feature = "board-pi5") {
        "K2712.BAK"
    } else {
        "KERNEL8.BAK"
    }
}

fn pi5_promote() -> bool {
    cfg!(feature = "board-pi5")
}

/// Record which kernel the firmware started and replace a leftover
/// `reboot requested` result. Called once, after the card is mounted.
pub fn boot_report() {
    let names = kernel_names();
    let pi4 = cfg!(feature = "board-pi4");
    let _keep_id = version::BOOT_ID[0];
    let dt = dtb_tryboot();
    // Sampled before the firmware-log read, then cleared. A later
    // GET_REBOOT_FLAGS is 0 even when this boot was a tryboot.
    let latched = if pi4 {
        crate::board::tryboot_latched()
    } else {
        0
    };
    let mbox_bit = latched & 1 != 0;
    let dt_hint = if dt.is_some() {
        dt
    } else if mbox_bit {
        Some(true)
    } else {
        None
    };
    BOOT_TRY.store(dt == Some(true) || mbox_bit, Ordering::Relaxed);
    HW_TRY.store(mbox_bit, Ordering::Relaxed);
    if pi4 && !crate::board::tryboot_disarmed() {
        if install_safe_tryboot() {
            crate::disklog::line("tryboot file kernel8.img");
        } else {
            crate::disklog::line("tryboot file stuck");
        }
    }
    let (img_id, new_id) = if pi4 {
        let img = tail_has_boot_id(names.live);
        let newer = tail_has_boot_id(names.dest);
        (img, newer)
    } else {
        (None, None)
    };
    let which = if pi4 {
        match fwupd::choose_booted(img_id, new_id, dt_hint) {
            fwupd::BootPick::Img => BOOT_IMG,
            fwupd::BootPick::New => BOOT_NEW,
            fwupd::BootPick::Unknown => BOOT_UNKNOWN,
        }
    } else {
        BOOT_UNKNOWN
    };
    BOOTED.store(which, Ordering::Relaxed);
    let name = match which {
        BOOT_IMG => "kernel8.img",
        BOOT_NEW => "kernel8.new",
        _ => "unknown",
    };
    crate::disklog::line(match dt {
        Some(true) => "tryboot dt 1",
        Some(false) => "tryboot dt 0",
        None => "tryboot dt none",
    });
    crate::disklog::line(if !pi4 {
        "tryboot mbox skipped"
    } else if mbox_bit {
        "tryboot mbox 1"
    } else if crate::board::tryboot_disarmed() {
        "tryboot mbox 0"
    } else {
        "tryboot mbox none"
    });
    if pi4 {
        crate::disklog::line(id_line(img_id, new_id));
    }
    let mut line = [0u8; 32];
    let prefix = b"boot ";
    line[..prefix.len()].copy_from_slice(prefix);
    let n = name.len().min(line.len() - prefix.len());
    line[prefix.len()..prefix.len() + n].copy_from_slice(&name.as_bytes()[..n]);
    if let Ok(text) = core::str::from_utf8(&line[..prefix.len() + n]) {
        crate::disklog::line(text);
    }
    if let Ok(text) = crate::fs::read_text("FWRES.TXT") {
        if text.trim() == "reboot requested" {
            let word = if which == BOOT_NEW {
                "tryboot boot"
            } else if which == BOOT_IMG || !pi4 {
                "normal boot"
            } else {
                "boot unknown"
            };
            note(word);
        }
    }
    let hash_name = if which == BOOT_NEW {
        Some(names.dest)
    } else if which == BOOT_IMG {
        Some(names.live)
    } else {
        None
    };
    if let Some(name) = hash_name {
        if let Ok(total) = crate::fs::file_len(name) {
            if total > 0 && total <= KERNEL_MAX {
                *HASH.lock() = Some(HashJob {
                    name,
                    offset: 0,
                    total,
                    sha: fwupd::Sha256::new(),
                });
            }
        }
    }
}

pub fn booted_name() -> &'static str {
    match BOOTED.load(Ordering::Relaxed) {
        BOOT_IMG => "kernel8.img",
        BOOT_NEW => "kernel8.new",
        _ => "unknown",
    }
}

/// Flattened device tree the firmware passed in, if the pointer is a tree
/// in RAM. Used for `/chosen/log`.
#[cfg(feature = "board-pi4")]
pub fn dtb_bytes() -> Option<&'static [u8]> {
    let pa = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(BOOT_DTB)) };
    dtb_slice(pa).or_else(|| {
        if cfg!(feature = "board-pi4") {
            const PROBE: u64 = 0x80000 + 0x800000;
            if pa != PROBE {
                return dtb_slice(PROBE);
            }
        }
        None
    })
}

#[cfg(feature = "board-pi4")]
fn dtb_slice(pa: u64) -> Option<&'static [u8]> {
    if pa == 0 || pa & 3 != 0 {
        return None;
    }
    let ram0 = crate::board::RAM_START;
    let ram1 = crate::board::RAM_END;
    if pa < ram0 || pa.saturating_add(40) > ram1 {
        return None;
    }
    let ptr = crate::addr::phys_to_virt(pa) as *const u8;
    let hdr = unsafe { core::slice::from_raw_parts(ptr, 40) };
    if hdr[0..4] != [0xd0, 0x0d, 0xfe, 0xed] {
        return None;
    }
    let total = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
    if total < 40 || total > 256 * 1024 || pa.saturating_add(total) > ram1 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(ptr, total as usize) })
}

fn dtb_tryboot() -> Option<bool> {
    let pa = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(BOOT_DTB)) };
    log_hex("dtb x0", pa);
    match magic_at(pa) {
        Some(magic) => log_hex32("dtb magic", magic),
        None => crate::disklog::line("dtb magic unread"),
    }
    if let Some(flag) = parse_dtb_at(pa) {
        return Some(flag);
    }
    // The arm64 header reserves 8 MiB at 0x80000. The firmware loads the
    // DTB after that reservation. The Pi stub often clears x0, so look there
    // when the entry pointer is not a tree. Other boards must not touch it.
    if cfg!(feature = "board-pi4") {
        const PROBE: u64 = 0x80000 + 0x800000;
        if pa != PROBE {
            if let Some(flag) = parse_dtb_at(PROBE) {
                log_hex("dtb probe", PROBE);
                return Some(flag);
            }
            crate::disklog::line("dtb probe miss");
        }
    }
    None
}

fn parse_dtb_at(pa: u64) -> Option<bool> {
    if pa == 0 || pa & 3 != 0 {
        return None;
    }
    let ram0 = crate::board::RAM_START;
    let ram1 = crate::board::RAM_END;
    if pa < ram0 || pa.saturating_add(40) > ram1 {
        return None;
    }
    let ptr = crate::addr::phys_to_virt(pa) as *const u8;
    let hdr = unsafe { core::slice::from_raw_parts(ptr, 40) };
    if hdr[0..4] != [0xd0, 0x0d, 0xfe, 0xed] {
        return None;
    }
    let total = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
    if total < 40 || total > 256 * 1024 || pa.saturating_add(total) > ram1 {
        return None;
    }
    let blob = unsafe { core::slice::from_raw_parts(ptr, total as usize) };
    fwupd::bootloader_tryboot(blob)
}

fn magic_at(pa: u64) -> Option<u32> {
    if pa == 0 || pa & 3 != 0 {
        return None;
    }
    let ram0 = crate::board::RAM_START;
    let ram1 = crate::board::RAM_END;
    if pa < ram0 || pa.saturating_add(4) > ram1 {
        return None;
    }
    let ptr = crate::addr::phys_to_virt(pa) as *const u8;
    let hdr = unsafe { core::slice::from_raw_parts(ptr, 4) };
    Some(u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]))
}

fn log_hex(prefix: &str, value: u64) {
    log_hex_bytes(prefix, &value.to_be_bytes());
}

fn log_hex32(prefix: &str, value: u32) {
    log_hex_bytes(prefix, &value.to_be_bytes());
}

fn log_hex_bytes(prefix: &str, bytes: &[u8]) {
    let mut raw = [0u8; 64];
    let pb = prefix.as_bytes();
    let n = pb.len().min(20);
    raw[..n].copy_from_slice(&pb[..n]);
    raw[n] = b' ';
    let mut hex = [0u8; 16];
    let Some(digits) = fwupd::hex_encode(bytes, &mut hex) else {
        return;
    };
    raw[n + 1..n + 1 + digits].copy_from_slice(&hex[..digits]);
    if let Ok(text) = core::str::from_utf8(&raw[..n + 1 + digits]) {
        crate::disklog::line(text);
    }
}

fn id_line(img: Option<bool>, new_file: Option<bool>) -> &'static str {
    match (img, new_file) {
        (Some(false), Some(true)) | (None, Some(true)) => "boot id new",
        (Some(true), Some(false) | None) => "boot id img",
        (Some(true), Some(true)) => "boot id both",
        (Some(false), Some(false)) => "boot id neither",
        _ => "boot id unread",
    }
}

fn tail_has_boot_id(name: &str) -> Option<bool> {
    let len = io(|| crate::fs::file_len(name)).ok()?;
    if len == 0 || len > KERNEL_MAX {
        return Some(false);
    }
    let take = len.min(4096);
    let start = len - take;
    let mut buf = [0u8; 4096];
    let got = io(|| crate::fs::read_at(name, start, &mut buf[..take as usize])).ok()?;
    if got == 0 {
        return Some(false);
    }
    // The image stores the id NUL-terminated, so 0.1.65 does not match 0.1.650.
    let id = version::BOOT_ID_STR.as_bytes();
    let mut needle = [0u8; 32];
    if id.len() + 1 > needle.len() {
        return Some(false);
    }
    needle[..id.len()].copy_from_slice(id);
    Some(fwupd::contains_bytes(&buf[..got], &needle[..id.len() + 1]))
}

fn promote(force: bool) -> i64 {
    if pi5_promote() {
        return -14;
    }
    if COPY.lock().is_some() || JOB.lock().is_some() {
        return -3;
    }
    let names = kernel_names();
    if !exists(names.dest) {
        note("not staged");
        return -4;
    }
    let from_tryboot = cfg!(feature = "board-pi4") && BOOTED.load(Ordering::Relaxed) == BOOT_NEW;
    if !from_tryboot && !force {
        note("needs tryboot");
        return -12;
    }
    let Some(len) = crate::fs::file_len(names.live).ok() else {
        note("no card");
        return -5;
    };
    start_copy(CopyJob {
        op: OP_BACKUP,
        phase: CP_COPY,
        src: names.live,
        temp: "KN8B.TMP",
        dest: "KERNEL8.BAK",
        side: "KN8B.OLD",
        offset: 0,
        total: len,
        image: false,
        sha: fwupd::Sha256::new(),
        expect: [0; 32],
        head: [0; 64],
    });
    0
}

fn rollback() -> i64 {
    if pi5_promote() {
        return -14;
    }
    if COPY.lock().is_some() || JOB.lock().is_some() {
        return -3;
    }
    let bak = bak_name();
    let Some(len) = crate::fs::file_len(bak).ok() else {
        note("no backup");
        return -13;
    };
    start_copy(CopyJob {
        op: OP_ROLLBACK,
        phase: CP_COPY,
        src: bak,
        temp: "KN8.TMP",
        dest: kernel_names().live,
        side: "KN8.OLD",
        offset: 0,
        total: len,
        image: false,
        sha: fwupd::Sha256::new(),
        expect: [0; 32],
        head: [0; 64],
    });
    0
}

fn start_copy(job: CopyJob) {
    let total = job.total;
    let label = match job.op {
        OP_ROLLBACK => "rolling back",
        _ => "promoting",
    };
    let _ = crate::fs::delete_file(job.temp);
    *COPY.lock() = Some(job);
    publish(PH_COPY, 0, total);
    note(label);
}

fn pump_copy() {
    if COPY_BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let start = crate::timer::millis();
    let mut moved = 0usize;
    let mut buf = [0u8; 4096];
    while moved < PUMP_BYTES && crate::timer::millis().saturating_sub(start) < PUMP_MS {
        let plan = {
            let slot = COPY.lock();
            let Some(job) = slot.as_ref() else {
                COPY_BUSY.store(false, Ordering::Release);
                return;
            };
            if job.phase == CP_RENAME {
                CopyPlan::Rename
            } else if job.offset >= job.total {
                CopyPlan::Advance
            } else {
                let name = if job.phase == CP_COPY {
                    job.src
                } else {
                    job.temp
                };
                CopyPlan::Io {
                    phase: job.phase,
                    name,
                    temp: job.temp,
                    op: job.op,
                    offset: job.offset,
                    n: (job.total - job.offset).min(buf.len() as u32) as usize,
                }
            }
        };
        match plan {
            CopyPlan::Rename => {
                let Some(job) = COPY.lock().take() else {
                    break;
                };
                finish_copy(job);
                COPY_BUSY.store(false, Ordering::Release);
                return;
            }
            CopyPlan::Advance => {
                if !advance_copy() {
                    COPY_BUSY.store(false, Ordering::Release);
                    return;
                }
            }
            CopyPlan::Io {
                phase,
                name,
                temp,
                op,
                offset,
                n,
            } => {
                let read = io(|| crate::fs::read_at(name, offset, &mut buf[..n]));
                let got = match read {
                    Ok(0) | Err(_) => {
                        fail_copy(op);
                        COPY_BUSY.store(false, Ordering::Release);
                        return;
                    }
                    Ok(got) => got,
                };
                if phase == CP_COPY
                    && io(|| crate::fs::write_named(temp, &buf[..got], true)).is_err()
                {
                    fail_copy(op);
                    COPY_BUSY.store(false, Ordering::Release);
                    return;
                }
                if !apply_copy(phase, offset, &buf[..got]) {
                    break;
                }
                moved += got;
            }
        }
    }
    COPY_BUSY.store(false, Ordering::Release);
}

enum CopyPlan {
    Rename,
    Advance,
    Io {
        phase: u8,
        name: &'static str,
        temp: &'static str,
        op: u8,
        offset: u32,
        n: usize,
    },
}

fn advance_copy() -> bool {
    let mut slot = COPY.lock();
    let Some(job) = slot.as_mut() else {
        return false;
    };
    if job.offset < job.total {
        return true;
    }
    if job.phase == CP_COPY {
        job.expect = job.sha.finish();
        job.sha = fwupd::Sha256::new();
        job.offset = 0;
        job.phase = CP_VERIFY;
        return true;
    }
    if job.phase == CP_VERIFY {
        let sha = job.sha.finish();
        if sha != job.expect {
            let temp = job.temp;
            let text = fail_text(job.op);
            *slot = None;
            drop(slot);
            io(|| {
                let _ = crate::fs::delete_file(temp);
            });
            publish(PH_IDLE, 0, 0);
            note(text);
            return false;
        }
        job.phase = CP_RENAME;
    }
    true
}

fn apply_copy(phase: u8, offset: u32, data: &[u8]) -> bool {
    let mut slot = COPY.lock();
    let Some(job) = slot.as_mut() else {
        return false;
    };
    if job.phase != phase || job.offset != offset {
        return true;
    }
    if phase == CP_COPY && job.offset < 64 {
        let at = job.offset as usize;
        let take = data.len().min(64 - at);
        job.head[at..at + take].copy_from_slice(&data[..take]);
    }
    job.sha.update(data);
    job.offset += data.len() as u32;
    let total = if phase == CP_COPY {
        job.total
    } else {
        job.total.saturating_mul(2)
    };
    let prog = if phase == CP_COPY {
        job.offset
    } else {
        job.total + job.offset
    };
    publish(PH_COPY, prog, total);
    true
}

fn fail_copy(op: u8) {
    let temp = COPY.lock().take().map(|job| job.temp);
    publish(PH_IDLE, 0, 0);
    if let Some(temp) = temp {
        io(|| {
            let _ = crate::fs::delete_file(temp);
        });
    }
    note(fail_text(op));
}

fn finish_copy(job: CopyJob) {
    let dig = fwupd::Digest {
        len: job.total,
        sha: job.expect,
        head: job.head,
    };
    let renamed = io(|| {
        fwupd::commit_replacing(
            &mut Fat,
            job.dest,
            job.temp,
            job.side,
            job.total,
            &job.expect,
            job.image,
            dig,
        )
    });
    if renamed.is_err() {
        publish(PH_IDLE, 0, 0);
        note(fail_text(job.op));
        return;
    }
    match job.op {
        OP_BACKUP => {
            let _ = io(|| write_bstat(job.total, &job.expect));
            let names = kernel_names();
            let Some(len) = crate::fs::file_len(names.dest).ok() else {
                publish(PH_IDLE, 0, 0);
                note("promote failed");
                return;
            };
            start_copy(CopyJob {
                op: OP_INSTALL,
                phase: CP_COPY,
                src: names.dest,
                temp: names.temp,
                dest: names.live,
                side: names.side,
                offset: 0,
                total: len,
                image: true,
                sha: fwupd::Sha256::new(),
                expect: [0; 32],
                head: [0; 64],
            });
        }
        OP_INSTALL => {
            let _ = io(|| write_kstat(job.total, &job.expect));
            io(|| {
                let _ = install_safe_tryboot();
                let _ = crate::fs::delete_file(kernel_names().dest);
            });
            *COPY.lock() = None;
            publish(PH_IDLE, job.total, job.total);
            note("promoted");
        }
        OP_ROLLBACK => {
            let _ = io(|| write_kstat(job.total, &job.expect));
            *COPY.lock() = None;
            publish(PH_IDLE, job.total, job.total);
            note("rolled back");
        }
        _ => {
            *COPY.lock() = None;
            publish(PH_IDLE, 0, 0);
            note("promote failed");
        }
    }
}

fn fail_text(op: u8) -> &'static str {
    if op == OP_ROLLBACK {
        "rollback failed"
    } else {
        "promote failed"
    }
}

fn pump_boot_hash() {
    let start = crate::timer::millis();
    let mut moved = 0usize;
    let mut buf = [0u8; 4096];
    while moved < PUMP_BYTES && crate::timer::millis().saturating_sub(start) < PUMP_MS {
        let planned = {
            let mut slot = HASH.lock();
            let Some(job) = slot.as_mut() else {
                return;
            };
            if job.offset >= job.total {
                let sha = job.sha.finish();
                *BOOT_SHA.lock() = Some(sha);
                *slot = None;
                return;
            }
            let n = (job.total - job.offset).min(buf.len() as u32) as usize;
            (job.name, job.offset, n)
        };
        let (name, offset, n) = planned;
        let got = io(|| crate::fs::read_at(name, offset, &mut buf[..n]));
        let mut slot = HASH.lock();
        let Some(job) = slot.as_mut() else {
            return;
        };
        let Ok(got) = got else {
            *slot = None;
            return;
        };
        if got == 0 || job.offset != offset {
            if got == 0 {
                *slot = None;
            }
            return;
        }
        job.sha.update(&buf[..got]);
        job.offset += got as u32;
        moved += got;
    }
}

fn write_bstat(len: u32, sha: &[u8; 32]) -> Result<(), ()> {
    write_stat("BSTAT.TXT", len, sha)
}

fn read_bstat() -> Option<(u32, [u8; 32])> {
    read_stat("BSTAT.TXT")
}

fn write_stat(name: &str, len: u32, sha: &[u8; 32]) -> Result<(), ()> {
    let mut line = [0u8; 74];
    fwupd::hex_encode(&len.to_be_bytes(), &mut line[..8]).ok_or(())?;
    line[8] = b' ';
    fwupd::hex_encode(sha, &mut line[9..73]).ok_or(())?;
    line[73] = b'\n';
    crate::fs::write_named(name, &line, false).map_err(|_| ())
}

fn read_stat(name: &str) -> Option<(u32, [u8; 32])> {
    let mut buf = [0u8; 80];
    let n = crate::fs::read_at(name, 0, &mut buf).ok()?;
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
