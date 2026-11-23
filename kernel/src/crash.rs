//! Capture a crash into reserved RAM, then reboot.
//!
//! The fault path does not take the filesystem lock and does not write the
//! card. FAT is updated on the next boot, after the record's checksum
//! matches. A direct SD write from this context could tear a directory
//! entry, so it is not attempted.
//!
//! The slot is a NOLOAD section 32 MiB into RAM. Boot zeros `.bss` and
//! `.dma` only. A watchdog reset (Pi 4) or PSCI reset (virt and Pi 5)
//! reloads the image and leaves this page. The cache is cleaned before
//! the reset so the record is in DRAM, not only in a line the reset drops.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use crate::crash_rec::{self, Draft, LoopState};

extern "C" {
    static mut __crash_start: u8;
    static __crash_end: u8;
}

static BUSY: AtomicBool = AtomicBool::new(false);
static ARMED: AtomicU8 = AtomicU8::new(0);
static AT: AtomicU64 = AtomicU64::new(0);
static RUNTIME_SAFE: AtomicBool = AtomicBool::new(false);

struct Published {
    count: u32,
    uptime_ms: u64,
    safe: u32,
    file: [u8; 16],
    summary: [u8; 96],
    text: [u8; 4096],
    text_n: usize,
    list: [u8; 128],
    list_n: usize,
}

static PUB: spin::Mutex<Published> = spin::Mutex::new(Published {
    count: 0,
    uptime_ms: 0,
    safe: 0,
    file: [0; 16],
    summary: [0; 96],
    text: [0; 4096],
    text_n: 0,
    list: [0; 128],
    list_n: 0,
});

fn slot() -> &'static mut [u8] {
    unsafe {
        let start = core::ptr::addr_of_mut!(__crash_start);
        let end = core::ptr::addr_of!(__crash_end) as usize;
        let len = end.saturating_sub(start as usize).min(crash_rec::SLOT);
        core::slice::from_raw_parts_mut(start, len)
    }
}

/// Validate a leftover record and write `CRASH/CRASHnnn.TXT`.
///
/// The list and the count always come from the files on the card. A firmware
/// reboot can drop the RAM slot, and a promote used to leave `/crash/list`
/// empty until the next crash rewrote it.
pub fn recover() {
    let raw = slot();
    let mut ram_file = [0u8; 16];
    let mut ram_sum = [0u8; 96];
    let mut ram_sum_n = 0usize;
    let mut ram_up = 0u64;
    let mut have_ram = false;
    if let Some(state) = crash_rec::load_loop(raw) {
        if state.safe != 0 {
            RUNTIME_SAFE.store(true, Ordering::Release);
            crate::disklog::line("crash loop, staying up");
        }
        if let Some(record) = crash_rec::load_record(raw) {
            have_ram = write_ram_record(
                &record,
                &mut ram_file,
                &mut ram_sum,
                &mut ram_sum_n,
                &mut ram_up,
            );
            crash_rec::store_loop(
                raw,
                LoopState {
                    streak: state.streak,
                    safe: state.safe,
                    seq: state.seq,
                },
            );
            clean(raw.as_ptr() as u64, crash_rec::SLOT);
        }
    }
    publish_card(have_ram, &ram_file, &ram_sum[..ram_sum_n], ram_up);
}

