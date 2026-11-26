//! SD high-speed switch and the SDHCI clock divider.
//!
//! High speed here is SDR25: 50 MHz at 3.3 V, function group 1 set to 1.
//! SDR50, SDR104, and DDR50 need a 1.8 V I/O switch. Nothing in this crate
//! describes that switch, and a caller that has not implemented it must
//! not request those functions.

/// Default-speed data clock. Identification stays at 400 kHz.
pub const DS_HZ: u32 = 25_000_000;
/// High-speed data clock. The divider may land lower when the base
/// clock does not divide evenly.
pub const HS_HZ: u32 = 50_000_000;
/// CMD6 SWITCH_FUNC status is one 512-bit block.
pub const SWITCH_LEN: usize = 64;

/// CMD6 check. Access mode (group 1) is high speed. Other groups are `0xF`,
/// which means "no change".
pub const CMD6_CHECK_HS: u32 = 0x00FF_FFF1;
/// CMD6 set. Same groups, with the mode bit set.
pub const CMD6_SWITCH_HS: u32 = 0x80FF_FFF1;

/// Parsed CMD6 status. `group1` is the function the card says is selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwitchStatus {
    pub high_speed_supported: bool,
    pub group1: u8,
    pub group1_busy: bool,
}

/// `None` when the buffer is shorter than the 64-byte status. A short or
/// empty reply is not a successful switch.
pub fn parse_switch(status: &[u8]) -> Option<SwitchStatus> {
    if status.len() < SWITCH_LEN {
        return None;
    }
    // Group 1 support is the big-endian field at bytes 12..14.
    // Bit 1 is function 1 (SDR25). Bit 0 is the default function.
    let support = u16::from_be_bytes([status[12], status[13]]);
    // Selected functions are packed 4 bits each. Group 1 is the low
    // nibble of byte 16.
    let group1 = status[16] & 0x0f;
    // Group 1 busy is the big-endian field at bytes 28..30. A set bit
    // means that function is not ready.
    let busy = u16::from_be_bytes([status[28], status[29]]);
    Some(SwitchStatus {
        high_speed_supported: support & (1 << 1) != 0,
        group1,
        group1_busy: busy & (1 << 1) != 0,
    })
}

/// The switch took effect: function 1 is selected and not busy.
pub fn switch_accepted(status: &[u8]) -> bool {
    matches!(
        parse_switch(status),
        Some(SwitchStatus {
            group1: 1,
            group1_busy: false,
            ..
        })
    )
}

/// SDHCI 3.00 10-bit divisor field (bits 15:8 and 7:6 of the clock register).
///
/// The card clock is `base / (2 * n)` for `n > 0`, and `base` when `n` is 0.
/// `n` is the smallest value whose result is not above `target_hz`. A target
/// of 0, or a base that is already at or below the target, programs no
/// division.
pub fn clock_div(base_hz: u32, target_hz: u32) -> u16 {
    if target_hz == 0 || base_hz <= target_hz {
        return 0;
    }
    let step = target_hz.saturating_mul(2).max(1);
    let mut n = base_hz.saturating_add(step - 1) / step;
    n = n.clamp(1, 0x3ff);
    let low = ((n & 0xff) as u16) << 8;
    let high = ((n & 0x300) >> 8) as u16;
    low | (high << 6)
}

/// Card clock from a clock-control register. Enable bits in the low
/// positions are ignored.
pub fn card_hz(base_hz: u32, div_reg: u16) -> u32 {
    let n = ((div_reg >> 8) & 0xff) as u32 | ((((div_reg as u32) >> 6) & 0x3) << 8);
    if n == 0 {
        base_hz
    } else {
        base_hz / (2 * n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divider_stays_at_or_under_the_target() {
        let reg = clock_div(200_000_000, HS_HZ);
        assert_eq!(card_hz(200_000_000, reg), 50_000_000);
        // 250 MHz does not divide to 50. The next legal step is 250/6.
        let reg = clock_div(250_000_000, HS_HZ);
        assert_eq!(card_hz(250_000_000, reg), 250_000_000 / 6);
        assert!(card_hz(250_000_000, reg) <= HS_HZ);
        let reg = clock_div(200_000_000, DS_HZ);
        assert_eq!(card_hz(200_000_000, reg), DS_HZ);
        let reg = clock_div(100_000_000, 400_000);
        assert_eq!(card_hz(100_000_000, reg), 400_000);
        assert_eq!(clock_div(50_000_000, HS_HZ), 0);
        assert_eq!(clock_div(200_000_000, 0), 0);
        for base in [50_000_000, 100_000_000, 200_000_000, 250_000_000] {
            for target in [400_000, DS_HZ, HS_HZ] {
                let reg = clock_div(base, target);
                let hz = card_hz(base, reg);
                if base > target {
                    assert!(hz <= target, "{base} -> {hz} above {target}");
                    assert!(hz > 0);
                } else {
                    assert_eq!(hz, base);
                }
            }
        }
    }

    #[test]
    fn divider_packs_the_upper_bits() {
        // n = 259 = 0x103. Bits 7:0 of n sit in register bits 15:8.
        // Bits 9:8 of n sit in register bits 7:6.
        let reg = clock_div(518_000_000, 1_000_000);
        assert_eq!(reg, 0x0340);
        assert_eq!(card_hz(518_000_000, reg), 518_000_000 / (2 * 0x103));
        let capped = clock_div(u32::MAX, 1);
        let n = ((capped >> 8) & 0xff) as u32 | ((((capped as u32) >> 6) & 0x3) << 8);
        assert_eq!(n, 0x3ff);
    }

    #[test]
    fn cmd6_accepts_high_speed_only_when_function_1_is_ready() {
        assert_eq!(CMD6_CHECK_HS, 0x00FF_FFF1);
        assert_eq!(CMD6_SWITCH_HS, 0x80FF_FFF1);
        let mut status = [0u8; SWITCH_LEN];
        status[13] = 0x02;
        let parsed = parse_switch(&status).unwrap();
        assert!(parsed.high_speed_supported);
        assert_eq!(parsed.group1, 0);
        assert!(!parsed.group1_busy);
        assert!(!switch_accepted(&status));
        status[16] = 0x01;
        assert!(switch_accepted(&status));
        status[29] = 0x02;
        let busy = parse_switch(&status).unwrap();
        assert!(busy.group1_busy);
        assert!(!switch_accepted(&status));
        status[29] = 0;
        status[13] = 0x01;
        status[16] = 0;
        assert!(!parse_switch(&status).unwrap().high_speed_supported);
        assert!(parse_switch(&status[..63]).is_none());
        assert!(!switch_accepted(&[]));
        assert!(!switch_accepted(&status[..63]));
    }
}
