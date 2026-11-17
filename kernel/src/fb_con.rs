//! Pi 4 HDMI text console.
//!
//! Every byte `console::write_str` sends to the PL011 is also drawn here,
//! once the framebuffer is up. Bytes from before that are kept in a fixed
//! buffer and replayed. The painter does not allocate, and it does not take
//! the scheduler, file, config, filesystem, or log locks. If this core
//! already holds the console, or another core does, the extra copy is
//! skipped. The UART write has already happened.
//!
//! The mailbox buffer is 16-byte aligned in `.dma` (Normal non-cacheable).
//! Cache lines are cleaned and invalidated around the call in case an
//! earlier write-back translation left a stale line. The GPU bus address
//! is the physical address with `0xC0000000` set. A refused call is tried
//! once more with the raw physical address. The address the firmware
//! returns has those bits cleared before it is mapped.
//!
//! This has not been run on a Pi 4. QEMU raspi4b emulates the property
//! channel and the framebuffer.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

mod glyphs {
    include!("../../display-console/src/font.rs");
}

const MBOX_BASE: u64 = 0xFE00_B880;
const STATUS: u64 = 0x18;
const WRITE: u64 = 0x20;
const FULL: u32 = 0x8000_0000;
const EMPTY: u32 = 0x4000_0000;
const REQ_OK: u32 = 0x8000_0000;

const TAG_SET_PHYS: u32 = 0x0004_8003;
const TAG_SET_VIRT: u32 = 0x0004_8004;
const TAG_SET_DEPTH: u32 = 0x0004_8005;
const TAG_SET_ORDER: u32 = 0x0004_8006;
const TAG_SET_OFFSET: u32 = 0x0004_8009;
const TAG_ALLOC: u32 = 0x0004_0001;
const TAG_PITCH: u32 = 0x0004_0008;
const TAG_BLANK: u32 = 0x0004_0002;

const EARLY_CAP: usize = 2048;
const LINE_CAP: usize = 240;
const CELL_W: u32 = 8;
const CELL_H: u32 = 16;

const WHITE: u32 = 0x00FF_FFFF;
const GREEN: u32 = 0x0000_FF00;
const YELLOW: u32 = 0x0000_FFFF;
const RED: u32 = 0x0000_00FF;

#[repr(C, align(16))]
struct Prop {
    words: [u32; 48],
}

#[link_section = ".dma"]
static mut PROP: Prop = Prop { words: [0; 48] };

static BUSY: AtomicBool = AtomicBool::new(false);
static mut SAVED_DAIF: u64 = 0;
static READY: AtomicBool = AtomicBool::new(false);

static mut EARLY: [u8; EARLY_CAP] = [0; EARLY_CAP];
static mut EARLY_LEN: usize = 0;

struct Screen {
    ptr: *mut u8,
    width: u32,
    height: u32,
    pitch: u32,
    row: u32,
    line: [u8; LINE_CAP],
    line_len: usize,
    bgr: bool,
}

static mut SCREEN: Screen = Screen {
    ptr: core::ptr::null_mut(),
    width: 0,
    height: 0,
    pitch: 0,
    row: 0,
    line: [0; LINE_CAP],
    line_len: 0,
    bgr: false,
};

static MODE_W: AtomicU32 = AtomicU32::new(0);
static MODE_H: AtomicU32 = AtomicU32::new(0);

/// Allocate the framebuffer and replay the lines already sent to the UART.
/// On failure the serial console is unchanged apart from one warning.
pub fn start() {
    let held = lock();
    let ok = held && bring_up();
    if ok {
        replay();
    }
    unlock(held);
    if ok {
        crate::console::write_str("[OK] HDMI console ");
        write_dec(MODE_W.load(Ordering::Relaxed));
        crate::console::write_str("x");
        write_dec(MODE_H.load(Ordering::Relaxed));
        crate::console::write_str("\r\n");
    } else {
        crate::console::write_str("[WARN] HDMI console unavailable\r\n");
    }
}