fn write_ram_record(
    record: &crash_rec::Record,
    ram_file: &mut [u8; 16],
    ram_sum: &mut [u8; 96],
    ram_sum_n: &mut usize,
    ram_up: &mut u64,
) -> bool {
    let mut text = [0u8; 4096];
    let n = crash_rec::render(&record, &mut text);
    let name = crash_rec::file_name(record.state.seq);
    let name_str = core::str::from_utf8(&name).unwrap_or("CRASH000.TXT");
    let wrote = crate::fs::save_crash(name_str, &text[..n]).is_ok();
    let mut line = [0u8; 64];
    let mut len = 0;
    len = push(&mut line, len, b"crash recovered ");
    if wrote {
        len = push(&mut line, len, &name);
    } else {
        len = push(&mut line, len, b"ram-only");
    }
    if let Ok(text) = core::str::from_utf8(&line[..len]) {
        crate::disklog::line(text);
    }
    ram_file[..name.len()].copy_from_slice(&name);
    let sum_n = record.summary_n.min(ram_sum.len());
    one_line(&record.summary[..sum_n], ram_sum, ram_sum_n);
    *ram_up = record.uptime_ms;
    let mut pub_ = PUB.lock();
    let copy = n.min(pub_.text.len());
    pub_.text[..copy].copy_from_slice(&text[..copy]);
    pub_.text_n = copy;
    true
}

fn publish_card(have_ram: bool, ram_file: &[u8; 16], ram_sum: &[u8], ram_up: u64) {
    let mut count = 0u32;
    let mut list = [0u8; 128];
    let mut list_n = 0;
    let mut newest: Option<([u8; 16], usize)> = None;
    let mut seq = 1u32;
    while seq <= crash_rec::KEEP {
        let candidate = crash_rec::file_name(seq);
        if let Ok(candidate) = core::str::from_utf8(&candidate) {
            if crate::fs::crash_exists(candidate) {
                count += 1;
                if list_n > 0 {
                    list_n = push(&mut list, list_n, b"\n");
                }
                list_n = push(&mut list, list_n, candidate.as_bytes());
                let mut name = [0u8; 16];
                let n = candidate.len().min(name.len());
                name[..n].copy_from_slice(&candidate.as_bytes()[..n]);
                newest = Some((name, n));
            }
        }
        seq += 1;
    }
    let mut file = [0u8; 16];
    let mut summary = [0u8; 96];
    let mut summary_n = 0usize;
    let mut uptime = 0u64;
    let mut body = [0u8; 4096];
    let mut body_n = 0usize;
    if have_ram {
        file = *ram_file;
        one_line(ram_sum, &mut summary, &mut summary_n);
        uptime = ram_up;
    } else if let Some((name, _)) = newest {
        file = name;
        if let Ok(name) = core::str::from_utf8(&file) {
            let end = name.find('\0').unwrap_or(name.len());
            if let Ok(n) = crate::fs::read_crash(&name[..end], &mut body) {
                body_n = n.min(body.len());
                if let Some((up, sum)) = fields_from_text(&body[..body_n]) {
                    uptime = up;
                    one_line(sum, &mut summary, &mut summary_n);
                }
            }
        }
    }
    let mut pub_ = PUB.lock();
    pub_.count = count;
    pub_.uptime_ms = uptime;
    pub_.safe = u32::from(RUNTIME_SAFE.load(Ordering::Acquire));
    pub_.file = file;
    pub_.summary = summary;
    if !have_ram {
        let copy = body_n.min(pub_.text.len());
        pub_.text[..copy].copy_from_slice(&body[..copy]);
        pub_.text_n = copy;
    }
    pub_.list = [0; 128];
    let list_copy = list_n.min(pub_.list.len());
    pub_.list[..list_copy].copy_from_slice(&list[..list_copy]);
    pub_.list_n = list_copy;
}

fn one_line(src: &[u8], out: &mut [u8], n: &mut usize) -> usize {
    *n = 0;
    for byte in src {
        if *n >= out.len() {
            break;
        }
        let byte = if *byte == b'\n' || *byte == b'\r' {
            b' '
        } else {
            *byte
        };
        if byte == 0 {
            break;
        }
        out[*n] = byte;
        *n += 1;
    }
    *n
}

fn fields_from_text(text: &[u8]) -> Option<(u64, &[u8])> {
    let uptime = line_value(text, b"uptime_ms ")
        .and_then(parse_u64)
        .unwrap_or(0);
    let summary = text
        .windows(8)
        .position(|window| window == b"summary ")
        .map(|at| {
            let rest = &text[at + 8..];
            if rest.last() == Some(&b'\n') {
                &rest[..rest.len() - 1]
            } else {
                rest
            }
        })?;
    Some((uptime, summary))
}

