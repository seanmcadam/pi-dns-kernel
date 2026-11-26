//! Tiny no-alloc writers shared by the kernel and the EL0 programs.
//!
//! Every function stops at the end of `out` and returns the new length.
//! Decimal text has no leading zeros. `push_hex` with `width == 0` writes
//! the shortest lowercase form (`0` for zero). A positive width writes that
//! many lowercase digits, including leading zeros, and never more than 16.
//! `crc32_ieee` is the reflected IEEE CRC-32 (init and xorout `0xffffffff`).
//! Bytes inside `skip` are treated as zero so a checksum field can sit in
//! the buffer it covers.

#![no_std]

use core::ops::Range;

pub fn push_bytes(out: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() && n < out.len() {
        out[n] = bytes[i];
        n += 1;
        i += 1;
    }
    n
}

pub fn push_dec(out: &mut [u8], n: usize, mut value: u64) -> usize {
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
    push_bytes(out, n, &tmp[i..])
}

pub fn push_hex(out: &mut [u8], n: usize, mut value: u64, width: usize) -> usize {
    if width == 0 {
        let mut tmp = [0u8; 16];
        let mut i = 16;
        loop {
            i -= 1;
            tmp[i] = hex_digit((value & 0xf) as u8);
            value >>= 4;
            if value == 0 {
                break;
            }
        }
        return push_bytes(out, n, &tmp[i..]);
    }
    let width = if width > 16 { 16 } else { width };
    let mut tmp = [0u8; 16];
    let mut i = width;
    while i > 0 {
        i -= 1;
        tmp[i] = hex_digit((value & 0xf) as u8);
        value >>= 4;
    }
    push_bytes(out, n, &tmp[..width])
}

pub fn hex_digit(nibble: u8) -> u8 {
    let nibble = nibble & 0xf;
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    }
}

/// Lowercase hex of `src`. Stops before a partial byte when `out` is short.
pub fn hex_encode(src: &[u8], out: &mut [u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < src.len() {
        if n + 2 > out.len() {
            return n;
        }
        out[n] = hex_digit(src[i] >> 4);
        out[n + 1] = hex_digit(src[i]);
        n += 2;
        i += 1;
    }
    n
}

pub fn hex_val(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Decode even-length hex. `None` when a digit is bad or `out` is short.
pub fn hex_decode(text: &[u8], out: &mut [u8]) -> Option<usize> {
    if !text.len().is_multiple_of(2) || text.len() / 2 > out.len() {
        return None;
    }
    let mut n = 0;
    let mut i = 0;
    while i < text.len() {
        let hi = hex_val(text[i])?;
        let lo = hex_val(text[i + 1])?;
        out[n] = (hi << 4) | lo;
        n += 1;
        i += 2;
    }
    Some(n)
}

pub fn crc32_ieee(raw: &[u8], skip: Range<usize>) -> u32 {
    let mut crc = 0xffff_ffffu32;
    let mut i = 0;
    while i < raw.len() {
        let byte = if skip.contains(&i) { 0 } else { raw[i] };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_stop_at_the_end() {
        let mut out = [0u8; 3];
        assert_eq!(push_bytes(&mut out, 0, b"abcd"), 3);
        assert_eq!(&out, b"abc");
        assert_eq!(push_bytes(&mut out, 3, b"z"), 3);
    }

    #[test]
    fn decimal_has_no_leading_zeros() {
        let mut out = [0u8; 20];
        let n = push_dec(&mut out, 0, 0);
        assert_eq!(&out[..n], b"0");
        let n = push_dec(&mut out, 0, 10);
        assert_eq!(&out[..n], b"10");
        let n = push_dec(&mut out, 0, u64::MAX);
        assert_eq!(&out[..n], b"18446744073709551615");
        let mut short = [0u8; 2];
        assert_eq!(push_dec(&mut short, 0, 100), 2);
        assert_eq!(&short, b"10");
    }

    #[test]
    fn hex_width_and_minimal() {
        let mut out = [0u8; 18];
        let n = push_hex(&mut out, 0, 0, 0);
        assert_eq!(&out[..n], b"0");
        let n = push_hex(&mut out, 0, 0x10, 0);
        assert_eq!(&out[..n], b"10");
        let n = push_hex(&mut out, 0, 0xab, 2);
        assert_eq!(&out[..n], b"ab");
        let n = push_hex(&mut out, 0, 0xab, 8);
        assert_eq!(&out[..n], b"000000ab");
        let n = push_hex(&mut out, 0, 0x1234, 16);
        assert_eq!(&out[..n], b"0000000000001234");
        assert_eq!(hex_digit(0), b'0');
        assert_eq!(hex_digit(10), b'a');
        assert_eq!(hex_digit(0x1f), b'f');
    }

    #[test]
    fn hex_round_trip() {
        let src = [0x00u8, 0x0a, 0xff];
        let mut text = [0u8; 8];
        assert_eq!(hex_encode(&src, &mut text), 6);
        assert_eq!(&text[..6], b"000aff");
        let mut back = [0u8; 3];
        assert_eq!(hex_decode(&text[..6], &mut back), Some(3));
        assert_eq!(back, src);
        assert_eq!(hex_decode(b"0", &mut back), None);
        assert_eq!(hex_decode(b"0g", &mut back), None);
        assert_eq!(hex_val(b'A'), Some(10));
    }

    #[test]
    fn crc32_matches_the_ieee_check_value() {
        assert_eq!(crc32_ieee(b"123456789", 0..0), 0xCBF4_3926);
    }

    /// Boot record written by `boot_rec::store` for attempts=2, stage=fs,
    /// previous stage=net. Bytes 12..16 are the stored checksum.
    #[test]
    fn crc32_matches_the_stored_boot_record() {
        let raw: [u8; 64] = [
            66, 79, 79, 84, 2, 0, 0, 0, 64, 0, 0, 0, 46, 163, 103, 248, 2, 0, 0, 0, 5, 0, 0, 0, 7,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        assert_eq!(crc32_ieee(&raw, 12..16), 0xF867_A32E);
        assert_eq!(&raw[12..16], &0xF867_A32Eu32.to_le_bytes());
    }

    #[test]
    fn crc32_treats_the_skip_range_as_zero() {
        let raw = [1u8, 2, 3, 4, 0xaa, 0xbb, 0xcc, 0xdd, 5];
        let hole = crc32_ieee(&raw, 4..8);
        let mut zeroed = raw;
        zeroed[4..8].fill(0);
        assert_eq!(hole, crc32_ieee(&zeroed, 0..0));
        assert_ne!(hole, crc32_ieee(&raw, 0..0));
    }
}
