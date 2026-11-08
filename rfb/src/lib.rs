//! RFB 3.8 server messages and VNC authentication.
//!
//! Buffers come from the caller. The pixel format is 32-bit little-endian
//! true color, red in bits 16..23, green in 8..15, blue in 0..7. Raw
//! encoding is type 0.

#![cfg_attr(not(test), no_std)]

mod des;

pub use des::{encrypt_block, vnc_block};

pub const VERSION: &[u8; 12] = b"RFB 003.008\n";
pub const WIDTH: u16 = 640;
pub const HEIGHT: u16 = 480;
pub const RAW: i32 = 0;

/// Security types. An empty password offers None (1). A password offers VNC
/// auth (2) and nothing else.
pub fn security_types(password: bool, out: &mut [u8]) -> usize {
    if password {
        if out.len() < 2 {
            return 0;
        }
        out[0] = 1;
        out[1] = 2;
        2
    } else {
        if out.len() < 2 {
            return 0;
        }
        out[0] = 1;
        out[1] = 1;
        2
    }
}

pub fn security_result(ok: bool) -> [u8; 4] {
    if ok {
        [0, 0, 0, 0]
    } else {
        [0, 0, 0, 1]
    }
}

/// 16-byte challenge from the hardware counter. Both halves differ.
pub fn challenge(ticks: u64, out: &mut [u8; 16]) {
    let a = ticks.wrapping_mul(0x9E37_79B1_85EB_CA87);
    let b = ticks.rotate_left(17) ^ 0xC2B2_AE3D_27D4_EB4F;
    out[..8].copy_from_slice(&a.to_le_bytes());
    out[8..].copy_from_slice(&b.to_le_bytes());
}

fn load_be(bytes: &[u8]) -> u64 {
    let mut block = 0u64;
    let mut i = 0;
    while i < 8 {
        block = (block << 8) | bytes[i] as u64;
        i += 1;
    }
    block
}

fn store_be(block: u64, out: &mut [u8]) {
    let mut i = 0;
    while i < 8 {
        out[i] = (block >> (56 - 8 * i)) as u8;
        i += 1;
    }
}

/// Encrypt a 16-byte challenge with an 8-byte VNC password (zero padded).
pub fn encrypt_challenge(password: &[u8], challenge: &[u8; 16], out: &mut [u8; 16]) {
    let mut key = [0u8; 8];
    let n = if password.len() < 8 { password.len() } else { 8 };
    key[..n].copy_from_slice(&password[..n]);
    store_be(vnc_block(load_be(&challenge[..8]), &key), &mut out[..8]);
    store_be(vnc_block(load_be(&challenge[8..]), &key), &mut out[8..]);
}

pub fn auth_matches(expected: &[u8; 16], got: &[u8]) -> bool {
    got.len() == 16 && expected.as_slice() == got
}

fn put_u16(out: &mut [u8], value: u16) {
    out[0] = (value >> 8) as u8;
    out[1] = value as u8;
}

fn put_u32(out: &mut [u8], value: u32) {
    out[0] = (value >> 24) as u8;
    out[1] = (value >> 16) as u8;
    out[2] = (value >> 8) as u8;
    out[3] = value as u8;
}

/// ServerInit without the name bytes. 24 bytes: size, pixel format, name length.
pub fn server_init_header(width: u16, height: u16, name_len: u32, out: &mut [u8]) -> usize {
    if out.len() < 24 {
        return 0;
    }
    put_u16(&mut out[0..2], width);
    put_u16(&mut out[2..4], height);
    // bpp, depth, little endian, true color
    out[4] = 32;
    out[5] = 24;
    out[6] = 0;
    out[7] = 1;
    put_u16(&mut out[8..10], 255);
    put_u16(&mut out[10..12], 255);
    put_u16(&mut out[12..14], 255);
    out[14] = 16;
    out[15] = 8;
    out[16] = 0;
    out[17] = 0;
    out[18] = 0;
    out[19] = 0;
    put_u32(&mut out[20..24], name_len);
    24
}