fn line_value<'a>(text: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0;
    while i + key.len() <= text.len() {
        if (i == 0 || text[i - 1] == b'\n') && &text[i..i + key.len()] == key {
            let rest = &text[i + key.len()..];
            let end = rest
                .iter()
                .position(|byte| *byte == b'\n')
                .unwrap_or(rest.len());
            return Some(&rest[..end]);
        }
        i += 1;
    }
    None
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut n = 0u64;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        n = n.saturating_mul(10).saturating_add((*byte - b'0') as u64);
    }
    Some(n)
}

/// HTTP task only. `kind` is 1 abort, 2 panic, 3 hang.
pub fn arm(kind: u8) -> i64 {
    if crate::percpu::slot() != crate::task::HTTP_SLOT {
        return -3;
    }
    if kind != crash_rec::KIND_ABORT as u8
        && kind != crash_rec::KIND_PANIC as u8
        && kind != crash_rec::KIND_HANG as u8
    {
        return -4;
    }
    if ARMED.swap(kind, Ordering::AcqRel) != 0 {
        return 1;
    }
    AT.store(
        crate::timer::millis().saturating_add(300),
        Ordering::Release,
    );
    0
}

pub fn poll() {
    let kind = ARMED.load(Ordering::Acquire);
    let at = AT.load(Ordering::Acquire);
    if kind == 0 || at == 0 || crate::timer::millis() < at {
        return;
    }
    if ARMED.swap(0, Ordering::AcqRel) == 0 {
        return;
    }
    match kind {
        1 => {
            let addr = 0xFFFF_0001_0000_0000u64 as *const u8;
            unsafe {
                core::ptr::read_volatile(addr);
            }
            note(crash_rec::KIND_ABORT, b"debug abort missed");
        }
        2 => panic!("debug crash"),
        _ => {
            note(crash_rec::KIND_HANG, b"debug hang");
            loop {
                unsafe {
                    core::arch::asm!("wfe", options(nomem, nostack));
                }
            }
        }
    }
}

pub fn busy() -> bool {
    BUSY.load(Ordering::Acquire)
}

pub fn from_exception(esr: u64, elr: u64, far: u64, spsr: u64, sp: u64, regs: &[u64]) {
    let ec = (esr >> 26) & 0x3f;
    let kind = if ec == 0x24 || ec == 0x25 {
        crash_rec::KIND_ABORT
    } else {
        crash_rec::KIND_FAULT
    };
    let mut summary = [0u8; 48];
    let mut n = push(
        &mut summary,
        0,
        crate::crash_rec::kind_name(kind).as_bytes(),
    );
    n = push(&mut summary, n, b" ec=");
    n = push_hex2(&mut summary, n, (ec & 0x3f) as u8);
    capture(kind, esr, elr, far, spsr, sp, regs, &summary[..n]);
}

pub fn from_panic(file: &[u8], line: u64, message: &[u8]) {
    let mut summary = [0u8; 96];
    let mut n = push(&mut summary, 0, b"panic ");
    n = push(&mut summary, n, message);
    n = push(&mut summary, n, b" ");
    let file = file.rsplit(|byte| *byte == b'/').next().unwrap_or(file);
    n = push(&mut summary, n, file);
    n = push(&mut summary, n, b":");
    n = push_dec(&mut summary, n, line);
    let sp = current_sp();
    let mut regs = [0u64; 31];
    snapshot(&mut regs);
    capture(
        crash_rec::KIND_PANIC,
        0,
        regs[30],
        0,
        0,
        sp,
        &regs,
        &summary[..n],
    );
}

