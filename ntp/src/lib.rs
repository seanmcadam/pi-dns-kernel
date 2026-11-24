//! SNTP packet checks, offset math, DNS A lookup bytes, and FAT dates.
//!
//! The kernel keeps time with its own timer between syncs. This crate does
//! not sleep, allocate, or touch a socket. A caller that is not synced yet
//! passes its monotonic clock in milliseconds; a caller that is synced
//! passes NTP milliseconds (Unix milliseconds plus [`NTP_UNIX_DELTA`]
//! seconds). Server timestamps are real NTP time. The offset formula is
//! the same in both cases.

#![cfg_attr(not(test), no_std)]

pub const PACKET_LEN: usize = 48;
/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch.
pub const NTP_UNIX_DELTA: i64 = 2_208_988_800;
pub const UNIX_MIN: i64 = 1_577_836_800;
pub const UNIX_MAX: i64 = 4_102_444_800;
pub const MAX_DELAY_MS: i64 = 16_000;
pub const MAX_STEP_MS: i64 = 86_400_000;
pub const RATE_SLOTS: usize = 8;
pub const RATE_GAP_MS: u64 = 2_000;

pub const MODE_CLIENT: u8 = 3;
pub const MODE_SERVER: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fix {
    pub unix_ms: i64,
    pub offset_ms: i64,
    pub delay_ms: i64,
    pub stratum: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateSlot {
    pub ip: u32,
    pub last_ms: u64,
}

impl RateSlot {
    pub const fn empty() -> Self {
        Self { ip: 0, last_ms: 0 }
    }
}

/// Client request. Returns the transmit timestamp that a reply must echo.
pub fn write_client(out: &mut [u8; PACKET_LEN], clock_ms: i64) -> [u8; 8] {
    *out = [0; PACKET_LEN];
    out[0] = (4 << 3) | MODE_CLIENT;
    write_clock_at(&mut out[40..48], clock_ms);
    let mut stamp = [0u8; 8];
    stamp.copy_from_slice(&out[40..48]);
    stamp
}

/// Server reply. `stratum` is the caller's stratum. Timestamps are NTP
/// milliseconds. `client_tx` is copied into the originate field.
pub fn write_server(
    out: &mut [u8; PACKET_LEN],
    client_tx: &[u8; 8],
    stratum: u8,
    ref_ip: [u8; 4],
    now_ntp_ms: i64,
    ref_ntp_ms: i64,
) {
    *out = [0; PACKET_LEN];
    out[0] = (4 << 3) | MODE_SERVER;
    out[1] = stratum;
    out[2] = 6;
    out[12..16].copy_from_slice(&ref_ip);
    write_clock_at(&mut out[16..24], ref_ntp_ms);
    out[24..32].copy_from_slice(client_tx);
    write_clock_at(&mut out[32..40], now_ntp_ms);
    write_clock_at(&mut out[40..48], now_ntp_ms);
}

fn write_clock_at(out: &mut [u8], clock_ms: i64) {
    let mut buf = [0u8; 8];
    write_clock_ms(&mut buf, clock_ms);
    out[..8].copy_from_slice(&buf);
}

pub fn write_clock_ms(out: &mut [u8; 8], clock_ms: i64) {
    if clock_ms <= 0 {
        *out = [0; 8];
        return;
    }
    let sec = (clock_ms / 1000) as u64;
    let frac_ms = (clock_ms % 1000) as u64;
    let frac = (frac_ms << 32) / 1000;
    out[0..4].copy_from_slice(&(sec as u32).to_be_bytes());
    out[4..8].copy_from_slice(&(frac as u32).to_be_bytes());
}

/// Server timestamp to NTP milliseconds, or `None` when it is zero.
/// A 32-bit seconds field that would land before [`UNIX_MIN`] is read as
/// the next NTP era.
pub fn read_ntp_ms(ts: &[u8]) -> Option<i64> {
    if ts.len() < 8 || ts[..8].iter().all(|byte| *byte == 0) {
        return None;
    }
    let sec = u32::from_be_bytes([ts[0], ts[1], ts[2], ts[3]]) as i64;
    let frac = u32::from_be_bytes([ts[4], ts[5], ts[6], ts[7]]) as u64;
    let mut ntp_sec = sec;
    if ntp_sec - NTP_UNIX_DELTA < UNIX_MIN {
        ntp_sec += 1i64 << 32;
    }
    let ms = (frac * 1000) >> 32;
    Some(ntp_sec * 1000 + ms as i64)
}

/// `((t2 - t1) + (t3 - t4)) / 2`.
pub fn offset_ms(t1: i64, t2: i64, t3: i64, t4: i64) -> i64 {
    let left = t2.wrapping_sub(t1);
    let right = t3.wrapping_sub(t4);
    left.wrapping_add(right) / 2
}

/// Delay `(t4 - t1) - (t3 - t2)`.
pub fn delay_ms(t1: i64, t2: i64, t3: i64, t4: i64) -> i64 {
    t4.wrapping_sub(t1).wrapping_sub(t3.wrapping_sub(t2))
}

pub fn evaluate(
    packet: &[u8],
    sent: &[u8; 8],
    t1_ms: i64,
    t4_ms: i64,
    synced: bool,
    current_unix_ms: i64,
) -> Result<Fix, &'static str> {
    if packet.len() < PACKET_LEN {
        return Err("short");
    }
    let li = packet[0] >> 6;
    let vn = (packet[0] >> 3) & 0x7;
    let mode = packet[0] & 0x7;
    if !(3..=4).contains(&vn) {
        return Err("version");
    }
    if mode != MODE_SERVER {
        return Err("mode");
    }
    if li == 3 {
        return Err("leap");
    }
    if packet[24..32] != *sent {
        return Err("originate");
    }
    let stratum = packet[1];
    if stratum == 0 {
        return Err(if kiss(&packet[12..16]) {
            "kiss"
        } else {
            "stratum"
        });
    }
    if stratum >= 16 {
        return Err("stratum");
    }
    let t2 = read_ntp_ms(&packet[32..40]).ok_or("time")?;
    let t3 = read_ntp_ms(&packet[40..48]).ok_or("time")?;
    let off = offset_ms(t1_ms, t2, t3, t4_ms);
    let delay = delay_ms(t1_ms, t2, t3, t4_ms);
    if !(0..=MAX_DELAY_MS).contains(&delay) {
        return Err("delay");
    }
    let unix_ms = t4_ms.wrapping_add(off).wrapping_sub(NTP_UNIX_DELTA * 1000);
    let unix_sec = unix_ms.div_euclid(1000);
    if !(UNIX_MIN..=UNIX_MAX).contains(&unix_sec) {
        return Err("time");
    }
    if synced {
        let step = unix_ms.wrapping_sub(current_unix_ms);
        if !(-MAX_STEP_MS..=MAX_STEP_MS).contains(&step) {
            return Err("offset");
        }
    }
    Ok(Fix {
        unix_ms,
        offset_ms: off,
        delay_ms: delay,
        stratum,
    })
}

