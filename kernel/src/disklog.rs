//! SD probe lines that stay readable after boot.
//!
//! `log` before the log task exists only reaches the UART. By the time
//! telnet is connected the 64-line ring has moved on, and the HDMI scroll
//! has moved on with it. These lines are copied into the sticky preface
//! (the same buffer as the HDMI init lines), written to the UART, and on
//! the Pi 4 drawn on the HDMI console. `/status` reads the same outcome.

use spin::Mutex;

struct Info {
    state: &'static str,
    error: &'static str,
    blocks: u32,
    mounted: u32,
    mode: &'static str,
    clock_hz: u32,
}

static INFO: Mutex<Info> = Mutex::new(Info {
    state: "absent",
    error: "",
    blocks: 0,
    mounted: 0,
    mode: "none",
    clock_hz: 0,
});

pub const EXPORT_LEN: usize = 80;

/// One sticky line. Truncated to the preface width.
pub fn line(text: &str) {
    crate::logger::save_early(text);
    crate::console::write_str(text);
    crate::console::write_str("\r\n");
}

/// A controller answered and a card was identified. Clears the error.
pub fn set_card(state: &'static str, blocks: u32) {
    let mut info = INFO.lock();
    info.state = state;
    info.error = "";
    info.blocks = blocks;
}

/// Identification failed. A later success replaces this.
pub fn set_error(state: &'static str, error: &'static str) {
    let mut info = INFO.lock();
    info.state = state;
    info.error = error;
    info.blocks = 0;
    info.mode = "none";
    info.clock_hz = 0;
}

/// Negotiated SD bus. `mode` is `hs`, `ds`, or `none`. `clock_hz` is the
/// card clock after the divider, or 0 when this board has no SD bus.
pub fn set_speed(mode: &'static str, clock_hz: u32) {
    let mut info = INFO.lock();
    info.mode = mode;
    info.clock_hz = clock_hz;
}

pub fn set_mounted(on: bool) {
    INFO.lock().mounted = u32::from(on);
}

/// Fill in an error only when the probe left one blank.
pub fn fail_if_empty(error: &'static str) {
    let mut info = INFO.lock();
    if info.error.is_empty() {
        info.error = error;
    }
}

pub fn note_error(error: &'static str) {
    INFO.lock().error = error;
}

/// `state` 16 bytes, `error` 32 bytes, `blocks` and `mounted` as
/// little-endian u32, `mode` 16 bytes, then `clock_hz`. Strings are
/// NUL terminated.
pub fn export(out: &mut [u8]) {
    let info = INFO.lock();
    out.fill(0);
    copy_cstr(out, 0, 16, info.state);
    copy_cstr(out, 16, 32, info.error);
    if out.len() >= 56 {
        out[48..52].copy_from_slice(&info.blocks.to_le_bytes());
        out[52..56].copy_from_slice(&info.mounted.to_le_bytes());
    }
    if out.len() >= 76 {
        copy_cstr(out, 56, 16, info.mode);
        out[72..76].copy_from_slice(&info.clock_hz.to_le_bytes());
    }
}

/// One pinned HDMI row. No newline. The Pi 4 banner is the only caller.
#[cfg(feature = "board-pi4")]
pub fn write_summary(out: &mut [u8]) -> usize {
    let info = INFO.lock();
    let mut n = 0;
    push(out, &mut n, "sd ");
    push(out, &mut n, info.state);
    if info.error.is_empty() {
        push(out, &mut n, " blk ");
        push_dec(out, &mut n, info.blocks);
    } else {
        push(out, &mut n, " ");
        push(out, &mut n, info.error);
    }
    if info.mounted == 1 {
        push(out, &mut n, " fat");
    }
    n
}

#[cfg(any(feature = "board-pi4", feature = "board-pi5"))]
pub struct Buf {
    data: [u8; 80],
    n: usize,
}

#[cfg(any(feature = "board-pi4", feature = "board-pi5"))]
impl Buf {
    pub fn new() -> Self {
        Self {
            data: [0; 80],
            n: 0,
        }
    }

    pub fn push(&mut self, text: &str) {
        for byte in text.bytes() {
            if self.n >= self.data.len() {
                return;
            }
            self.data[self.n] = byte;
            self.n += 1;
        }
    }

    #[cfg(feature = "board-pi4")]
    pub fn hex(&mut self, value: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut tmp = [0u8; 8];
        let mut i = 0;
        while i < 8 {
            tmp[i] = HEX[((value >> (28 - i * 4)) & 0xf) as usize];
            i += 1;
        }
        self.push(core::str::from_utf8(&tmp).unwrap_or("00000000"));
    }

    pub fn dec(&mut self, value: u32) {
        let mut tmp = [0u8; 10];
        let mut n = tmp.len();
        let mut rest = value;
        if rest == 0 {
            self.push("0");
            return;
        }
        while rest > 0 && n > 0 {
            n -= 1;
            tmp[n] = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
        self.push(core::str::from_utf8(&tmp[n..]).unwrap_or("0"));
    }

    pub fn emit(self) {
        let text = core::str::from_utf8(&self.data[..self.n]).unwrap_or("sd");
        line(text);
    }
}

fn copy_cstr(out: &mut [u8], at: usize, width: usize, text: &str) {
    let end = at + width;
    if out.len() < end || width == 0 {
        return;
    }
    let take = text.len().min(width - 1);
    out[at..at + take].copy_from_slice(&text.as_bytes()[..take]);
}

#[cfg(feature = "board-pi4")]
fn push(out: &mut [u8], n: &mut usize, text: &str) {
    for byte in text.bytes() {
        if *n >= out.len() {
            return;
        }
        out[*n] = byte;
        *n += 1;
    }
}

#[cfg(feature = "board-pi4")]
fn push_dec(out: &mut [u8], n: &mut usize, mut value: u32) {
    let mut tmp = [0u8; 10];
    let mut i = tmp.len();
    if value == 0 {
        push(out, n, "0");
        return;
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    push(out, n, core::str::from_utf8(&tmp[i..]).unwrap_or("0"));
}
