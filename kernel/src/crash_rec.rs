//! Crash record stored across a reset.
//!
//! The bytes live in a RAM slot the boot path does not zero. Nothing here
//! allocates, takes a lock, or talks to a disk. The next boot checks the
//! magic and the checksum, then writes the text to the card.

pub const MAGIC: u32 = 0x4853_5243;
pub const VERSION: u16 = 1;
pub const SLOT: usize = 4096;
pub const KEEP: u32 = 8;
pub const QUICK_MS: u64 = 180_000;
pub const LOOP_LIMIT: u32 = 3;

pub const KIND_ABORT: u16 = 1;
pub const KIND_PANIC: u16 = 2;
pub const KIND_HANG: u16 = 3;
pub const KIND_FAULT: u16 = 4;

const LOOP_LEN: u32 = 32;
const OFF_SUM: usize = 12;
const LOG_MAX: usize = 512;
const STACK_MAX: usize = 256;
const FRAME_MAX: usize = 8;
const SUMMARY_MAX: usize = 96;
const NAME_MAX: usize = 16;
const ID_MAX: usize = 32;

/// Full record size. A loop-only slot is [`LOOP_LEN`] bytes.
/// 1328 is the previous layout, still accepted. 1336 adds `unix_secs`.
pub const RECORD_LEN_V1: usize = 1328;
pub const RECORD_LEN: usize = 1336;

pub fn kind_name(kind: u16) -> &'static str {
    match kind {
        KIND_ABORT => "data abort",
        KIND_PANIC => "panic",
        KIND_HANG => "hang",
        KIND_FAULT => "fault",
        _ => "unknown",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopState {
    pub streak: u32,
    pub safe: u32,
    pub seq: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub streak: u32,
    pub safe: u32,
    /// Reboot unless this boot was already in the safe state.
    pub reboot: bool,
}

/// Three quick crashes arm safe mode and still reboot once, so the next
/// boot can bring the network up. A later quick crash on that boot does
/// not reboot immediately. The caller leaves the PM watchdog counting:
/// parking the CPU after disarming it leaves no network and no reset.
/// A crash after `QUICK_MS` starts the streak over and resets again.
pub fn decide(prev: LoopState, uptime_ms: u64) -> Decision {
    let quick = uptime_ms < QUICK_MS;
    let streak = if quick {
        prev.streak.saturating_add(1)
    } else {
        1
    };
    let safe = if !quick {
        0
    } else if prev.safe != 0 || streak >= LOOP_LIMIT {
        1
    } else {
        0
    };
    Decision {
        streak,
        safe,
        reboot: prev.safe == 0 || !quick,
    }
}

/// `CRASH000.TXT` .. `CRASH007.TXT`. `seq` 1 is the first file.
pub fn file_name(seq: u32) -> [u8; 12] {
    let slot = if seq == 0 { 0 } else { (seq - 1) % KEEP };
    let mut name = *b"CRASH000.TXT";
    name[7] = b'0' + (slot as u8);
    name
}

/// Next sequence for a user-fault file.
///
/// `ram_seq` is the last sequence in the RAM slot, or 0 when a reset wiped
/// that slot. `present` has bit `n` set when `CRASHnnn.TXT` is already on
/// the card. The next file follows whichever source is further ahead, so a
/// wiped slot does not overwrite `CRASH000` while later files are still
/// on the card. A full set of eight still wraps, overwriting the oldest slot.
pub fn next_seq(ram_seq: u32, present: u32) -> u32 {
    let mut last = ram_seq;
    let mut slot = 0u32;
    while slot < KEEP {
        if present & (1 << slot) != 0 {
            let seq_here = slot + 1;
            if seq_here > last {
                last = seq_here;
            }
        }
        slot += 1;
    }
    last.saturating_add(1)
}

pub struct Draft<'a> {
    pub kind: u16,
    pub streak: u32,
    pub safe: u32,
    pub seq: u32,
    pub core: u32,
    pub uptime_ms: u64,
    pub esr: u64,
    pub elr: u64,
    pub far: u64,
    pub spsr: u64,
    pub sp: u64,
    pub regs: &'a [u64],
    pub frames: &'a [u64],
    pub stack: &'a [u8],
    pub log: &'a [u8],
    pub summary: &'a [u8],
    pub version: &'a [u8],
    pub boot_id: &'a [u8],
    /// Unix seconds at the crash, or 0 when the clock was not synced.
    pub unix_secs: u64,
}

