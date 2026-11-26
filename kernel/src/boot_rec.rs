//! Boot-attempt counter stored beside the crash record.
//!
//! The bytes live in the same NOLOAD page the crash path uses, after the
//! crash slot. Nothing here takes a lock or writes the card. A boot that
//! never reaches `good` is a failed boot. Three of those ask for
//! `KERNEL8.BAK`. The fallback boot is not counted as another failure of
//! the live image.

pub const MAGIC: u32 = 0x544F_4F42;
pub const VERSION: u16 = 2;
pub const LEN: usize = 64;
pub const RESET_UNKNOWN: u32 = 0;
pub const RESET_WATCHDOG: u32 = 1;
pub const RESET_REBOOT: u32 = 2;
pub const RESET_POWER: u32 = 3;
pub const FAIL_LIMIT: u32 = 3;
pub const GOOD_MS: u64 = 15_000;
pub const WATCHDOG_MS: u64 = 15_000;

pub const STAGE_START: u32 = 1;
pub const STAGE_MMU: u32 = 2;
pub const STAGE_TIMER: u32 = 3;
pub const STAGE_SMP: u32 = 4;
pub const STAGE_FS: u32 = 5;
pub const STAGE_DISPLAY: u32 = 6;
pub const STAGE_NET: u32 = 7;
pub const STAGE_SERVE: u32 = 8;
pub const STAGE_GOOD: u32 = 9;

/// `fallback` 0 is a normal boot. 1 means a reset into the backup was
/// requested. 2 means this boot is that backup. 3 means the backup could
/// not be armed and this kernel keeps running.
pub const FB_NONE: u32 = 0;
pub const FB_RESET: u32 = 1;
pub const FB_ACTIVE: u32 = 2;
pub const FB_ABANDONED: u32 = 3;

const OFF_SUM: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rec {
    pub attempts: u32,
    pub stage: u32,
    pub prev_stage: u32,
    pub fallback: u32,
    pub good: u32,
    pub restored: u32,
    /// Identity of the kernel this streak belongs to. A different image
    /// starts a new streak.
    pub image: [u8; 16],
    /// Why the previous boot stopped, when that was recorded before reset.
    pub reset: u32,
}

/// How this boot relates to the kernel the record is counting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// `config.txt` loaded the same image the record counts.
    Same,
    /// `config.txt` loaded a different image.
    Other,
    /// Tryboot of `kernel8.new`, or any tryboot that is not the backup.
    Staged,
    /// Tryboot of `kernel8.bak`.
    Backup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    /// Rewrite `TRYBOOT.TXT` at the backup and reboot through tryboot.
    Fallback,
    /// Do not reset again.
    Stay,
}

/// Last stage the previous boot reached.
///
/// `good` means that boot passed the 15 second healthy mark, even if a later
/// rewrite left `stage` at `start`. `start` means it did not get past arming
/// the record. `0` is `unknown` (no previous boot). This is not the current
/// boot's progress; that is `boot_good`.
pub fn stopped_at(prev: &Rec) -> u32 {
    if prev.good != 0 || prev.stage == STAGE_GOOD {
        STAGE_GOOD
    } else if prev.stage != 0 {
        prev.stage
    } else {
        prev.prev_stage
    }
}

pub fn stage_name(stage: u32) -> &'static str {
    match stage {
        STAGE_START => "start",
        STAGE_MMU => "mmu",
        STAGE_TIMER => "timer",
        STAGE_SMP => "smp",
        STAGE_FS => "fs",
        STAGE_DISPLAY => "display",
        STAGE_NET => "net",
        STAGE_SERVE => "serve",
        STAGE_GOOD => "good",
        _ => "unknown",
    }
}

#[cfg(test)]
pub fn on_boot(prev: Option<Rec>) -> (Rec, Action) {
    reconcile(prev, Arrival::Same, [0; 16])
}