/// Copy one UART string onto the display. Safe to skip. Never writes the UART.
pub fn push(text: &str) {
    let held = lock();
    if !held {
        return;
    }
    if READY.load(Ordering::Relaxed) {
        paint(text.as_bytes());
    } else {
        stash(text.as_bytes());
    }
    unlock(held);
}

fn bring_up() -> bool {
    // 1280x720 is the request. A firmware that substitutes another size is
    // accepted. A refused call is tried again at 640x480.
    if let Some(mode) = allocate(1280, 720) {
        return finish(mode);
    }
    if let Some(mode) = allocate(640, 480) {
        return finish(mode);
    }
    false
}

struct Mode {
    width: u32,
    height: u32,
    pitch: u32,
    pa: u64,
    bytes: u64,
    bgr: bool,
}

fn finish(mode: Mode) -> bool {
    let Some(va) = crate::mmu::map_framebuffer(mode.pa, mode.bytes) else {
        return false;
    };
    unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        screen.ptr = va as *mut u8;
        screen.width = mode.width;
        screen.height = mode.height;
        screen.pitch = mode.pitch;
        screen.row = 0;
        screen.line_len = 0;
        screen.bgr = mode.bgr;
        clear_screen(screen);
    }
    MODE_W.store(mode.width, Ordering::Relaxed);
    MODE_H.store(mode.height, Ordering::Relaxed);
    READY.store(true, Ordering::Release);
    true
}

fn allocate(width: u32, height: u32) -> Option<Mode> {
    if let Some(mode) = allocate_at(width, height, true) {
        return Some(mode);
    }
    allocate_at(width, height, false)
}

fn allocate_at(width: u32, height: u32, bus_alias: bool) -> Option<Mode> {
    let mut msg = [0u32; 40];
    let mut n = 1usize;
    msg[n] = 0;
    n += 1;
    n = tag2(&mut msg, n, TAG_SET_PHYS, width, height);
    n = tag2(&mut msg, n, TAG_SET_VIRT, width, height);
    n = tag1(&mut msg, n, TAG_SET_DEPTH, 32);
    n = tag1(&mut msg, n, TAG_SET_ORDER, 1);
    n = tag2(&mut msg, n, TAG_SET_OFFSET, 0, 0);
    let alloc_at = n;
    n = tag2(&mut msg, n, TAG_ALLOC, 4096, 0);
    let pitch_at = n;
    n = tag1(&mut msg, n, TAG_PITCH, 0);
    n = tag1(&mut msg, n, TAG_BLANK, 0);
    msg[n] = 0;
    n += 1;
    msg[0] = (n * 4) as u32;
    if !exchange(&mut msg[..n], bus_alias) {
        return None;
    }
    if msg[1] != REQ_OK {
        return None;
    }
    let got_w = msg[5];
    let got_h = msg[6];
    let depth = msg[15];
    let order = msg[19];
    let bus = msg[alloc_at + 3];
    let gpu_bytes = msg[alloc_at + 4] as u64;
    let pitch = msg[pitch_at + 3];
    if got_w < CELL_W || got_h < CELL_H || depth != 32 || pitch < got_w * 4 {
        return None;
    }
    let bytes = (pitch as u64).checked_mul(got_h as u64)?;
    if gpu_bytes < bytes || bus == 0 {
        return None;
    }
    let pa = (bus & !0xC000_0000) as u64;
    if pa % 4096 != 0 {
        return None;
    }
    Some(Mode {
        width: got_w,
        height: got_h,
        pitch,
        pa,
        bytes,
        bgr: order == 0,
    })
}

fn tag2(msg: &mut [u32], n: usize, tag: u32, a: u32, b: u32) -> usize {
    msg[n] = tag;
    msg[n + 1] = 8;
    msg[n + 2] = 0;
    msg[n + 3] = a;
    msg[n + 4] = b;
    n + 5
}

fn tag1(msg: &mut [u32], n: usize, tag: u32, value: u32) -> usize {
    msg[n] = tag;
    msg[n + 1] = 4;
    msg[n + 2] = 0;
    msg[n + 3] = value;
    n + 4
}