fn kiss(refid: &[u8]) -> bool {
    refid.len() >= 4 && refid[..4].iter().all(|byte| byte.is_ascii_uppercase())
}

pub fn rate_allow(table: &mut [RateSlot], ip: u32, now_ms: u64) -> bool {
    if ip == 0 || table.is_empty() {
        return false;
    }
    let mut found = None;
    let mut oldest = 0usize;
    let mut i = 0;
    while i < table.len() {
        if table[i].ip == ip {
            found = Some(i);
            break;
        }
        if table[i].ip == 0 || table[i].last_ms < table[oldest].last_ms {
            oldest = i;
        }
        i += 1;
    }
    let slot = found.unwrap_or(oldest);
    if table[slot].ip == ip && now_ms.saturating_sub(table[slot].last_ms) < RATE_GAP_MS {
        return false;
    }
    table[slot].ip = ip;
    table[slot].last_ms = now_ms;
    true
}

pub fn civil_from_unix(secs: i64) -> Option<Civil> {
    if secs < 0 {
        return None;
    }
    let tod = secs.rem_euclid(86_400) as u32;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u8;
    let year = (y + i64::from(month <= 2)) as u16;
    if year < 1970 {
        return None;
    }
    Some(Civil {
        year,
        month,
        day,
        hour: (tod / 3600) as u8,
        minute: ((tod % 3600) / 60) as u8,
        second: (tod % 60) as u8,
    })
}