/// Fold the record and the way this image was loaded.
///
/// A tryboot of `kernel8.new`, or a normal boot of a different image, drops
/// the streak and the fallback. Rollback is allowed only for `Backup`.
pub fn reconcile(prev: Option<Rec>, arrival: Arrival, image: [u8; 16]) -> (Rec, Action) {
    let Some(prev) = prev else {
        return (fresh_image(0, image), Action::Continue);
    };
    let stopped = stopped_at(&prev);
    if arrival == Arrival::Staged || arrival == Arrival::Other {
        return (fresh_image(stopped, image), Action::Continue);
    }
    if arrival != Arrival::Backup && prev.image != [0; 16] && prev.image != image {
        return (fresh_image(stopped, image), Action::Continue);
    }
    if arrival == Arrival::Backup {
        return backup_arrival(prev, image);
    }
    if prev.restored != 0 && prev.good != 0 {
        return (fresh_image(stopped, image), Action::Continue);
    }
    if prev.good != 0 && prev.fallback < FB_ACTIVE {
        return (fresh_image(stopped, image), Action::Continue);
    }
    if prev.fallback == FB_RESET {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: stopped,
                fallback: FB_ACTIVE,
                good: 0,
                restored: 0,
                image,
                reset: 0,
            },
            Action::Stay,
        );
    }
    if prev.fallback == FB_ACTIVE && prev.restored == 0 {
        return (
            owned(prev.attempts, stopped, FB_RESET, image),
            Action::Fallback,
        );
    }
    if prev.fallback == FB_ABANDONED {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: stopped,
                fallback: FB_ABANDONED,
                good: 0,
                restored: 0,
                image,
                reset: 0,
            },
            Action::Stay,
        );
    }
    let attempts = prev.attempts.saturating_add(1);
    if attempts >= FAIL_LIMIT {
        return (owned(attempts, stopped, FB_RESET, image), Action::Fallback);
    }
    (owned(attempts, stopped, FB_NONE, image), Action::Continue)
}

fn backup_arrival(prev: Rec, image: [u8; 16]) -> (Rec, Action) {
    let stopped = stopped_at(&prev);
    if prev.fallback == FB_RESET {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: stopped,
                fallback: FB_ACTIVE,
                good: 0,
                restored: 0,
                image,
                reset: 0,
            },
            Action::Stay,
        );
    }
    if prev.fallback == FB_ACTIVE && prev.restored == 0 {
        return (
            owned(prev.attempts, stopped, FB_RESET, image),
            Action::Fallback,
        );
    }
    if prev.fallback == FB_ABANDONED {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: stopped,
                fallback: FB_ABANDONED,
                good: 0,
                restored: 0,
                image,
                reset: 0,
            },
            Action::Stay,
        );
    }
    (fresh_image(stopped, image), Action::Continue)
}

/// `true` only when this boot is the `kernel8.bak` tryboot the record asked for.
pub fn allows_rollback(rec: &Rec, arrival: Arrival) -> bool {
    arrival == Arrival::Backup
        && (rec.fallback == FB_ACTIVE || rec.fallback == FB_RESET)
        && rec.fallback != FB_ABANDONED
        && rec.restored == 0
}

pub fn image_id(bytes: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut i = 0;
    while i < bytes.len() {
        out[i % 16] ^= bytes[i].wrapping_add(i as u8);
        i += 1;
    }
    out
}

pub fn reset_name(kind: u32) -> &'static str {
    match kind {
        RESET_WATCHDOG => "watchdog",
        RESET_REBOOT => "reboot",
        RESET_POWER => "power",
        _ => "unknown",
    }
}

/// `recorded` is the value stored before a software watchdog reset.
/// `rsts` is `PM_RSTS`. Bit 5 is a watchdog full reset, bit 9 is the marker
/// this kernel writes before it asks for a reset, and bit 12 is power-on.
pub fn classify_reset(rsts: u32, recorded: u32) -> u32 {
    if recorded == RESET_WATCHDOG {
        return RESET_WATCHDOG;
    }
    if rsts & 0x200 != 0 {
        return RESET_REBOOT;
    }
    if rsts & 0x20 != 0 {
        return RESET_WATCHDOG;
    }
    if rsts & 0x1000 != 0 {
        return RESET_POWER;
    }
    RESET_UNKNOWN
}

fn owned(attempts: u32, prev_stage: u32, fallback: u32, image: [u8; 16]) -> Rec {
    Rec {
        attempts,
        stage: STAGE_START,
        prev_stage,
        fallback,
        good: 0,
        restored: 0,
        image,
        reset: 0,
    }
}

pub fn mark_good(mut rec: Rec) -> Rec {
    rec.good = 1;
    rec.attempts = 0;
    rec.stage = STAGE_GOOD;
    rec
}

/// The next reset is treated as the third failed boot.
pub fn simulate_fail(mut rec: Rec) -> Rec {
    rec.good = 0;
    rec.restored = 0;
    rec.fallback = FB_NONE;
    rec.attempts = FAIL_LIMIT - 1;
    rec
}

pub fn load(raw: &[u8]) -> Option<Rec> {
    if raw.len() < LEN || read_u32(raw, 0) != MAGIC || read_u16(raw, 4) != VERSION {
        return None;
    }
    let len = read_u32(raw, 8);
    if len as usize != LEN || read_u32(raw, OFF_SUM) != checksum(&raw[..LEN]) {
        return None;
    }
    let mut image = [0u8; 16];
    image.copy_from_slice(&raw[40..56]);
    Some(Rec {
        attempts: read_u32(raw, 16),
        stage: read_u32(raw, 20),
        prev_stage: read_u32(raw, 24),
        fallback: read_u32(raw, 28),
        good: read_u32(raw, 32),
        restored: read_u32(raw, 36),
        image,
        reset: read_u32(raw, 56),
    })
}