pub struct Record {
    pub state: LoopState,
    pub kind: u16,
    pub core: u32,
    pub uptime_ms: u64,
    pub esr: u64,
    pub elr: u64,
    pub far: u64,
    pub spsr: u64,
    pub sp: u64,
    pub regs: [u64; 31],
    pub frames: [u64; FRAME_MAX],
    pub frame_n: usize,
    pub stack: [u8; STACK_MAX],
    pub stack_n: usize,
    pub log: [u8; LOG_MAX],
    pub log_n: usize,
    pub summary: [u8; SUMMARY_MAX],
    pub summary_n: usize,
    pub version: [u8; NAME_MAX],
    pub version_n: usize,
    pub boot_id: [u8; ID_MAX],
    pub boot_n: usize,
    pub unix_secs: u64,
}

/// `None` when the slot is empty, short, or the checksum does not match.
pub fn load_loop(raw: &[u8]) -> Option<LoopState> {
    if raw.len() < LOOP_LEN as usize || read_u32(raw, 0) != MAGIC {
        return None;
    }
    if read_u16(raw, 4) != VERSION {
        return None;
    }
    let len = read_u32(raw, 8);
    if len < LOOP_LEN || len as usize > raw.len() {
        return None;
    }
    if len != LOOP_LEN && len as usize != RECORD_LEN && len as usize != RECORD_LEN_V1 {
        return None;
    }
    if read_u32(raw, OFF_SUM) != checksum(&raw[..len as usize]) {
        return None;
    }
    Some(LoopState {
        streak: read_u32(raw, 16),
        safe: read_u32(raw, 20),
        seq: read_u32(raw, 24),
    })
}

pub fn load_record(raw: &[u8]) -> Option<Record> {
    let state = load_loop(raw)?;
    let len = read_u32(raw, 8) as usize;
    if (len != RECORD_LEN && len != RECORD_LEN_V1) || raw.len() < len {
        return None;
    }
    let kind = read_u16(raw, 6);
    if kind_name(kind) == "unknown" {
        return None;
    }
    let frame_n = (read_u32(raw, 328) as usize).min(FRAME_MAX);
    let stack_n = (read_u32(raw, 396) as usize).min(STACK_MAX);
    let log_n = (read_u32(raw, 656) as usize).min(LOG_MAX);
    let sum_n = (read_u32(raw, 1172) as usize).min(SUMMARY_MAX);
    let ver_n = (read_u32(raw, 1272) as usize).min(NAME_MAX);
    let id_n = (read_u32(raw, 1292) as usize).min(ID_MAX);
    let mut regs = [0u64; 31];
    let mut i = 0;
    while i < 31 {
        regs[i] = read_u64(raw, 80 + i * 8);
        i += 1;
    }
    let mut frames = [0u64; FRAME_MAX];
    i = 0;
    while i < frame_n {
        frames[i] = read_u64(raw, 332 + i * 8);
        i += 1;
    }
    let mut stack = [0u8; STACK_MAX];
    stack[..stack_n].copy_from_slice(&raw[400..400 + stack_n]);
    let mut log = [0u8; LOG_MAX];
    log[..log_n].copy_from_slice(&raw[660..660 + log_n]);
    let mut summary = [0u8; SUMMARY_MAX];
    summary[..sum_n].copy_from_slice(&raw[1176..1176 + sum_n]);
    let mut version = [0u8; NAME_MAX];
    version[..ver_n].copy_from_slice(&raw[1276..1276 + ver_n]);
    let mut boot_id = [0u8; ID_MAX];
    boot_id[..id_n].copy_from_slice(&raw[1296..1296 + id_n]);
    let unix_secs = if len >= RECORD_LEN {
        read_u64(raw, RECORD_LEN_V1)
    } else {
        0
    };
    Some(Record {
        state,
        kind,
        core: read_u32(raw, 28),
        uptime_ms: read_u64(raw, 32),
        esr: read_u64(raw, 40),
        elr: read_u64(raw, 48),
        far: read_u64(raw, 56),
        spsr: read_u64(raw, 64),
        sp: read_u64(raw, 72),
        regs,
        frames,
        frame_n,
        stack,
        stack_n,
        log,
        log_n,
        summary,
        summary_n: sum_n,
        version,
        version_n: ver_n,
        boot_id,
        boot_n: id_n,
        unix_secs,
    })
}

/// Drop the crash text and restart the file sequence.
///
/// The loop streak and the safe flag stay. A later boot does not write
/// `CRASH*.TXT` from this slot. Returns false when the slot is empty.
pub fn drop_record(raw: &mut [u8]) -> bool {
    let Some(state) = load_loop(raw) else {
        return false;
    };
    store_loop(
        raw,
        LoopState {
            streak: state.streak,
            safe: state.safe,
            seq: 0,
        },
    );
    let header = LOOP_LEN as usize;
    if raw.len() > header {
        raw[header..].fill(0);
    }
    true
}

