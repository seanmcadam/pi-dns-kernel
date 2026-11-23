//! Boot-attempt counter stored beside the crash record.
//!
//! The bytes live in the same NOLOAD page the crash path uses, after the
//! crash slot. Nothing here takes a lock or writes the card. A boot that
//! never reaches `good` is a failed boot. Three of those ask for
//! `KERNEL8.BAK`. The fallback boot is not counted as another failure of
//! the live image.

pub const MAGIC: u32 = 0x544F_4F42;
pub const VERSION: u16 = 1;
pub const LEN: usize = 64;
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Continue,
    /// Rewrite `TRYBOOT.TXT` at the backup and reboot through tryboot.
    Fallback,
    /// Do not reset again.
    Stay,
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

pub fn on_boot(prev: Option<Rec>) -> (Rec, Action) {
    let Some(prev) = prev else {
        return (fresh(0), Action::Continue);
    };
    if prev.restored != 0 && prev.good != 0 {
        return (fresh(prev.stage), Action::Continue);
    }
    if prev.good != 0 && prev.fallback < FB_ACTIVE {
        return (fresh(prev.stage), Action::Continue);
    }
    if prev.fallback == FB_RESET {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: prev.stage,
                fallback: FB_ACTIVE,
                good: 0,
                restored: 0,
            },
            Action::Stay,
        );
    }
    if prev.fallback == FB_ACTIVE && prev.restored == 0 {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: prev.stage,
                fallback: FB_RESET,
                good: 0,
                restored: 0,
            },
            Action::Fallback,
        );
    }
    if prev.fallback == FB_ABANDONED {
        return (
            Rec {
                attempts: prev.attempts,
                stage: STAGE_START,
                prev_stage: prev.stage,
                fallback: FB_ABANDONED,
                good: 0,
                restored: 0,
            },
            Action::Stay,
        );
    }
    let attempts = prev.attempts.saturating_add(1);
    if attempts >= FAIL_LIMIT {
        return (
            Rec {
                attempts,
                stage: STAGE_START,
                prev_stage: prev.stage,
                fallback: FB_RESET,
                good: 0,
                restored: 0,
            },
            Action::Fallback,
        );
    }
    (
        Rec {
            attempts,
            stage: STAGE_START,
            prev_stage: prev.stage,
            fallback: FB_NONE,
            good: 0,
            restored: 0,
        },
        Action::Continue,
    )
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
    Some(Rec {
        attempts: read_u32(raw, 16),
        stage: read_u32(raw, 20),
        prev_stage: read_u32(raw, 24),
        fallback: read_u32(raw, 28),
        good: read_u32(raw, 32),
        restored: read_u32(raw, 36),
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
    let sum = checksum(&raw[..LEN]);
    write_u32(raw, OFF_SUM, sum);
    true
}

fn fresh(prev_stage: u32) -> Rec {
    Rec {
        attempts: 0,
        stage: STAGE_START,
        prev_stage,
        fallback: FB_NONE,
        good: 0,
        restored: 0,
    }
}

pub fn checksum(raw: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    let mut i = 0;
    while i < raw.len() {
        let byte = if (OFF_SUM..OFF_SUM + 4).contains(&i) {
            0
        } else {
            raw[i]
        };
        crc ^= byte as u32;
        let mut bit = 0;
        while bit < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            bit += 1;
        }
        i += 1;
    }
    !crc
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
    }
}