pub fn store(raw: &mut [u8], rec: &Rec) -> bool {
    if raw.len() < LEN {
        return false;
    }
    raw[..LEN].fill(0);
    write_u32(raw, 0, MAGIC);
    write_u16(raw, 4, VERSION);
    write_u32(raw, 8, LEN as u32);
    write_u32(raw, 16, rec.attempts);
    write_u32(raw, 20, rec.stage);
    write_u32(raw, 24, rec.prev_stage);
    write_u32(raw, 28, rec.fallback);
    write_u32(raw, 32, rec.good);
    write_u32(raw, 36, rec.restored);
    raw[40..56].copy_from_slice(&rec.image);
    write_u32(raw, 56, rec.reset);
    let sum = checksum(&raw[..LEN]);
    write_u32(raw, OFF_SUM, sum);
    true
}

#[cfg(test)]
fn fresh(prev_stage: u32) -> Rec {
    fresh_image(prev_stage, [0; 16])
}

fn fresh_image(prev_stage: u32, image: [u8; 16]) -> Rec {
    Rec {
        attempts: 0,
        stage: STAGE_START,
        prev_stage,
        fallback: FB_NONE,
        good: 0,
        restored: 0,
        image,
        reset: 0,
    }
}

pub fn checksum(raw: &[u8]) -> u32 {
    minfmt::crc32_ieee(raw, OFF_SUM..OFF_SUM + 4)
}

fn read_u16(raw: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([raw[at], raw[at + 1]])
}

fn read_u32(raw: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]])
}

