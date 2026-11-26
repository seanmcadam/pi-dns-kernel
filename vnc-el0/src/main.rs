//! RFB 3.8 server. One client at a time. The pixels are the shared console.

#![no_std]
#![no_main]

use core::slice;

core::arch::global_asm!(include_str!("../../user-common/start.s"));

mod syscall {
    #![allow(dead_code)]
    include!("../../user-common/syscall.rs");
}

use syscall::{
    sys, DEBUG_STATUS, DISPLAY_IO, EXIT, LOG_RECENT, TCP_ACCEPT, TCP_LISTEN, TCP_RECV, TCP_SEND,
    TIMER_FREQ, TIMER_TICKS, YIELD,
};

const FB: u64 = 0x1_000A_0000;
const NAME: &[u8] = b"console";

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    let mut port = sys(DISPLAY_IO, 4, 0, 0, 0, 0, 0);
    if port <= 0 || port > 65535 {
        port = 5900;
    }
    let mut password = [0u8; 8];
    let pass_len = sys(DISPLAY_IO, 5, password.as_mut_ptr() as u64, 8, 0, 0, 0);
    let pass_len = if pass_len > 0 { pass_len as usize } else { 0 };
    let mut tries = 0;
    let listen = loop {
        let fd = sys(TCP_LISTEN, port as u64, 0, 0, 0, 0, 0);
        if fd >= 0 {
            break fd as u64;
        }
        tries += 1;
        if tries > 20 {
            panic_rt::note_text(b"vnc listen failed");
            exit_now(1);
        }
        yield_now();
    };
    loop {
        let conn = sys(TCP_ACCEPT, listen, 0, 0, 0, 0, 0);
        if conn < 0 {
            yield_now();
            continue;
        }
        session(conn as u64, &password[..pass_len]);
        syscall::close_tcp(conn);
    }
}

fn session(fd: u64, password: &[u8]) {
    if !send_all(fd, rfb::VERSION.as_ptr(), rfb::VERSION.len()) {
        return;
    }
    let mut version = [0u8; 12];
    if !recv_exact(fd, &mut version) || &version[..3] != b"RFB" {
        return;
    }
    let mut types = [0u8; 2];
    let n = rfb::security_types(!password.is_empty(), &mut types);
    if n == 0 || !send_all(fd, types.as_ptr(), n) {
        return;
    }
    let mut choice = [0u8; 1];
    if !recv_exact(fd, &mut choice) {
        return;
    }
    let authed = if password.is_empty() {
        choice[0] == 1
    } else if choice[0] == 2 {
        check_password(fd, password)
    } else {
        false
    };
    let result = rfb::security_result(authed);
    if !send_all(fd, result.as_ptr(), result.len()) || !authed {
        return;
    }
    let mut shared = [0u8; 1];
    if !recv_exact(fd, &mut shared) {
        return;
    }
    let mut header = [0u8; 24];
    let n = rfb::server_init_header(rfb::WIDTH, rfb::HEIGHT, NAME.len() as u32, &mut header);
    if n == 0 || !send_all(fd, header.as_ptr(), n) || !send_all(fd, NAME.as_ptr(), NAME.len()) {
        return;
    }
    let freq = sys(TIMER_FREQ, 0, 0, 0, 0, 0, 0) as u64;
    let mut follow = false;
    let mut next = 0u64;
    loop {
        let mut kind = [0u8; 1];
        let n = sys(TCP_RECV, fd, kind.as_mut_ptr() as u64, 1, 1, 0, 0);
        if n == -11 {
            let now = sys(TIMER_TICKS, 0, 0, 0, 0, 0, 0) as u64;
            if follow && freq > 0 && now >= next {
                if !send_frame(fd) {
                    return;
                }
                next = now.saturating_add(freq);
            }
            yield_now();
            continue;
        }
        if n <= 0 {
            return;
        }
        match kind[0] {
            0 => {
                if !skip(fd, 19) {
                    return;
                }
            }
            2 => {
                if !skip_encodings(fd) {
                    return;
                }
            }
            3 => {
                if !skip(fd, 9) {
                    return;
                }
                if !send_frame(fd) {
                    return;
                }
                follow = true;
                next = sys(TIMER_TICKS, 0, 0, 0, 0, 0, 0) as u64 + freq;
            }
            4 => {
                if !skip(fd, 7) {
                    return;
                }
            }
            5 => {
                if !skip(fd, 5) {
                    return;
                }
            }
            6 => {
                if !skip_cut(fd) {
                    return;
                }
            }
            _ => return,
        }
    }
}

