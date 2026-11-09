// Bounded panic text. No allocation. The caller supplies the buffer.
// A message that mentions a credential is replaced with `redacted`. The
// bytes that would have been logged are not kept.

pub(crate) fn format_panic_line(
    out: &mut [u8],
    who: &[u8],
    file: &[u8],
    line: u32,
    msg: &[u8],
) -> usize {
    let mut n = 0;
    n = push(out, n, b"panic ");
    n = push(out, n, who);
    if !file.is_empty() {
        n = push(out, n, b" ");
        let base = basename(file);
        let base = tail(base, 24);
        n = push(out, n, base);
        n = push(out, n, b":");
        n = push_u32(out, n, line);
    }
    if !msg.is_empty() {
        if secret(msg) {
            n = push(out, n, b" redacted");
        } else {
            n = push(out, n, b" ");
            n = push(out, n, msg);
        }
    }
    n
}

pub(crate) fn secret(msg: &[u8]) -> bool {
    const WORDS: [&[u8]; 6] = [
        b"password",
        b"passwd",
        b"secret",
        b"token",
        b"authorization",
        b"credential",
    ];
    let mut i = 0;
    while i < WORDS.len() {
        if contains_word(msg, WORDS[i]) {
            return true;
        }
        i += 1;
    }
    false
}

fn basename(file: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut i = 0;
    while i < file.len() {
        if file[i] == b'/' || file[i] == b'\\' {
            start = i + 1;
        }
        i += 1;
    }
    &file[start..]
}

fn tail<'a>(bytes: &'a [u8], max: usize) -> &'a [u8] {
    if bytes.len() > max {
        &bytes[bytes.len() - max..]
    } else {
        bytes
    }
}

fn contains_word(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    let last = hay.len() - needle.len();
    let mut i = 0;
    while i <= last {
        let mut j = 0;
        while j < needle.len() && lower(hay[i + j]) == needle[j] {
            j += 1;
        }
        if j == needle.len() {
            return true;
        }
        i += 1;
    }
    false
}

fn lower(byte: u8) -> u8 {
    if byte.is_ascii_uppercase() {
        byte - b'A' + b'a'
    } else {
        byte
    }
}

pub(crate) fn push(out: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() && n < out.len() {
        out[n] = bytes[i];
        n += 1;
        i += 1;
    }
    n
}

pub(crate) fn push_u32(out: &mut [u8], n: usize, mut value: u32) -> usize {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if value == 0 {
        return push(out, n, b"0");
    }
    while value > 0 {
        i -= 1;
        buf[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    push(out, n, &buf[i..])
}