/// FramebufferUpdate with one raw rectangle header. Pixel bytes follow.
pub fn raw_update_header(x: u16, y: u16, width: u16, height: u16, out: &mut [u8]) -> usize {
    if out.len() < 16 {
        return 0;
    }
    out[0] = 0;
    out[1] = 0;
    put_u16(&mut out[2..4], 1);
    put_u16(&mut out[4..6], x);
    put_u16(&mut out[6..8], y);
    put_u16(&mut out[8..10], width);
    put_u16(&mut out[10..12], height);
    put_u32(&mut out[12..16], RAW as u32);
    16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn des_vector() {
        let cipher = encrypt_block(0x0123_4567_89AB_CDEF, 0x1334_5779_9BBC_DFF1);
        assert_eq!(cipher, 0x85E8_1354_0F0A_B405);
    }

    #[test]
    fn vnc_key_is_bit_reversed() {
        // The standard key, bit-reversed per byte, encrypts the same block
        // to a different cipher. OpenSSL DES-ECB produced this value.
        let password = 0x1334_5779_9BBC_DFF1u64.to_be_bytes();
        let cipher = vnc_block(0x0123_4567_89AB_CDEF, &password);
        assert_eq!(cipher, 0xCEE5_5ACD_2386_B23F);
        assert_ne!(cipher, 0x85E8_1354_0F0A_B405);
    }

    #[test]
    fn vnc_password_challenge() {
        let mut out = [0u8; 16];
        let challenge = [0x11u8; 8].into_iter().chain([0x22u8; 8]).collect::<Vec<_>>();
        let mut block = [0u8; 16];
        block.copy_from_slice(&challenge);
        encrypt_challenge(b"password", &block, &mut out);
        assert_eq!(
            out,
            [
                0x10, 0x12, 0xb7, 0x74, 0x52, 0xf3, 0x1b, 0xcc, 0xf5, 0x69, 0xdb, 0xc7, 0xce,
                0x82, 0xc8, 0xc1
            ]
        );
    }

    #[test]
    fn handshake_bytes() {
        assert_eq!(VERSION, b"RFB 003.008\n");
        let mut types = [0u8; 4];
        assert_eq!(security_types(false, &mut types), 2);
        assert_eq!(&types[..2], &[1, 1]);
        assert_eq!(security_types(true, &mut types), 2);
        assert_eq!(&types[..2], &[1, 2]);
        assert_eq!(security_result(true), [0, 0, 0, 0]);
        assert_eq!(security_result(false), [0, 0, 0, 1]);
        let mut header = [0u8; 24];
        assert_eq!(server_init_header(WIDTH, HEIGHT, 10, &mut header), 24);
        assert_eq!(u16::from_be_bytes([header[0], header[1]]), WIDTH);
        assert_eq!(u16::from_be_bytes([header[2], header[3]]), HEIGHT);
        assert_eq!(header[4], 32);
        assert_eq!(header[6], 0);
        assert_eq!(header[7], 1);
        assert_eq!(header[14], 16);
        assert_eq!(header[15], 8);
        assert_eq!(header[16], 0);
        assert_eq!(&header[20..], &10u32.to_be_bytes());
        let mut update = [0u8; 16];
        assert_eq!(raw_update_header(0, 0, WIDTH, HEIGHT, &mut update), 16);
        assert_eq!(update[0], 0);
        assert_eq!(&update[2..4], &1u16.to_be_bytes());
        assert_eq!(&update[8..10], &WIDTH.to_be_bytes());
        assert_eq!(&update[10..12], &HEIGHT.to_be_bytes());
        assert_eq!(&update[12..], &0u32.to_be_bytes());
    }

    #[test]
    fn challenge_changes_with_ticks() {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        challenge(1, &mut a);
        challenge(2, &mut b);
        assert_ne!(a, b);
        assert_eq!(a.len(), 16);
    }
}
