//! The surface HDMI and VNC both draw.
//!
//! 640 by 480, 32-bit pixels, little-endian, bytes B, G, R, X. The pitch is
//! in bytes and may be wider than 2560. Three bands stack on the screen:
//! system counters, DNS counters, and a log window that keeps the newest
//! lines at the bottom. Text is an 8x8 font. Lowercase uses the uppercase glyph.

#![cfg_attr(not(test), no_std)]

mod font;

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
pub const BPP: usize = 4;
pub const STRIDE: usize = WIDTH * BPP;
pub const BYTES: usize = STRIDE * HEIGHT;
pub const LINE_H: usize = 9;

pub const SYSTEM_LABEL_Y: usize = 12;
pub const SYSTEM_TEXT_Y: usize = 24;
pub const SYSTEM_TEXT_ROWS: usize = 16;
pub const DNS_LABEL_Y: usize = 172;
pub const DNS_TEXT_Y: usize = 184;
pub const DNS_TEXT_ROWS: usize = 14;
pub const LOG_LABEL_Y: usize = 314;
pub const LOG_TEXT_Y: usize = 326;
pub const LOG_ROWS: usize = 16;

/// Last-resort sizes. Used only after a physical-size query stays empty.
/// The first is the one the console asks for. The second is the one the
/// firmware itself picked when hotplug was forced, so it is the smaller ask.
pub const HDMI_FALLBACK_W: u32 = 1280;
pub const HDMI_FALLBACK_H: u32 = 720;
pub const HDMI_LAST_W: u32 = 640;
pub const HDMI_LAST_H: u32 = 480;

/// What to do after one physical-size query.
///
/// A reported size is kept. A miss queries again, then stops. It does not
/// reset HDMI or program a mode: a size the firmware already published
/// has to be left alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HdmiStep {
    Keep,
    PowerCycle,
    QueryAgain,
    Fallback,
    Stop,
}

pub fn hdmi_step(have_size: bool, misses: u32) -> HdmiStep {
    if have_size {
        return HdmiStep::Keep;
    }
    match misses {
        0 | 1 => HdmiStep::QueryAgain,
        _ => HdmiStep::Stop,
    }
}

const BG: [u8; 4] = [0x2E, 0x1A, 0x1A, 0x00];
const BAR: [u8; 4] = [0x40, 0x20, 0x10, 0x00];
const FG: [u8; 4] = [0xE0, 0xE0, 0xE8, 0x00];
const DIM: [u8; 4] = [0xA0, 0x90, 0x70, 0x00];

/// Paint the three bands. `page` is the supervisor text. `log_text` is the
/// recent log. Lines longer than the screen are clipped. The log band shows
/// the last [`LOG_ROWS`] lines, so a new line pushes an old one off the top.
pub fn draw(buf: &mut [u8], pitch: usize, title: &str, page: &str, log_text: &str) {
    if pitch < STRIDE || buf.len() < pitch {
        return;
    }
    let rows = buf.len() / pitch;
    let rows = if rows < HEIGHT { rows } else { HEIGHT };
    fill(buf, pitch, rows, &BG);
    fill_rows(buf, pitch, 0, 10, &BAR);
    text(buf, pitch, rows, 2, 1, title, &FG);
    band(buf, pitch, rows, SYSTEM_LABEL_Y, "system");
    paint_lines(buf, pitch, rows, SYSTEM_TEXT_Y, SYSTEM_TEXT_ROWS, page, Kind::System);
    band(buf, pitch, rows, DNS_LABEL_Y, "dns");
    paint_lines(buf, pitch, rows, DNS_TEXT_Y, DNS_TEXT_ROWS, page, Kind::Dns);
    band(buf, pitch, rows, LOG_LABEL_Y, "log");
    let mut kept = [""; LOG_ROWS];
    let n = visible_lines(log_text, LOG_ROWS, &mut kept);
    let mut i = 0;
    let mut y = LOG_TEXT_Y;
    while i < n {
        if y + 8 > rows {
            break;
        }
        text(buf, pitch, rows, 2, y, kept[i], &FG);
        y += LINE_H;
        i += 1;
    }
}

/// Last `keep` lines, oldest of that window first, so the newest sits at the end.
pub fn visible_lines<'a>(text: &'a str, keep: usize, out: &mut [&'a str]) -> usize {
    if keep == 0 || out.is_empty() {
        return 0;
    }
    let keep = if keep < out.len() { keep } else { out.len() };
    let mut n = 0;
    for line in split_lines(text) {
        if n < keep {
            out[n] = line;
            n += 1;
        } else {
            let mut i = 1;
            while i < keep {
                out[i - 1] = out[i];
                i += 1;
            }
            out[keep - 1] = line;
        }
    }
    n
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    System,
    Dns,
}

fn paint_lines(buf: &mut [u8], pitch: usize, rows: usize, y0: usize, max: usize, page: &str, kind: Kind) {
    let mut y = y0;
    let mut seen = 0;
    for line in split_lines(page) {
        if classify(line) != Some(kind) {
            continue;
        }
        if seen >= max || y + 8 > rows {
            break;
        }
        text(buf, pitch, rows, 2, y, line, &FG);
        y += LINE_H;
        seen += 1;
    }
}