/// FAT date then FAT time. Years outside 1980..=2107 do not fit.
pub fn fat_words(civil: Civil) -> Option<(u16, u16)> {
    if !(1980..=2107).contains(&civil.year) {
        return None;
    }
    if !(1..=12).contains(&civil.month) || !(1..=31).contains(&civil.day) {
        return None;
    }
    if civil.hour > 23 || civil.minute > 59 || civil.second > 59 {
        return None;
    }
    let date = ((civil.year - 1980) << 9) | (u16::from(civil.month) << 5) | u16::from(civil.day);
    let time = (u16::from(civil.hour) << 11)
        | (u16::from(civil.minute) << 5)
        | u16::from(civil.second / 2);
    Some((date, time))
}

pub fn fat_from_unix(secs: i64) -> Option<(u16, u16)> {
    fat_words(civil_from_unix(secs)?)
}

pub fn format_utc(unix_secs: i64, out: &mut [u8]) -> usize {
    let Some(civil) = civil_from_unix(unix_secs) else {
        return 0;
    };
    if out.len() < 20 {
        return 0;
    }
    write_u16(&mut out[0..4], civil.year);
    out[4] = b'-';
    write_u8(&mut out[5..7], civil.month);
    out[7] = b'-';
    write_u8(&mut out[8..10], civil.day);
    out[10] = b'T';
    write_u8(&mut out[11..13], civil.hour);
    out[13] = b':';
    write_u8(&mut out[14..16], civil.minute);
    out[16] = b':';
    write_u8(&mut out[17..19], civil.second);
    out[19] = b'Z';
    20
}

pub fn format_prefix(unix_secs: i64, uptime_secs: u64, out: &mut [u8]) -> usize {
    let n = format_utc(unix_secs, out);
    if n == 0 || n >= out.len() {
        return 0;
    }
    out[n] = b' ';
    let mut n = n + 1;
    n = push_u64(out, n, uptime_secs);
    if n + 2 > out.len() {
        return 0;
    }
    out[n] = b's';
    out[n + 1] = b' ';
    n + 2
}

pub fn write_a_query(out: &mut [u8], id: u16, name: &[u8]) -> Option<usize> {
    if name.is_empty() || name.len() > 253 || out.len() < 17 + name.len() {
        return None;
    }
    out[..12].fill(0);
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2] = 0x01;
    out[5] = 1;
    let mut n = 12;
    for label in name.split(|byte| *byte == b'.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        if n + label.len() + 1 >= out.len() {
            return None;
        }
        out[n] = label.len() as u8;
        n += 1;
        out[n..n + label.len()].copy_from_slice(label);
        n += label.len();
    }
    out[n] = 0;
    n += 1;
    if n + 4 > out.len() {
        return None;
    }
    out[n] = 0;
    out[n + 1] = 1;
    out[n + 2] = 0;
    out[n + 3] = 1;
    Some(n + 4)
}

pub fn read_a(packet: &[u8], id: u16) -> Option<[u8; 4]> {
    if packet.len() < 12 || u16::from_be_bytes([packet[0], packet[1]]) != id {
        return None;
    }
    let flags = u16::from_be_bytes([packet[2], packet[3]]);
    if flags & 0x8000 == 0 || flags & 0x000f != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([packet[4], packet[5]]) as usize;
    let answers = u16::from_be_bytes([packet[6], packet[7]]) as usize;
    let mut i = 12;
    let mut q = 0;
    while q < questions {
        i = skip_name(packet, i)?;
        i = i.checked_add(4)?;
        if i > packet.len() {
            return None;
        }
        q += 1;
    }
    let mut a = 0;
    while a < answers && a < 16 {
        i = skip_name(packet, i)?;
        if i + 10 > packet.len() {
            return None;
        }
        let kind = u16::from_be_bytes([packet[i], packet[i + 1]]);
        let class = u16::from_be_bytes([packet[i + 2], packet[i + 3]]);
        let rdlen = u16::from_be_bytes([packet[i + 8], packet[i + 9]]) as usize;
        i += 10;
        if i + rdlen > packet.len() {
            return None;
        }
        if kind == 1 && class == 1 && rdlen == 4 {
            return Some([packet[i], packet[i + 1], packet[i + 2], packet[i + 3]]);
        }
        i += rdlen;
        a += 1;
    }
    None
}