fn exchange(msg: &mut [u32], bus_alias: bool) -> bool {
    if msg.len() > 48 {
        return false;
    }
    let start = crate::timer::get_ticks();
    unsafe {
        let prop = addr_of_mut!(PROP);
        let dst = addr_of_mut!((*prop).words).cast::<u32>();
        for (i, word) in msg.iter().enumerate() {
            dst.add(i).write_volatile(*word);
        }
        let virt = prop as u64;
        civac(virt as *const u8, msg.len() * 4);
        let phys = crate::addr::virt_to_phys(virt);
        let mut bus = phys as u32;
        if bus_alias {
            bus |= 0xC000_0000;
        }
        if !mbox_send((bus & !0xF) | 8, start) {
            return false;
        }
        civac(virt as *const u8, msg.len() * 4);
        for (i, word) in msg.iter_mut().enumerate() {
            *word = dst.add(i).read_volatile();
        }
    }
    true
}

fn mbox_send(msg: u32, start: u64) -> bool {
    let base = crate::addr::phys_to_virt(MBOX_BASE);
    loop {
        if timed_out(start) {
            return false;
        }
        let Some(status) = crate::exception::read_u32_catching((base + STATUS) as *const u32)
        else {
            return false;
        };
        if status & FULL == 0 {
            break;
        }
    }
    unsafe {
        core::ptr::write_volatile((base + WRITE) as *mut u32, msg);
    }
    loop {
        if timed_out(start) {
            return false;
        }
        let Some(status) = crate::exception::read_u32_catching((base + STATUS) as *const u32)
        else {
            return false;
        };
        if status & EMPTY != 0 {
            continue;
        }
        let Some(value) = crate::exception::read_u32_catching(base as *const u32) else {
            return false;
        };
        if value & 0xF == 8 {
            return true;
        }
    }
}

fn timed_out(start: u64) -> bool {
    let freq = crate::timer::frequency();
    if freq == 0 {
        return true;
    }
    crate::timer::get_ticks().wrapping_sub(start) > freq / 2
}

fn civac(ptr: *const u8, len: usize) {
    let start = (ptr as usize) & !63;
    let end = (ptr as usize).saturating_add(len);
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc civac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
}

fn stash(bytes: &[u8]) {
    unsafe {
        let len = *addr_of!(EARLY_LEN);
        let buf = addr_of_mut!(EARLY).cast::<u8>();
        let mut n = len;
        for byte in bytes {
            if n >= EARLY_CAP {
                break;
            }
            buf.add(n).write_volatile(*byte);
            n += 1;
        }
        *addr_of_mut!(EARLY_LEN) = n;
    }
}

fn replay() {
    unsafe {
        let len = *addr_of!(EARLY_LEN);
        let buf = addr_of!(EARLY).cast::<u8>();
        let bytes = core::slice::from_raw_parts(buf, len);
        paint(bytes);
    }
}

fn paint(bytes: &[u8]) {
    for byte in bytes {
        if *byte == b'\r' {
            continue;
        }
        if *byte == b'\n' {
            flush_line();
            continue;
        }
        let full = unsafe {
            let screen = &mut *addr_of_mut!(SCREEN);
            if screen.line_len < LINE_CAP {
                screen.line[screen.line_len] = *byte;
                screen.line_len += 1;
            }
            let cols = columns(screen);
            cols > 0 && screen.line_len >= cols
        };
        if full {
            flush_line();
        }
    }
}

fn flush_line() {
    unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        let len = screen.line_len;
        let color = ink(&screen.line[..len], screen.bgr);
        let mut col = 0u32;
        let mut i = 0;
        while i < len {
            draw_glyph(screen, col, screen.row, screen.line[i], color);
            col += 1;
            i += 1;
        }
        screen.line_len = 0;
        advance(screen);
    }
}

fn advance(screen: &mut Screen) {
    let rows = screen.height / CELL_H;
    if rows == 0 {
        return;
    }
    screen.row += 1;
    if screen.row >= rows {
        scroll(screen);
        screen.row = rows - 1;
    }
}