pub fn note(kind: u16, summary: &[u8]) {
    let sp = current_sp();
    let mut regs = [0u64; 31];
    snapshot(&mut regs);
    capture(kind, 0, regs[30], 0, 0, sp, &regs, summary);
}

fn capture(
    kind: u16,
    esr: u64,
    elr: u64,
    far: u64,
    spsr: u64,
    sp: u64,
    regs: &[u64],
    summary: &[u8],
) {
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    crate::interrupts::mask();
    let raw = slot();
    if raw.len() < crash_rec::RECORD_LEN {
        BUSY.store(false, Ordering::Release);
        return;
    }
    let prev = crash_rec::load_loop(raw).unwrap_or(LoopState {
        streak: 0,
        safe: 0,
        seq: 0,
    });
    let uptime = crate::timer::millis();
    let decision = crash_rec::decide(prev, uptime);
    let mut log_buf = [0u8; 512];
    let log_n = crate::logger::recent_best_effort(&mut log_buf);
    let mut stack = [0u8; 256];
    let stack_n = copy_stack(sp, &mut stack);
    let mut frames = [0u64; 8];
    let fp = if regs.len() > 29 { regs[29] } else { 0 };
    let frame_n = walk_frames(fp, &mut frames);
    let draft = Draft {
        kind,
        streak: decision.streak,
        safe: decision.safe,
        seq: prev.seq.saturating_add(1),
        core: core_id(),
        uptime_ms: uptime,
        esr,
        elr,
        far,
        spsr,
        sp,
        regs,
        frames: &frames[..frame_n],
        stack: &stack[..stack_n],
        log: &log_buf[..log_n],
        summary,
        version: version::VERSION.as_bytes(),
        boot_id: version::BOOT_ID_STR.as_bytes(),
    };
    if !crash_rec::store_record(raw, &draft) {
        BUSY.store(false, Ordering::Release);
        return;
    }
    clean(raw.as_ptr() as u64, crash_rec::SLOT);
    if decision.reboot {
        raw_line(b"crash reset\r\n");
        let _ = crate::board::crash_reset();
    } else {
        crate::boot::disarm();
    }
    BUSY.store(false, Ordering::Release);
}

pub fn status(out: &mut [u8]) -> usize {
    if out.len() < 128 {
        return 0;
    }
    out[..128].fill(0);
    let pub_ = PUB.lock();
    out[0..4].copy_from_slice(&pub_.count.to_le_bytes());
    out[4..12].copy_from_slice(&pub_.uptime_ms.to_le_bytes());
    let safe = u32::from(pub_.safe != 0 || RUNTIME_SAFE.load(Ordering::Acquire));
    out[12..16].copy_from_slice(&safe.to_le_bytes());
    out[16..32].copy_from_slice(&pub_.file);
    out[32..128].copy_from_slice(&pub_.summary);
    128
}

pub fn last_text(out: &mut [u8]) -> usize {
    let pub_ = PUB.lock();
    let n = pub_.text_n.min(out.len());
    out[..n].copy_from_slice(&pub_.text[..n]);
    n
}

pub fn list_text(out: &mut [u8]) -> usize {
    let pub_ = PUB.lock();
    let n = pub_.list_n.min(out.len());
    out[..n].copy_from_slice(&pub_.list[..n]);
    n
}

fn core_id() -> u32 {
    let ptr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {ptr}, tpidr_el1",
            ptr = out(reg) ptr,
            options(nomem, nostack)
        );
    }
    if ptr == 0 {
        0
    } else {
        crate::percpu::id() as u32
    }
}

fn current_sp() -> u64 {
    let sp: u64;
    unsafe {
        core::arch::asm!("mov {sp}, sp", sp = out(reg) sp, options(nomem, nostack));
    }
    sp
}