pub fn ntp_from_bootreply(frame: &[u8], out: &mut [[u8; 4]]) -> usize {
    if out.is_empty() || frame.len() < 46 {
        return 0;
    }
    let ihl = ((frame[14] & 0x0f) as usize) * 4;
    if ihl < 20 {
        return 0;
    }
    let start = 14 + ihl + 8 + 240;
    if frame.len() < start + 4 || frame[start - 4..start] != [0x63, 0x82, 0x53, 0x63] {
        return 0;
    }
    let mut o = start;
    let mut nout = 0;
    while o < frame.len() && nout < out.len() {
        let kind = frame[o];
        if kind == 255 {
            break;
        }
        if kind == 0 {
            o += 1;
            continue;
        }
        if o + 1 >= frame.len() {
            break;
        }
        let n = frame[o + 1] as usize;
        if o + 2 + n > frame.len() {
            break;
        }
        if kind == 42 {
            let mut p = 0;
            while p + 4 <= n && nout < out.len() {
                let ip = [
                    frame[o + 2 + p],
                    frame[o + 2 + p + 1],
                    frame[o + 2 + p + 2],
                    frame[o + 2 + p + 3],
                ];
                if ip != [0, 0, 0, 0] {
                    out[nout] = ip;
                    nout += 1;
                }
                p += 4;
            }
        }
        o += 2 + n;
    }
    nout
}

fn skip_name(packet: &[u8], mut i: usize) -> Option<usize> {
    let mut hops = 0;
    loop {
        if i >= packet.len() || hops > 16 {
            return None;
        }
        let len = packet[i];
        if len == 0 {
            return Some(i + 1);
        }
        if len & 0xc0 == 0xc0 {
            if i + 1 >= packet.len() {
                return None;
            }
            return Some(i + 2);
        }
        if len & 0xc0 != 0 {
            return None;
        }
        i = i.checked_add(1 + len as usize)?;
        hops += 1;
    }
}

fn write_u16(out: &mut [u8], value: u16) {
    out[0] = b'0' + (value / 1000) as u8;
    out[1] = b'0' + ((value / 100) % 10) as u8;
    out[2] = b'0' + ((value / 10) % 10) as u8;
    out[3] = b'0' + (value % 10) as u8;
}

fn write_u8(out: &mut [u8], value: u8) {
    out[0] = b'0' + value / 10;
    out[1] = b'0' + value % 10;
}