/// Write a loop-only slot. The previous crash text is gone.
pub fn store_loop(raw: &mut [u8], state: LoopState) {
    raw[..LOOP_LEN as usize].fill(0);
    write_u32(raw, 0, MAGIC);
    write_u16(raw, 4, VERSION);
    write_u32(raw, 8, LOOP_LEN);
    write_u32(raw, 16, state.streak);
    write_u32(raw, 20, state.safe);
    write_u32(raw, 24, state.seq);
    let sum = checksum(&raw[..LOOP_LEN as usize]);
    write_u32(raw, OFF_SUM, sum);
}

pub fn store_record(raw: &mut [u8], draft: &Draft<'_>) -> bool {
    if raw.len() < RECORD_LEN {
        return false;
    }
    raw[..RECORD_LEN].fill(0);
    write_u32(raw, 0, MAGIC);
    write_u16(raw, 4, VERSION);
    write_u16(raw, 6, draft.kind);
    write_u32(raw, 8, RECORD_LEN as u32);
    write_u32(raw, 16, draft.streak);
    write_u32(raw, 20, draft.safe);
    write_u32(raw, 24, draft.seq);
    write_u32(raw, 28, draft.core);
    write_u64(raw, 32, draft.uptime_ms);
    write_u64(raw, 40, draft.esr);
    write_u64(raw, 48, draft.elr);
    write_u64(raw, 56, draft.far);
    write_u64(raw, 64, draft.spsr);
    write_u64(raw, 72, draft.sp);
    let mut i = 0;
    while i < draft.regs.len() && i < 31 {
        write_u64(raw, 80 + i * 8, draft.regs[i]);
        i += 1;
    }
    let frames = draft.frames.len().min(FRAME_MAX);
    write_u32(raw, 328, frames as u32);
    i = 0;
    while i < frames {
        write_u64(raw, 332 + i * 8, draft.frames[i]);
        i += 1;
    }
    let stack_n = draft.stack.len().min(STACK_MAX);
    write_u32(raw, 396, stack_n as u32);
    raw[400..400 + stack_n].copy_from_slice(&draft.stack[..stack_n]);
    let log_n = draft.log.len().min(LOG_MAX);
    write_u32(raw, 656, log_n as u32);
    raw[660..660 + log_n].copy_from_slice(&draft.log[..log_n]);
    let sum_n = draft.summary.len().min(SUMMARY_MAX);
    write_u32(raw, 1172, sum_n as u32);
    raw[1176..1176 + sum_n].copy_from_slice(&draft.summary[..sum_n]);
    let ver_n = draft.version.len().min(NAME_MAX);
    write_u32(raw, 1272, ver_n as u32);
    raw[1276..1276 + ver_n].copy_from_slice(&draft.version[..ver_n]);
    let id_n = draft.boot_id.len().min(ID_MAX);
    write_u32(raw, 1292, id_n as u32);
    raw[1296..1296 + id_n].copy_from_slice(&draft.boot_id[..id_n]);
    write_u64(raw, RECORD_LEN_V1, draft.unix_secs);
    let sum = checksum(&raw[..RECORD_LEN]);
    write_u32(raw, OFF_SUM, sum);
    true
}

pub fn render(view: &Record, out: &mut [u8]) -> usize {
    let mut n = 0;
    n = push(out, n, b"pi4dns crash\nversion ");
    n = push(out, n, &view.version[..view.version_n]);
    n = push(out, n, b"\nboot ");
    n = push(out, n, &view.boot_id[..view.boot_n]);
    n = push(out, n, b"\nkind ");
    n = push(out, n, kind_name(view.kind).as_bytes());
    n = push(out, n, b"\nuptime_ms ");
    n = push_dec(out, n, view.uptime_ms);
    if view.unix_secs > 0 {
        let mut utc = [0u8; 20];
        let utc_n = ntp::format_utc(view.unix_secs as i64, &mut utc);
        if utc_n == 20 {
            n = push(out, n, b"\nutc ");
            n = push(out, n, &utc[..utc_n]);
        }
    }
    n = push(out, n, b"\ncore ");
    n = push_dec(out, n, view.core as u64);
    n = push(out, n, b"\nesr ");
    n = push_hex(out, n, view.esr);
    n = push(out, n, b"\nelr ");
    n = push_hex(out, n, view.elr);
    n = push(out, n, b"\nfar ");
    n = push_hex(out, n, view.far);
    n = push(out, n, b"\nspsr ");
    n = push_hex(out, n, view.spsr);
    n = push(out, n, b"\nsp ");
    n = push_hex(out, n, view.sp);
    n = push(out, n, b"\n");
    let mut i = 0;
    while i < 31 {
        n = push(out, n, b"x");
        n = push_dec(out, n, i as u64);
        n = push(out, n, b" ");
        n = push_hex(out, n, view.regs[i]);
        n = push(out, n, b"\n");
        i += 1;
    }
    n = push(out, n, b"frames\n");
    i = 0;
    while i < view.frame_n {
        n = push(out, n, b" ");
        n = push_hex(out, n, view.frames[i]);
        n = push(out, n, b"\n");
        i += 1;
    }
    n = push(out, n, b"stack\n");
    n = push_bytes(out, n, &view.stack[..view.stack_n]);
    n = push(out, n, b"\nlog\n");
    n = push(out, n, &view.log[..view.log_n]);
    if view.log_n == 0 || view.log[view.log_n - 1] != b'\n' {
        n = push(out, n, b"\n");
    }
    n = push(out, n, b"summary ");
    n = push(out, n, &view.summary[..view.summary_n]);
    n = push(out, n, b"\n");
    n
}

