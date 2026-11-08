//! The surface HDMI and VNC both draw.
//!
//! 320 by 240, 32-bit pixels, little-endian, bytes B, G, R, X. The pitch is
//! in bytes and may be wider than 1280. Text is an 8x8 uppercase font.

#![cfg_attr(not(test), no_std)]

mod font;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;
pub const BPP: usize = 4;
pub const STRIDE: usize = WIDTH * BPP;
pub const BYTES: usize = STRIDE * HEIGHT;

const BG: [u8; 4] = [0x2E, 0x1A, 0x1A, 0x00];
const BAR: [u8; 4] = [0x40, 0x20, 0x10, 0x00];
const FG: [u8; 4] = [0xE0, 0xE0, 0xE8, 0x00];
const DIM: [u8; 4] = [0xA0, 0x90, 0x70, 0x00];

/// Paint the status console. `log_text` is recent log lines. `debug_text` is
/// the supervisor page. Lines longer than the screen are clipped.
pub fn draw(buf: &mut [u8], pitch: usize, title: &str, log_text: &str, debug_text: &str) {
    if pitch < STRIDE || buf.len() < pitch {
        return;
    }
    let rows = buf.len() / pitch;
    let rows = if rows < HEIGHT { rows } else { HEIGHT };
    fill(buf, pitch, rows, &BG);
    fill_rows(buf, pitch, 0, 10, &BAR);
    text(buf, pitch, rows, 2, 1, title, &FG);
    text(buf, pitch, rows, 2, 12, "log", &DIM);
    let mut y = 22;
    for line in lines_of(log_text, 12) {
        if y + 8 > rows {
            break;
        }
        text(buf, pitch, rows, 2, y, line, &FG);
        y += 9;
    }
    y += 4;
    if y + 8 <= rows {
        text(buf, pitch, rows, 2, y, "debug", &DIM);
        y += 10;
    }
    for line in lines_of(debug_text, 14) {
        if y + 8 > rows {
            break;
        }
        text(buf, pitch, rows, 2, y, line, &FG);
        y += 9;
    }
}

fn lines_of(text: &str, max: usize) -> Lines<'_> {
    Lines { text, max, seen: 0 }
}

struct Lines<'a> {
    text: &'a str,
    max: usize,
    seen: usize,
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.seen >= self.max || self.text.is_empty() {
            return None;
        }
        let (line, rest) = match self.text.find('\n') {
            Some(at) => (&self.text[..at], &self.text[at + 1..]),
            None => (self.text, ""),
        };
        self.text = rest;
        self.seen += 1;
        Some(line)
    }
}

fn fill(buf: &mut [u8], pitch: usize, rows: usize, color: &[u8; 4]) {
    let mut y = 0;
    while y < rows {
        let row = &mut buf[y * pitch..y * pitch + STRIDE];
        let mut x = 0;
        while x < STRIDE {
            row[x..x + 4].copy_from_slice(color);
            x += 4;
        }
        y += 1;
    }
}

fn fill_rows(buf: &mut [u8], pitch: usize, y0: usize, y1: usize, color: &[u8; 4]) {
    let mut y = y0;
    while y < y1 {
        let row = &mut buf[y * pitch..y * pitch + STRIDE];
        let mut x = 0;
        while x < STRIDE {
            row[x..x + 4].copy_from_slice(color);
            x += 4;
        }
        y += 1;
    }
}

fn text(buf: &mut [u8], pitch: usize, rows: usize, x: usize, y: usize, s: &str, color: &[u8; 4]) {
    let mut col = x;
    for byte in s.bytes() {
        if col + 8 > WIDTH {
            break;
        }
        glyph(buf, pitch, rows, col, y, byte, color);
        col += 8;
    }
}

fn glyph(buf: &mut [u8], pitch: usize, rows: usize, x: usize, y: usize, ch: u8, color: &[u8; 4]) {
    let bits = font::glyph(ch);
    let mut row = 0;
    while row < 8 {
        if y + row >= rows {
            break;
        }
        let mut col = 0;
        while col < 8 {
            if bits[row] & (0x80 >> col) != 0 {
                let at = (y + row) * pitch + (x + col) * 4;
                if at + 4 <= buf.len() {
                    buf[at..at + 4].copy_from_slice(color);
                }
            }
            col += 1;
        }
        row += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_changes_pixels() {
        let mut buf = vec![0u8; BYTES];
        draw(&mut buf, STRIDE, "CONSOLE", "line\n", "vnc off\n");
        assert_eq!(&buf[..4], &BAR);
        let below = STRIDE * 20;
        assert_eq!(&buf[below..below + 4], &BG);
        assert!(buf.iter().any(|byte| *byte == FG[0]));
    }
}