fn check_password(fd: u64, password: &[u8]) -> bool {
    let mut challenge = [0u8; 16];
    rfb::challenge(sys(TIMER_TICKS, 0, 0, 0, 0, 0, 0) as u64, &mut challenge);
    if !send_all(fd, challenge.as_ptr(), challenge.len()) {
        return false;
    }
    let mut got = [0u8; 16];
    if !recv_exact(fd, &mut got) {
        return false;
    }
    let mut expect = [0u8; 16];
    rfb::encrypt_challenge(password, &challenge, &mut expect);
    rfb::auth_matches(&expect, &got)
}

fn send_frame(fd: u64) -> bool {
    let mut log_buf = [0u8; 1024];
    let mut debug_buf = [0u8; 2048];
    let log_n = sys(LOG_RECENT, log_buf.as_mut_ptr() as u64, log_buf.len() as u64, 0, 0, 0, 0);
    let debug_n = sys(
        DEBUG_STATUS,
        debug_buf.as_mut_ptr() as u64,
        debug_buf.len() as u64,
        0,
        0,
        0,
        0,
    );
    let log = text_of(&log_buf, log_n);
    let debug = text_of(&debug_buf, debug_n);
    unsafe {
        let fb = slice::from_raw_parts_mut(FB as *mut u8, display_console::BYTES);
        display_console::draw(fb, display_console::STRIDE, "console", debug, log);
    }
    let mut header = [0u8; 16];
    let n = rfb::raw_update_header(0, 0, rfb::WIDTH, rfb::HEIGHT, &mut header);
    n != 0
        && send_all(fd, header.as_ptr(), n)
        && send_all(fd, FB as *const u8, display_console::BYTES)
}

fn text_of(buf: &[u8], n: i64) -> &str {
    if n <= 0 {
        return "";
    }
    let n = n as usize;
    let n = if n < buf.len() { n } else { buf.len() };
    core::str::from_utf8(&buf[..n]).unwrap_or("")
}

fn skip_encodings(fd: u64) -> bool {
    let mut head = [0u8; 3];
    if !recv_exact(fd, &mut head) {
        return false;
    }
    let count = u16::from_be_bytes([head[1], head[2]]) as usize;
    let mut left = count;
    let mut junk = [0u8; 4];
    while left > 0 {
        if !recv_exact(fd, &mut junk) {
            return false;
        }
        left -= 1;
        if count - left > 64 {
            return false;
        }
    }
    true
}

fn skip_cut(fd: u64) -> bool {
    let mut head = [0u8; 7];
    if !recv_exact(fd, &mut head) {
        return false;
    }
    let mut len = u32::from_be_bytes([head[3], head[4], head[5], head[6]]) as usize;
    if len > 4096 {
        return false;
    }
    let mut junk = [0u8; 64];
    while len > 0 {
        let n = if len < junk.len() { len } else { junk.len() };
        if !recv_exact(fd, &mut junk[..n]) {
            return false;
        }
        len -= n;
    }
    true
}

fn skip(fd: u64, mut len: usize) -> bool {
    let mut junk = [0u8; 32];
    while len > 0 {
        let n = if len < junk.len() { len } else { junk.len() };
        if !recv_exact(fd, &mut junk[..n]) {
            return false;
        }
        len -= n;
    }
    true
}

fn recv_exact(fd: u64, buf: &mut [u8]) -> bool {
    let mut got = 0;
    while got < buf.len() {
        let n = sys(
            TCP_RECV,
            fd,
            buf[got..].as_mut_ptr() as u64,
            (buf.len() - got) as u64,
            0,
            0,
            0,
        );
        if n == 0 {
            return false;
        }
        if n < 0 {
            yield_now();
            continue;
        }
        got += n as usize;
    }
    true
}

fn send_all(fd: u64, ptr: *const u8, len: usize) -> bool {
    let mut off = 0;
    while off < len {
        let chunk = if len - off > 1024 { 1024 } else { len - off };
        let n = sys(TCP_SEND, fd, ptr as u64 + off as u64, chunk as u64, 0, 0, 0);
        if n == 0 {
            return false;
        }
        if n < 0 {
            yield_now();
            continue;
        }
        off += n as usize;
    }
    true
}

fn yield_now() {
    let _ = sys(YIELD, 0, 0, 0, 0, 0, 0);
}

fn exit_now(code: u64) -> ! {
    let _ = sys(EXIT, code, 0, 0, 0, 0, 0);
    loop {
        yield_now();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    panic_rt::report_panic(b"vnc", info);
    exit_now(1);
}