/// IEEE CRC-32 of `raw`, with the checksum field treated as zero.
pub fn checksum(raw: &[u8]) -> u32 {
    minfmt::crc32_ieee(raw, OFF_SUM..OFF_SUM + 4)
}

fn read_u16(raw: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([raw[at], raw[at + 1]])
}

fn read_u32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

fn read_u64(raw: &[u8], at: usize) -> u64 {
    let mut tmp = [0u8; 8];
    tmp.copy_from_slice(&raw[at..at + 8]);
    u64::from_le_bytes(tmp)
}

fn write_u16(raw: &mut [u8], at: usize, value: u16) {
    raw[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(raw: &mut [u8], at: usize, value: u32) {
    raw[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(raw: &mut [u8], at: usize, value: u64) {
    raw[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn push(out: &mut [u8], n: usize, bytes: &[u8]) -> usize {
    minfmt::push_bytes(out, n, bytes)
}

fn push_dec(out: &mut [u8], n: usize, value: u64) -> usize {
    minfmt::push_dec(out, n, value)
}

fn push_hex(out: &mut [u8], n: usize, value: u64) -> usize {
    minfmt::push_hex(out, n, value, 16)
}

fn push_bytes(out: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() {
        if i > 0 && i % 16 == 0 {
            n = push(out, n, b"\n");
        } else if i > 0 {
            n = push(out, n, b" ");
        }
        let byte = bytes[i];
        let hex = [minfmt::hex_digit(byte >> 4), minfmt::hex_digit(byte)];
        n = push(out, n, &hex);
        i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> [u8; SLOT] {
        let regs = [0x11u64, 0x22, 0x33];
        let frames = [0xaaaa_u64, 0xbbbb];
        let stack = [0xdeu8, 0xad];
        let draft = Draft {
            kind: KIND_ABORT,
            streak: 2,
            safe: 0,
            seq: 2,
            core: 1,
            uptime_ms: 1500,
            esr: 0x9600_0044,
            elr: 0x1234,
            far: 0x5678,
            spsr: 0x3c5,
            sp: 0x8000,
            regs: &regs,
            frames: &frames,
            stack: &stack,
            log: b"boot line\n",
            summary: b"data abort ec=25",
            version: b"0.1.81",
            boot_id: b"pi4dns-boot-id:0.1.81",
            unix_secs: 0,
        };
        let mut raw = [0u8; SLOT];
        assert!(store_record(&mut raw, &draft));
        raw
    }

    #[test]
    fn checksum_matches_crc32_ieee() {
        let raw = sample();
        let sum = read_u32(&raw, OFF_SUM);
        assert_eq!(
            sum,
            minfmt::crc32_ieee(&raw[..RECORD_LEN], OFF_SUM..OFF_SUM + 4)
        );
        assert_eq!(sum, checksum(&raw[..RECORD_LEN]));
        assert_eq!(sum, 0x8B6E_AE98);
    }

    #[test]
    fn checksum_rejects_a_flipped_byte_and_a_short_slot() {
        let mut raw = sample();
        let view = load_record(&raw).unwrap();
        assert_eq!(view.kind, KIND_ABORT);
        assert_eq!(view.elr, 0x1234);
        assert_eq!(view.far, 0x5678);
        assert_eq!(view.esr, 0x9600_0044);
        assert_eq!(view.core, 1);
        assert_eq!(view.uptime_ms, 1500);
        assert_eq!(view.state.streak, 2);
        assert_eq!(view.state.seq, 2);
        assert_eq!(view.regs[0], 0x11);
        assert_eq!(view.frame_n, 2);
        assert_eq!(view.frames[0], 0xaaaa);
        assert_eq!(view.frames[1], 0xbbbb);
        assert_eq!(&view.stack[..view.stack_n], &[0xde, 0xad]);
        assert_eq!(&view.log[..view.log_n], b"boot line\n");
        assert_eq!(&view.summary[..view.summary_n], b"data abort ec=25");
        raw[48] ^= 0xff;
        assert!(load_record(&raw).is_none());
        assert!(load_loop(&raw).is_none());
        assert!(load_loop(&[]).is_none());
        assert!(load_loop(&[0; 32]).is_none());
    }

    #[test]
    fn dropping_the_record_keeps_the_loop_and_forgets_the_text() {
        let mut raw = sample();
        assert!(load_record(&raw).is_some());
        assert!(drop_record(&mut raw));
        assert!(load_record(&raw).is_none());
        let state = load_loop(&raw).unwrap();
        assert_eq!(state.streak, 2);
        assert_eq!(state.safe, 0);
        assert_eq!(state.seq, 0);
        assert!(!drop_record(&mut [0; SLOT]));
    }

    #[test]
    fn names_rotate_through_eight_files() {
        assert_eq!(&file_name(1), b"CRASH000.TXT");
        assert_eq!(&file_name(8), b"CRASH007.TXT");
        assert_eq!(&file_name(9), b"CRASH000.TXT");
        assert_eq!(&file_name(0), b"CRASH000.TXT");
        assert_eq!(next_seq(0, 0), 1);
        assert_eq!(&file_name(next_seq(0, 0)), b"CRASH000.TXT");
        // RAM wiped, CRASH000..CRASH004 already on the card: write CRASH005.
        let five = (1u32 << 5) - 1;
        assert_eq!(next_seq(0, five), 6);
        assert_eq!(&file_name(next_seq(0, five)), b"CRASH005.TXT");
        assert_eq!(next_seq(5, five), 6);
        assert_eq!(next_seq(0, 1), 2);
        assert_eq!(&file_name(next_seq(0, 1)), b"CRASH001.TXT");
        assert_eq!(next_seq(0, 0xff), 9);
        assert_eq!(&file_name(next_seq(0, 0xff)), b"CRASH000.TXT");
        assert_eq!(next_seq(20, 0xff), 21);
        assert_eq!(&file_name(next_seq(20, 0xff)), b"CRASH004.TXT");
        let mut seen = [false; 8];
        let mut seq = 1;
        while seq <= 16 {
            let name = file_name(seq);
            let slot = (name[7] - b'0') as usize;
            seen[slot] = true;
            seq += 1;
        }
        assert!(seen.iter().all(|on| *on));
    }

    #[test]
    fn three_quick_crashes_reboot_once_then_stay_up() {
        let mut state = LoopState {
            streak: 0,
            safe: 0,
            seq: 0,
        };
        let first = decide(state, 1_000);
        assert!(first.reboot);
        assert_eq!(first.streak, 1);
        assert_eq!(first.safe, 0);
        state.streak = first.streak;
        let second = decide(state, 2_000);
        assert!(second.reboot);
        assert_eq!(second.streak, 2);
        state.streak = second.streak;
        let third = decide(state, 3_000);
        assert!(third.reboot);
        assert_eq!(third.streak, 3);
        assert_eq!(third.safe, 1);
        state.streak = third.streak;
        state.safe = third.safe;
        let fourth = decide(state, 4_000);
        assert!(!fourth.reboot);
        assert_eq!(fourth.safe, 1);
        let later = decide(state, QUICK_MS);
        assert!(later.reboot);
        assert_eq!(later.streak, 1);
        assert_eq!(later.safe, 0);
    }

    #[test]
    fn render_names_the_kind_and_the_link_register() {
        let raw = sample();
        let view = load_record(&raw).unwrap();
        let mut text = [0u8; 2048];
        let n = render(&view, &mut text);
        let text = core::str::from_utf8(&text[..n]).unwrap();
        assert!(text.contains("kind data abort"));
        assert!(text.contains("elr 0000000000001234"));
        assert!(text.contains("summary data abort ec=25"));
        assert!(text.contains("boot line"));
        let mut kept = [0u8; SLOT];
        store_loop(
            &mut kept,
            LoopState {
                streak: 3,
                safe: 1,
                seq: 3,
            },
        );
        assert!(load_record(&kept).is_none());
        assert_eq!(
            load_loop(&kept).unwrap(),
            LoopState {
                streak: 3,
                safe: 1,
                seq: 3,
            }
        );
    }
}
