//! Runtime board name. Drivers stay the ones this image was built with.

use crate::board_rev::{self, Choice, Identity};

pub fn choice() -> Choice {
    board_rev::choose(
        crate::board::NAME,
        board_rev::decode_revision(crate::board::board_revision()),
        dtb_identity(),
        crate::config_manager::board_detect(),
    )
}

pub fn report() {
    let choice = choice();
    let mut line = [0u8; 72];
    let mut n = push(&mut line, 0, b"board ");
    n = push(&mut line, n, choice.model.as_bytes());
    n = push(&mut line, n, b" source ");
    n = push(&mut line, n, choice.source.as_bytes());
    n = push(&mut line, n, b" drivers ");
    n = push(&mut line, n, choice.drivers.as_bytes());
    let rev = crate::board::board_revision();
    if rev != 0 {
        n = push(&mut line, n, b" rev ");
        n = push_hex(&mut line, n, rev);
    }
    if let Ok(text) = core::str::from_utf8(&line[..n]) {
        crate::disklog::line(text);
    }
}

pub fn status(out: &mut [u8]) -> usize {
    if out.len() < 40 {
        return 0;
    }
    out[..40].fill(0);
    let choice = choice();
    let rev = if choice.source == "build" && !crate::config_manager::board_detect() {
        0
    } else {
        crate::board::board_revision()
    };
    out[0..4].copy_from_slice(&rev.to_le_bytes());
    put(&mut out[4..20], choice.model);
    put(&mut out[20..36], choice.source);
    40
}

fn dtb_identity() -> Option<Identity> {
    board_rev::decode_compatible(dtb_bytes())
}

fn dtb_bytes() -> &'static [u8] {
    #[cfg(feature = "board-pi4")]
    {
        crate::fw::dtb_bytes().unwrap_or(&[])
    }
    #[cfg(not(feature = "board-pi4"))]
    {
        &[]
    }
}

fn put(out: &mut [u8], text: &str) {
    let n = text.len().min(out.len());
    out[..n].copy_from_slice(&text.as_bytes()[..n]);
}

fn push(out: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    for byte in bytes {
        if n >= out.len() {
            return n;
        }
        out[n] = *byte;
        n += 1;
    }
    n
}

fn push_hex(out: &mut [u8], mut n: usize, value: u32) -> usize {
    let digits = b"0123456789abcdef";
    let raw = value.to_be_bytes();
    for byte in raw {
        if n + 2 > out.len() {
            return n;
        }
        out[n] = digits[(byte >> 4) as usize];
        out[n + 1] = digits[(byte & 0xf) as usize];
        n += 2;
    }
    n
}