fn scroll(screen: &Screen) {
    let pitch = screen.pitch as usize;
    let row_bytes = pitch * CELL_H as usize;
    let total = pitch * screen.height as usize;
    if row_bytes == 0 || total <= row_bytes {
        return;
    }
    let mut src = row_bytes;
    while src + 4 <= total {
        unsafe {
            let word = (screen.ptr.add(src) as *const u32).read_volatile();
            (screen.ptr.add(src - row_bytes) as *mut u32).write_volatile(word);
        }
        src += 4;
    }
    let mut dst = total - row_bytes;
    while dst + 4 <= total {
        unsafe {
            (screen.ptr.add(dst) as *mut u32).write_volatile(0);
        }
        dst += 4;
    }
}

fn clear_screen(screen: &Screen) {
    let total = screen.pitch as usize * screen.height as usize;
    let mut off = 0;
    while off + 4 <= total {
        unsafe {
            (screen.ptr.add(off) as *mut u32).write_volatile(0);
        }
        off += 4;
    }
}

fn draw_glyph(screen: &Screen, col: u32, row: u32, ch: u8, color: u32) {
    let x0 = col * CELL_W;
    let y0 = row * CELL_H;
    if x0.saturating_add(CELL_W) > screen.width || y0.saturating_add(CELL_H) > screen.height {
        return;
    }
    let bits = glyphs::glyph(ch);
    for (ry, row_bits) in bits.iter().enumerate() {
        for dup in 0..2 {
            let y = y0 + ry as u32 * 2 + dup;
            for bit in 0..8 {
                let on = row_bits & (0x80 >> bit) != 0;
                put_px(screen, x0 + bit, y, if on { color } else { 0 });
            }
        }
    }
}

fn put_px(screen: &Screen, x: u32, y: u32, color: u32) {
    let off = y as usize * screen.pitch as usize + x as usize * 4;
    unsafe {
        (screen.ptr.add(off) as *mut u32).write_volatile(color);
    }
}

fn columns(screen: &Screen) -> usize {
    let cols = (screen.width / CELL_W) as usize;
    if cols > LINE_CAP {
        LINE_CAP
    } else {
        cols
    }
}

fn ink(line: &[u8], bgr: bool) -> u32 {
    let color = if has_prefix(line, b"[OK]") {
        GREEN
    } else if has_prefix(line, b"[WARN]") {
        YELLOW
    } else if has_prefix(line, b"[ERROR]")
        || has_prefix(line, b"!!!")
        || has_prefix(line, b"=== EXCEPTION")
        || has_prefix(line, b"elfault")
        || has_prefix(line, b"kfault")
        || has_prefix(line, b"panic")
        || has_prefix(line, b"nested")
    {
        RED
    } else {
        WHITE
    };
    if bgr {
        let r = color & 0xff;
        let g = (color >> 8) & 0xff;
        let b = (color >> 16) & 0xff;
        (r << 16) | (g << 8) | b
    } else {
        color
    }
}

fn has_prefix(line: &[u8], prefix: &[u8]) -> bool {
    line.len() >= prefix.len() && &line[..prefix.len()] == prefix
}

fn lock() -> bool {
    if BUSY
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let previous: u64;
    unsafe {
        core::arch::asm!(
            "mrs {previous}, daif",
            "msr daifset, #0xf",
            previous = out(reg) previous,
            options(nomem, nostack)
        );
        *addr_of_mut!(SAVED_DAIF) = previous;
    }
    true
}

fn unlock(held: bool) {
    if !held {
        return;
    }
    unsafe {
        let previous = *addr_of!(SAVED_DAIF);
        core::arch::asm!(
            "msr daif, {previous}",
            previous = in(reg) previous,
            options(nomem, nostack)
        );
    }
    BUSY.store(false, Ordering::Release);
}

fn write_dec(mut n: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if n == 0 {
        crate::console::write_str("0");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    if let Ok(text) = core::str::from_utf8(&buf[i..]) {
        crate::console::write_str(text);
    }
}