fn write_u16(raw: &mut [u8], at: usize, value: u16) {
    raw[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(raw: &mut [u8], at: usize, value: u32) {
    raw[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(rec: &Rec) -> Rec {
        let mut raw = [0u8; LEN];
        assert!(store(&mut raw, rec));
        load(&raw).unwrap()
    }

    #[test]
    fn checksum_matches_crc32_ieee() {
        let mut raw = [0u8; LEN];
        assert!(store(
            &mut raw,
            &Rec {
                attempts: 2,
                stage: STAGE_FS,
                prev_stage: STAGE_NET,
                fallback: 0,
                good: 0,
                restored: 0,
                image: [0; 16],
                reset: 0,
            }
        ));
        let sum = read_u32(&raw, OFF_SUM);
        assert_eq!(sum, minfmt::crc32_ieee(&raw, OFF_SUM..OFF_SUM + 4));
        assert_eq!(sum, 0xF867_A32E);
    }

    #[test]
    fn checksum_rejects_a_flipped_byte() {
        let mut raw = [0u8; LEN];
        assert!(store(
            &mut raw,
            &Rec {
                attempts: 2,
                stage: STAGE_FS,
                prev_stage: STAGE_NET,
                fallback: 0,
                good: 0,
                restored: 0,
                image: [0; 16],
                reset: 0,
            }
        ));
        assert_eq!(load(&raw).unwrap().stage, STAGE_FS);
        raw[20] ^= 0xff;
        assert!(load(&raw).is_none());
        assert!(load(&[0; 16]).is_none());
    }

    #[test]
    fn three_failed_boots_fall_back_once() {
        let (mut rec, action) = on_boot(None);
        assert_eq!(action, Action::Continue);
        assert_eq!(rec.attempts, 0);
        rec.stage = STAGE_FS;
        rec = round(&rec);
        let (rec, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Continue);
        assert_eq!(rec.attempts, 1);
        assert_eq!(rec.prev_stage, STAGE_FS);
        let (rec, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Continue);
        assert_eq!(rec.attempts, 2);
        let (rec, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Fallback);
        assert_eq!(rec.attempts, 3);
        assert_eq!(rec.fallback, FB_RESET);
        let (rec, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Stay);
        assert_eq!(rec.fallback, FB_ACTIVE);
        assert_eq!(rec.attempts, 3);
        let dead = on_boot(Some(rec));
        assert_eq!(dead.1, Action::Fallback);
        assert_eq!(dead.0.attempts, 3);
    }

    #[test]
    fn a_good_boot_clears_the_streak_and_a_simulated_fail_does_not() {
        let (rec, _) = on_boot(None);
        let (rec, action) = on_boot(Some(mark_good(rec)));
        assert_eq!(action, Action::Continue);
        assert_eq!(rec.attempts, 0);
        let forced = round(&simulate_fail(mark_good(rec)));
        let (rec, action) = on_boot(Some(forced));
        assert_eq!(action, Action::Fallback);
        assert_eq!(rec.attempts, FAIL_LIMIT);
    }

    #[test]
    fn an_abandoned_fallback_does_not_reset_again() {
        let mut rec = fresh(0);
        rec.stage = STAGE_FS;
        rec.fallback = FB_ABANDONED;
        rec.attempts = FAIL_LIMIT;
        let (next, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Stay);
        assert_eq!(next.fallback, FB_ABANDONED);
        assert_eq!(next.attempts, FAIL_LIMIT);
        assert_eq!(stage_name(next.prev_stage), "fs");
    }

    #[test]
    fn a_restored_backup_boots_the_live_image_again() {
        let mut rec = fresh(STAGE_GOOD);
        rec.fallback = FB_ACTIVE;
        rec.good = 1;
        rec.restored = 1;
        let (next, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Continue);
        assert_eq!(next.fallback, FB_NONE);
        assert_eq!(next.attempts, 0);
        assert_eq!(stage_name(next.prev_stage), "good");
    }

    #[test]
    fn a_good_boot_stays_good_when_the_stage_word_was_reset() {
        let (rec, _) = on_boot(None);
        let mut rec = mark_good(rec);
        rec.stage = STAGE_START;
        let (next, action) = on_boot(Some(rec));
        assert_eq!(action, Action::Continue);
        assert_eq!(next.attempts, 0);
        assert_eq!(stage_name(next.prev_stage), "good");
    }

    #[test]
    fn a_staged_or_different_image_drops_the_fallback() {
        let image = image_id(b"pi4dns-boot-id:0.1.82");
        let mut rec = fresh(STAGE_FS);
        rec.stage = STAGE_FS;
        rec.attempts = 3;
        rec.fallback = FB_ACTIVE;
        rec.image = image;
        let (staged, action) = reconcile(Some(rec), Arrival::Staged, image);
        assert_eq!(action, Action::Continue);
        assert_eq!(staged.attempts, 0);
        assert_eq!(staged.fallback, FB_NONE);
        assert!(!allows_rollback(&staged, Arrival::Staged));
        let other_id = image_id(b"pi4dns-boot-id:0.1.83");
        let (other, action) = reconcile(Some(rec), Arrival::Other, other_id);
        assert_eq!(action, Action::Continue);
        assert_eq!(other.fallback, FB_NONE);
        assert_eq!(other.image, other_id);
        let (same, _) = reconcile(Some(rec), Arrival::Same, other_id);
        assert_eq!(same.fallback, FB_NONE);
        assert!(!allows_rollback(&rec, Arrival::Same));
    }

    #[test]
    fn only_the_backup_tryboot_may_roll_back() {
        let image = image_id(b"pi4dns-boot-id:0.1.83");
        let mut rec = fresh(STAGE_FS);
        rec.stage = STAGE_FS;
        rec.attempts = 3;
        rec.fallback = FB_RESET;
        rec.image = image;
        let (next, action) = reconcile(Some(rec), Arrival::Backup, image);
        assert_eq!(action, Action::Stay);
        assert_eq!(next.fallback, FB_ACTIVE);
        assert_eq!(stage_name(next.prev_stage), "fs");
        assert!(allows_rollback(&next, Arrival::Backup));
        assert!(!allows_rollback(&next, Arrival::Staged));
        assert!(!allows_rollback(&next, Arrival::Same));
        let (again, action) = reconcile(Some(next), Arrival::Backup, image);
        assert_eq!(action, Action::Fallback);
        assert_eq!(again.fallback, FB_RESET);
        let mut idle = fresh(0);
        idle.image = image;
        let (unasked, action) = reconcile(Some(idle), Arrival::Backup, image);
        assert_eq!(action, Action::Continue);
        assert!(!allows_rollback(&unasked, Arrival::Backup));
    }

    #[test]
    fn reset_cause_prefers_a_recorded_watchdog_over_the_software_marker() {
        assert_eq!(classify_reset(0, 0), RESET_UNKNOWN);
        assert_eq!(classify_reset(0x20, 0), RESET_WATCHDOG);
        assert_eq!(classify_reset(0x200, 0), RESET_REBOOT);
        assert_eq!(classify_reset(0x1000, 0), RESET_POWER);
        assert_eq!(classify_reset(0x220, RESET_WATCHDOG), RESET_WATCHDOG);
        assert_eq!(classify_reset(0, RESET_WATCHDOG), RESET_WATCHDOG);
        assert_eq!(reset_name(RESET_WATCHDOG), "watchdog");
        assert_ne!(
            image_id(b"pi4dns-boot-id:0.1.82"),
            image_id(b"pi4dns-boot-id:0.1.83")
        );
    }
}