fn classify(line: &str) -> Option<Kind> {
    if line.starts_with("cpu ")
        || line.starts_with("up=")
        || line.starts_with("heap ")
        || line.starts_with("net ")
        || line.starts_with("free_pages=")
        || line.starts_with("asids=")
        || line.starts_with("fds=")
        || line.starts_with("limit=")
        || is_proc(line)
    {
        Some(Kind::System)
    } else if line.starts_with("cache ")
        || line.starts_with("queries=")
        || line.starts_with("hitrate=")
        || line.starts_with("top-")
    {
        Some(Kind::Dns)
    } else {
        None
    }
}

fn is_proc(line: &str) -> bool {
    line.starts_with("dns ")
        || line.starts_with("http ")
        || line.starts_with("spin ")
        || line.starts_with("log ")
        || line.starts_with("telnet ")
        || line.starts_with("vnc ")
        || line.starts_with("hdmi ")
}

fn band(buf: &mut [u8], pitch: usize, rows: usize, y: usize, label: &str) {
    if y + 10 <= rows {
        fill_rows(buf, pitch, y, y + 10, &BAR);
    }
    text(buf, pitch, rows, 2, y + 1, label, &DIM);
}

fn split_lines(text: &str) -> Split<'_> {
    Split { text }
}

struct Split<'a> {
    text: &'a str,
}

impl<'a> Iterator for Split<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.text.is_empty() {
            return None;
        }
        let (line, rest) = match self.text.find('\n') {
            Some(at) => (&self.text[..at], &self.text[at + 1..]),
            None => (self.text, ""),
        };
        self.text = rest;
        if line.is_empty() && rest.is_empty() {
            return None;
        }
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
        if y * pitch + STRIDE > buf.len() {
            break;
        }
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
    fn sections_stack_from_top_to_bottom() {
        let mut buf = vec![0u8; BYTES];
        draw(&mut buf, STRIDE, "CONSOLE", "cpu 0 user=1\ntop-name a.test=1\n", "line\n");
        assert_eq!(&buf[..4], &BAR);
        assert_eq!(&buf[SYSTEM_LABEL_Y * STRIDE..SYSTEM_LABEL_Y * STRIDE + 4], &BAR);
        assert_eq!(&buf[DNS_LABEL_Y * STRIDE..DNS_LABEL_Y * STRIDE + 4], &BAR);
        assert_eq!(&buf[LOG_LABEL_Y * STRIDE..LOG_LABEL_Y * STRIDE + 4], &BAR);
        let at = SYSTEM_TEXT_Y * STRIDE + (2 + 2) * 4;
        assert_eq!(&buf[at..at + 4], &FG);
        assert!(ink(&buf, DNS_TEXT_Y, DNS_TEXT_Y + 8));
        assert!(ink(&buf, LOG_TEXT_Y, LOG_TEXT_Y + 8));
    }

    #[test]
    fn the_log_window_keeps_the_newest_line_at_the_bottom() {
        let mut lines = ["aaaa"; 8];
        let n = visible_lines("a\nb\nc\n", 2, &mut lines);
        assert_eq!(n, 2);
        assert_eq!(lines[0], "b");
        assert_eq!(lines[1], "c");

        let mut log = String::new();
        let mut i = 0;
        while i < 40 {
            log.push_str("aaaa\n");
            i += 1;
        }
        log.push_str("Z\n");
        let mut buf = vec![0u8; BYTES];
        draw(&mut buf, STRIDE, "CONSOLE", "", &log);
        let top = LOG_TEXT_Y * STRIDE + 2 * 4;
        let bot_y = LOG_TEXT_Y + (LOG_ROWS - 1) * LINE_H;
        let bot = bot_y * STRIDE + 2 * 4;
        assert_eq!(&buf[bot..bot + 4], &FG);
        assert_ne!(&buf[top..top + 4], &FG);
    }

    #[test]
    fn a_detected_size_is_kept_and_a_miss_only_retries_the_query() {
        assert_eq!(hdmi_step(true, 0), HdmiStep::Keep);
        assert_eq!(hdmi_step(true, 3), HdmiStep::Keep);
        assert_eq!(hdmi_step(false, 0), HdmiStep::QueryAgain);
        assert_eq!(hdmi_step(false, 1), HdmiStep::QueryAgain);
        assert_eq!(hdmi_step(false, 2), HdmiStep::Stop);
        assert_eq!(hdmi_step(false, 9), HdmiStep::Stop);
    }

    fn ink(buf: &[u8], y0: usize, y1: usize) -> bool {
        let mut y = y0;
        while y < y1 {
            let row = &buf[y * STRIDE..y * STRIDE + STRIDE];
            if row.chunks(4).any(|px| px == FG) {
                return true;
            }
            y += 1;
        }
        false
    }
}