fn snapshot(out: &mut [u64; 31]) {
    unsafe {
        core::arch::asm!(
            "stp x0, x1, [x16, #0]",
            "stp x2, x3, [x16, #16]",
            "stp x4, x5, [x16, #32]",
            "stp x6, x7, [x16, #48]",
            "stp x8, x9, [x16, #64]",
            "stp x10, x11, [x16, #80]",
            "stp x12, x13, [x16, #96]",
            "stp x14, x15, [x16, #112]",
            "stp x16, x17, [x16, #128]",
            "stp x18, x19, [x16, #144]",
            "stp x20, x21, [x16, #160]",
            "stp x22, x23, [x16, #176]",
            "stp x24, x25, [x16, #192]",
            "stp x26, x27, [x16, #208]",
            "stp x28, x29, [x16, #224]",
            "str x30, [x16, #240]",
            in("x16") out.as_mut_ptr(),
            options(nostack),
        );
    }
}

fn walk_frames(mut fp: u64, out: &mut [u64]) -> usize {
    let mut n = 0;
    let mut prev = 0u64;
    while n < out.len() {
        if fp & 0xf != 0 || fp <= prev || !safe_span(fp, 16) {
            break;
        }
        let next = read_u64(fp);
        let ret = read_u64(fp.wrapping_add(8));
        if ret == 0 {
            break;
        }
        out[n] = ret;
        n += 1;
        prev = fp;
        fp = next;
    }
    n
}

fn copy_stack(sp: u64, out: &mut [u8]) -> usize {
    let len = out.len().min(256);
    if sp & 7 != 0 || !safe_span(sp, len as u64) {
        return 0;
    }
    let mut i = 0;
    while i < len {
        out[i] = unsafe { core::ptr::read_volatile(sp.wrapping_add(i as u64) as *const u8) };
        i += 1;
    }
    len
}

fn safe_span(start: u64, len: u64) -> bool {
    if len == 0 || start & 0xFFFF_0000_0000_0000 != 0xFFFF_0000_0000_0000 {
        return false;
    }
    let end = match start.checked_add(len) {
        Some(end) => end,
        None => return false,
    };
    let phys = crate::addr::virt_to_phys(start);
    let phys_end = crate::addr::virt_to_phys(end - 1);
    if phys < crate::board::RAM_START || phys_end >= crate::board::RAM_END {
        return false;
    }
    let crash = core::ptr::addr_of!(__crash_start) as u64;
    let crash_end = crash.wrapping_add(crash_rec::SLOT as u64);
    if start < crash_end && end > crash {
        return false;
    }
    let mut page = start & !0xfff;
    while page < end {
        if page == crate::mmu::guard_page()
            || crate::task::is_kstack_guard_va(page)
            || crate::smp::is_stack_guard_va(page)
        {
            return false;
        }
        page = page.wrapping_add(0x1000);
    }
    true
}

fn read_u64(addr: u64) -> u64 {
    unsafe { core::ptr::read_volatile(addr as *const u64) }
}

fn clean(start: u64, len: usize) {
    let mut addr = start & !63;
    let end = start.wrapping_add(len as u64);
    while addr < end {
        unsafe {
            core::arch::asm!("dc cvac, {addr}", addr = in(reg) addr, options(nostack));
        }
        addr = addr.wrapping_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}

fn raw_line(bytes: &[u8]) {
    let uart = crate::addr::phys_to_virt(crate::board::UART_PHYS) as *mut u32;
    for &byte in bytes {
        unsafe {
            core::ptr::write_volatile(uart, byte as u32);
        }
    }
}

fn push(out: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    for byte in bytes {
        if n >= out.len() {
            return n;
        }
        out[n] = *byte;
        n += 1;
    }
    n
}

fn push_dec(out: &mut [u8], n: usize, mut value: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if value == 0 {
        i -= 1;
        tmp[i] = b'0';
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    push(out, n, &tmp[i..])
}

fn push_hex2(out: &mut [u8], n: usize, value: u8) -> usize {
    let digits = [hex_digit(value >> 4), hex_digit(value & 0xf)];
    push(out, n, &digits)
}

fn hex_digit(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    }
}