fn push_u64(out: &mut [u8], mut n: usize, mut value: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    if value == 0 {
        if n < out.len() {
            out[n] = b'0';
            return n + 1;
        }
        return n;
    }
    while value > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    while i < tmp.len() && n < out.len() {
        out[n] = tmp[i];
        n += 1;
        i += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ntp_ms(unix_sec: i64) -> i64 {
        (unix_sec + NTP_UNIX_DELTA) * 1000
    }

    #[test]
    fn offset_and_delay_match_the_sntp_formula() {
        assert_eq!(offset_ms(1000, 5000, 5000, 2000), 3500);
        assert_eq!(delay_ms(1000, 5000, 5000, 2000), 1000);
    }

    #[test]
    fn an_unsynced_reply_lands_on_the_server_time() {
        let server = ntp_ms(1_609_459_200);
        let mut packet = [0u8; PACKET_LEN];
        let sent = write_client(&mut packet, 1000);
        write_server(&mut packet, &sent, 2, [1, 2, 3, 4], server, server);
        let fix = evaluate(&packet, &sent, 1000, 1100, false, 0).unwrap();
        assert_eq!(fix.stratum, 2);
        assert_eq!(fix.delay_ms, 100);
        assert_eq!(fix.unix_ms / 1000, 1_609_459_200);
        assert!(fix.offset_ms > 1_000_000);
    }

    #[test]
    fn kiss_stratum_and_a_large_step_are_rejected() {
        let server = ntp_ms(1_609_459_200);
        let mut packet = [0u8; PACKET_LEN];
        let sent = write_client(&mut packet, server);
        let now = 1_609_459_200 * 1000;
        write_server(&mut packet, &sent, 2, [9, 9, 9, 9], server + 20, server);
        let ok = evaluate(&packet, &sent, server, server + 40, true, now).unwrap();
        assert!(ok.offset_ms.abs() < 1000);

        packet[1] = 0;
        packet[12..16].copy_from_slice(b"RATE");
        assert_eq!(
            evaluate(&packet, &sent, server, server + 40, true, now),
            Err("kiss")
        );
        packet[1] = 16;
        assert_eq!(
            evaluate(&packet, &sent, server, server + 40, true, now),
            Err("stratum")
        );
        packet[1] = 2;
        packet[0] = (3 << 6) | (4 << 3) | MODE_SERVER;
        assert_eq!(
            evaluate(&packet, &sent, server, server + 40, true, now),
            Err("leap")
        );

        let later = ntp_ms(1_609_459_200 + 2 * 86_400);
        write_server(&mut packet, &sent, 2, [9, 9, 9, 9], later, later);
        assert_eq!(
            evaluate(&packet, &sent, server, server + 40, true, now),
            Err("offset")
        );
    }

    #[test]
    fn a_negative_delay_is_rejected() {
        let server = ntp_ms(1_609_459_200);
        let mut packet = [0u8; PACKET_LEN];
        let sent = write_client(&mut packet, 1000);
        write_server(&mut packet, &sent, 1, [1, 1, 1, 1], server, server);
        write_clock_at(&mut packet[40..48], server + 5_000);
        assert_eq!(evaluate(&packet, &sent, 1000, 1100, false, 0), Err("delay"));
    }

    #[test]
    fn fat_words_encode_the_unix_and_the_fallback_date() {
        let civil = civil_from_unix(1_609_459_200).unwrap();
        assert_eq!(
            civil,
            Civil {
                year: 2021,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0,
            }
        );
        assert_eq!(fat_from_unix(1_609_459_200), Some((21025, 0)));
        let fallback = Civil {
            year: 2026,
            month: 11,
            day: 5,
            hour: 0,
            minute: 0,
            second: 0,
        };
        assert_eq!(fat_words(fallback), Some((23909, 0)));
        assert_eq!(civil_from_unix(0).unwrap().year, 1970);
        assert!(fat_from_unix(0).is_none());
        let mut text = [0u8; 20];
        assert_eq!(format_utc(1_609_459_200, &mut text), 20);
        assert_eq!(&text, b"2021-01-01T00:00:00Z");
    }

    #[test]
    fn rate_limit_keeps_one_reply_per_gap() {
        let mut table = [RateSlot::empty(); RATE_SLOTS];
        assert!(rate_allow(&mut table, 0x0a000001, 1_000));
        assert!(!rate_allow(&mut table, 0x0a000001, 1_500));
        assert!(rate_allow(&mut table, 0x0a000002, 1_500));
        assert!(rate_allow(&mut table, 0x0a000001, 1_000 + RATE_GAP_MS));
    }

    #[test]
    fn dns_query_and_option_42_round_trip() {
        let mut query = [0u8; 64];
        let n = write_a_query(&mut query, 0x1234, b"0.pool.ntp.org").unwrap();
        let mut response = [0u8; 128];
        response[..n].copy_from_slice(&query[..n]);
        response[2] = 0x81;
        response[3] = 0x80;
        response[7] = 1;
        let mut i = n;
        response[i] = 0xc0;
        response[i + 1] = 0x0c;
        i += 2;
        response[i + 1] = 1;
        response[i + 3] = 1;
        response[i + 9] = 4;
        response[i + 10] = 1;
        response[i + 11] = 2;
        response[i + 12] = 3;
        response[i + 13] = 4;
        assert_eq!(read_a(&response[..i + 14], 0x1234), Some([1, 2, 3, 4]));
        assert!(read_a(&query[..n], 0x1234).is_none());

        let mut frame = [0u8; 300];
        frame[14] = 0x45;
        let start = 14 + 20 + 8 + 240;
        frame[start - 4..start].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        frame[start] = 42;
        frame[start + 1] = 8;
        frame[start + 2..start + 6].copy_from_slice(&[10, 0, 2, 2]);
        frame[start + 6..start + 10].copy_from_slice(&[1, 1, 1, 1]);
        frame[start + 10] = 255;
        let mut out = [[0u8; 4]; 4];
        assert_eq!(ntp_from_bootreply(&frame[..start + 11], &mut out), 2);
        assert_eq!(out[0], [10, 0, 2, 2]);
        assert_eq!(out[1], [1, 1, 1, 1]);
    }
}
