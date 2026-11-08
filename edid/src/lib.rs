//! A small EDID 1.3 reader.
//!
//! The block is 128 bytes. The header is `00 FF FF FF FF FF FF 00`. Byte 127
//! makes the sum of the block zero modulo 256. The preferred timing is the
//! first detailed descriptor (offset 54) when its pixel clock is not zero.

#![cfg_attr(not(test), no_std)]

const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub width: u16,
    pub height: u16,
}

/// Preferred width and height, or `None` when the block is not a timing.
pub fn preferred(block: &[u8]) -> Option<Timing> {
    if block.len() < 128 {
        return None;
    }
    if block[..8] != HEADER {
        return None;
    }
    let mut sum = 0u16;
    let mut i = 0;
    while i < 128 {
        sum = sum.wrapping_add(block[i] as u16);
        i += 1;
    }
    if sum & 0xFF != 0 {
        return None;
    }
    let d = &block[54..72];
    // A zero pixel clock marks a display descriptor, not a video mode.
    if d[0] == 0 && d[1] == 0 {
        return None;
    }
    let width = (((d[4] as u16) & 0xF0) << 4) | d[2] as u16;
    let height = (((d[7] as u16) & 0xF0) << 4) | d[5] as u16;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Timing { width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(width_lo: u8, width_hi: u8, height_lo: u8, height_hi: u8) -> [u8; 128] {
        let mut block = [0u8; 128];
        block[..8].copy_from_slice(&HEADER);
        block[54] = 0xD0;
        block[55] = 0x1E;
        block[56] = width_lo;
        block[58] = width_hi;
        block[59] = height_lo;
        block[61] = height_hi;
        let mut sum = 0u16;
        let mut i = 0;
        while i < 127 {
            sum = sum.wrapping_add(block[i] as u16);
            i += 1;
        }
        block[127] = (0u16.wrapping_sub(sum) & 0xFF) as u8;
        block
    }

    #[test]
    fn reads_640x480() {
        // byte2 = 0x80 and the high nibble of byte4 = 2 -> 0x280.
        // byte5 = 0xE0 and the high nibble of byte7 = 1 -> 0x1E0.
        let block = sample(0x80, 0x20, 0xE0, 0x10);
        assert_eq!(
            preferred(&block),
            Some(Timing {
                width: 640,
                height: 480
            })
        );
    }

    #[test]
    fn rejects_a_bad_checksum() {
        let mut block = sample(0x80, 0x20, 0xE0, 0x10);
        block[127] ^= 0xFF;
        assert_eq!(preferred(&block), None);
    }

    #[test]
    fn rejects_a_bad_header_and_a_display_descriptor() {
        let mut block = sample(0x80, 0x20, 0xE0, 0x10);
        block[1] = 0;
        let sum_fix = {
            let mut sum = 0u16;
            for byte in &block[..127] {
                sum = sum.wrapping_add(*byte as u16);
            }
            (0u16.wrapping_sub(sum) & 0xFF) as u8
        };
        block[127] = sum_fix;
        assert_eq!(preferred(&block), None);
        let mut desc = sample(0x80, 0x20, 0xE0, 0x10);
        desc[54] = 0;
        desc[55] = 0;
        let mut sum = 0u16;
        for byte in &desc[..127] {
            sum = sum.wrapping_add(*byte as u16);
        }
        desc[127] = (0u16.wrapping_sub(sum) & 0xFF) as u8;
        assert_eq!(preferred(&desc), None);
    }
}
