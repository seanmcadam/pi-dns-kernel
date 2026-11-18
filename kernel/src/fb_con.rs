//! Pi 4 HDMI text console.
//!
//! Every byte `console::write_str` sends to the PL011 is also drawn here,
//! once the framebuffer is up. Bytes from before that are kept in a fixed
//! buffer and replayed. The painter does not allocate, and it does not take
//! the scheduler, file, config, filesystem, or log locks. If this core
//! already holds the console, or another core does, the extra copy is
//! skipped. The UART write has already happened.
//!
//! The mailbox buffer is 16-byte aligned in `.dma`. The coarse map is
//! write-back, so every mailbox word is cleaned and invalidated. The GPU
//! bus address is the physical address with `0xC0000000` set. A refused
//! call is tried once more with the raw physical address. A returned
//! address whose top bits are a Pi 4 alias becomes an ARM address with
//! `bus & 0x3FFFFFFF`. Pixels are stored through the coarse map and the
//! cache is cleaned, then the strict map moves them to a non-cacheable
//! window. A mailbox that never answers gives up on a counter timeout
//! and a spin budget.
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
const TAG_GET_PHYS: u32 = 0x0004_0003;
const TAG_GET_DEPTH: u32 = 0x0004_0005;
const TAG_GET_ORDER: u32 = 0x0004_0006;

const EARLY_CAP: usize = 2048;
const LINE_CAP: usize = 240;
const CELL_W: u32 = 8;
const CELL_H: u32 = 16;

const WHITE: u32 = 0x00FF_FFFF;
const GREEN: u32 = 0x0000_FF00;
const YELLOW: u32 = 0x0000_FFFF;
const RED: u32 = 0x0000_00FF;

// 64-byte alignment keeps the property buffer on its own cache lines.
// A 16-byte alignment shares the first line with the previous .dma
// object. Cleaning that line after the GPU has written the reply
// stores the neighbor back over the answer.
#[repr(C, align(64))]
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
    /// Physical framebuffer. Kept so the strict map can retarget it.
    pa: u64,
    bytes: u64,
    /// True while `ptr` is the write-back alias. Stores are cleaned.
    clean: bool,
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
    pa: 0,
    bytes: 0,
    clean: false,
};

static MODE_W: AtomicU32 = AtomicU32::new(0);
static MODE_H: AtomicU32 = AtomicU32::new(0);
/// Set when a fault has painted the pinned rows. Later log lines and the
/// status refresh must not scroll that text off the panel.
static FROZEN: AtomicBool = AtomicBool::new(false);

const HEADER_CAP: usize = 640;
/// NIC status uses the first three rows. The fourth is the per-core
/// heartbeat, painted without clearing the rows above.
const STATUS_ROWS: u32 = 3;
const PINNED_ROWS: u32 = 4;
static mut HEADER: [u8; HEADER_CAP] = [0; HEADER_CAP];
static mut HEADER_LEN: usize = 0;

/// Allocate the framebuffer and paint one line, before the strict map.
/// Does not write the UART. Returns whether the panel was programmed.
pub fn start_early() -> bool {
    let held = lock();
    let ok = held && bring_up();
    if ok {
        replay();
        paint(b"Pi 4 boot\n");
    }
    unlock(held);
    ok
}

/// Serial line for the mode. Call after the UART is programmed.
pub fn announce() {
    if READY.load(Ordering::Acquire) {
        crate::console::write_str("[OK] HDMI console ");
        write_dec(MODE_W.load(Ordering::Relaxed));
        crate::console::write_str("x");
        write_dec(MODE_H.load(Ordering::Relaxed));
        crate::console::write_str("\r\n");
    } else {
        crate::console::write_str("[WARN] HDMI console unavailable\r\n");
    }
}

/// Point the painter at the non-cacheable window. The strict map has to
/// be installed first. A refused window keeps the write-back alias.
pub fn switch_to_nc() {
    let held = lock();
    if !held {
        return;
    }
    unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        if !screen.ptr.is_null() && screen.clean && screen.bytes > 0 {
            civac(screen.ptr, screen.bytes as usize);
            if let Some(va) = crate::mmu::map_framebuffer(screen.pa, screen.bytes) {
                screen.ptr = va as *mut u8;
                screen.clean = false;
            }
        }
    }
    unlock(held);
}

/// Paint one line on row 0 without taking the console lock. A lockup may
/// already hold that lock, or the owner may be the core that is stuck.
/// Does not allocate and does not write the UART. A missing framebuffer
/// is a no-op. Newlines fill the other pinned rows, and the log stops
/// scrolling so the text stays on screen.
pub fn emergency(text: &str) {
    freeze(text);
}

/// Paint up to three pinned rows and stop the scrolling log. Safe to call
/// from a fault with the console lock already held. Does not allocate.
pub fn freeze(text: &str) {
    FROZEN.store(true, Ordering::Relaxed);
    if !READY.load(Ordering::Relaxed) {
        return;
    }
    unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        if screen.ptr.is_null() {
            return;
        }
        let pitch = screen.pitch as usize;
        let row_bytes = pitch * CELL_H as usize;
        let rows_bytes = row_bytes * PINNED_ROWS as usize;
        let mut off = 0;
        while off + 4 <= rows_bytes {
            (screen.ptr.add(off) as *mut u32).write_volatile(0);
            off += 4;
        }
        let color = if screen.bgr { 0x00FF_0000 } else { RED };
        let cols = columns(screen);
        let mut row = 0u32;
        let mut col = 0usize;
        for &ch in text.as_bytes() {
            if ch == b'\n' || ch == b'\r' {
                row += 1;
                col = 0;
                if row >= PINNED_ROWS {
                    break;
                }
                continue;
            }
            if col >= cols {
                continue;
            }
            draw_glyph(screen, col as u32, row, ch, color);
            col += 1;
        }
    }
}

/// Replace the three pinned top rows. The log scrolls underneath them. A refused
/// lock skips this update; the next call tries again. Does not write the UART.
pub fn set_status(text: &str) {
    if FROZEN.load(Ordering::Relaxed) {
        return;
    }
    let held = lock();
    if !held {
        return;
    }
    unsafe {
        let bytes = text.as_bytes();
        let n = bytes.len().min(HEADER_CAP);
        let same = *addr_of!(HEADER_LEN) == n && core::slice::from_raw_parts(addr_of!(HEADER).cast::<u8>(), n) == &bytes[..n];
        if !same {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr_of_mut!(HEADER).cast(), n);
            *addr_of_mut!(HEADER_LEN) = n;
            if READY.load(Ordering::Relaxed) {
                let screen = &mut *addr_of_mut!(SCREEN);
                if !screen.ptr.is_null() {
                    draw_status(screen);
                }
            }
        }
    }
    unlock(held);
}

/// Copy one UART string onto the display. Safe to skip. Never writes the UART.
pub fn push(text: &str) {
    if FROZEN.load(Ordering::Relaxed) {
        return;
    }
    let held = lock();
    if !held {
        return;
    }
    if READY.load(Ordering::Relaxed) {
        paint(text.as_bytes());
    } else {
        stash(text.as_bytes());
    }
    // Noted after the paint returns. A painter that never returns leaves
    // the previous checkpoint to age, which is what the other cores' timer
    // reports. Noting on the way in made a stuck scroll look freshly alive.
    if crate::act::looping() && crate::percpu::live() {
        crate::act::note_core(crate::percpu::id());
    }
    unlock(held);
}

/// Draw `text` on one pinned row. The scrolling log is left alone. A refused
/// lock skips this update. Used from the timer for the heartbeat row.
pub fn paint_pinned_row(row: u32, text: &str) {
    if FROZEN.load(Ordering::Relaxed) || row >= PINNED_ROWS {
        return;
    }
    if !READY.load(Ordering::Relaxed) {
        return;
    }
    let held = lock();
    if !held {
        return;
    }
    unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        if !screen.ptr.is_null() {
            clear_row(screen, row);
            let color = ink(b"", screen.bgr);
            let cols = columns(screen);
            let mut col = 0usize;
            for &ch in text.as_bytes() {
                if ch == b'\n' || ch == b'\r' || col >= cols {
                    break;
                }
                draw_glyph(screen, col as u32, row, ch, color);
                col += 1;
            }
        }
    }
    unlock(held);
}

fn bring_up() -> bool {
    // The firmware already chose a mode for the rainbow. With the dtb
    // loaded and no vc4-kms overlay, that scanout is the one to keep.
    // Setting another size, or the blank tag, made this board drop HDMI
    // to no signal. Take the existing buffer first. Set that same size
    // only when the firmware will not hand the buffer over. A made-up
    // 1280x720 is not a fallback: it is the call that removes the signal.
    let mut attempt = 0;
    while attempt < 4 {
        if let Some((w, h)) = current_size() {
            if let Some(mode) = keep_mode(w, h) {
                return finish(mode);
            }
            if let Some(mode) = allocate(w, h) {
                return finish(mode);
            }
        }
        attempt += 1;
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
    if !fb_reachable(mode.pa, mode.bytes) {
        return false;
    }
    unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        screen.ptr = crate::addr::phys_to_virt(mode.pa) as *mut u8;
        screen.width = mode.width;
        screen.height = mode.height;
        screen.pitch = mode.pitch;
        screen.row = 0;
        screen.line_len = 0;
        screen.bgr = mode.bgr;
        screen.pa = mode.pa;
        screen.bytes = mode.bytes;
        screen.clean = true;
        clear_screen(screen);
        screen.row = PINNED_ROWS;
        if *addr_of!(HEADER_LEN) == 0 {
            let msg = b"NIC --- link --- mac --- dhcp --- ip ---\ntxS=- txC=- mibT=- mibR=-";
            core::ptr::copy_nonoverlapping(msg.as_ptr(), addr_of_mut!(HEADER).cast(), msg.len());
            *addr_of_mut!(HEADER_LEN) = msg.len();
        }
        draw_status(screen);
    }
    MODE_W.store(mode.width, Ordering::Relaxed);
    MODE_H.store(mode.height, Ordering::Relaxed);
    READY.store(true, Ordering::Release);
    true
}

/// The coarse map covers the kernel gigabyte as Normal write-back. A Pi 4
/// framebuffer address is in that gigabyte after the alias bits are
/// removed. Any other gigabyte gets a Normal block so the clear is stored
/// before the strict map exists.
fn fb_reachable(pa: u64, bytes: u64) -> bool {
    let Some(end) = pa.checked_add(bytes) else {
        return false;
    };
    if bytes == 0 || pa % 4096 != 0 {
        return false;
    }
    extern "C" {
        static __heap_end: u8;
        static mut early_l1_hi: u64;
    }
    let heap_end = crate::addr::virt_to_phys(core::ptr::addr_of!(__heap_end) as u64);
    if pa < heap_end || end > crate::board::RAM_END {
        return false;
    }
    let mut gb = pa >> 30;
    let last = (end - 1) >> 30;
    while gb <= last {
        let base = gb << 30;
        let index = gb as usize;
        if index >= 512 {
            return false;
        }
        unsafe {
            let slot = addr_of_mut!(early_l1_hi).cast::<u64>().add(index);
            let entry = slot.read_volatile();
            if entry & 1 == 0 {
                let desc = base | 1 | 0x300 | 0x400 | (0x40u64 << 48);
                slot.write_volatile(desc);
                civac(slot as *const u8, 8);
                core::arch::asm!(
                    "dsb ish",
                    "tlbi vmalle1is",
                    "dsb ish",
                    "isb",
                    options(nostack)
                );
            } else if (entry >> 2) & 7 != 0 {
                return false;
            }
        }
        gb += 1;
    }
    true
}

fn current_size() -> Option<(u32, u32)> {
    if let Some(size) = current_size_at(true) {
        return Some(size);
    }
    current_size_at(false)
}

fn current_size_at(bus_alias: bool) -> Option<(u32, u32)> {
    let mut msg = [0u32; 8];
    msg[0] = 32;
    msg[2] = TAG_GET_PHYS;
    msg[3] = 8;
    if !exchange(&mut msg, bus_alias) || msg[1] != REQ_OK {
        return None;
    }
    let width = msg[5];
    let height = msg[6];
    if (320..=3840).contains(&width) && (240..=2160).contains(&height) {
        Some((width, height))
    } else {
        None
    }
}

fn keep_mode(width: u32, height: u32) -> Option<Mode> {
    if let Some(mode) = keep_mode_at(width, height, true) {
        return Some(mode);
    }
    keep_mode_at(width, height, false)
}

fn keep_mode_at(width: u32, height: u32, bus_alias: bool) -> Option<Mode> {
    let mut msg = [0u32; 40];
    let mut n = 1usize;
    msg[n] = 0;
    n += 1;
    n = tag2(&mut msg, n, TAG_SET_OFFSET, 0, 0);
    let alloc_at = n;
    n = tag2(&mut msg, n, TAG_ALLOC, 4096, 0);
    let pitch_at = n;
    n = tag1(&mut msg, n, TAG_PITCH, 0);
    let depth_at = n;
    n = tag1(&mut msg, n, TAG_GET_DEPTH, 0);
    let order_at = n;
    n = tag1(&mut msg, n, TAG_GET_ORDER, 0);
    msg[n] = 0;
    n += 1;
    msg[0] = (n * 4) as u32;
    if !exchange(&mut msg[..n], bus_alias) || msg[1] != REQ_OK {
        return None;
    }
    let depth = msg[depth_at + 3];
    let order = msg[order_at + 3];
    let bus = msg[alloc_at + 3];
    let gpu_bytes = msg[alloc_at + 4] as u64;
    let pitch = msg[pitch_at + 3];
    if depth != 32 || width < CELL_W || pitch < width.saturating_mul(4) || bus == 0 {
        return None;
    }
    let bytes = (pitch as u64).checked_mul(height as u64)?;
    if gpu_bytes < bytes {
        return None;
    }
    let pa = (bus & 0x3FFF_FFFF) as u64;
    if pa % 4096 != 0 {
        return None;
    }
    Some(Mode {
        width,
        height,
        pitch,
        pa,
        bytes,
        bgr: order == 0,
    })
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
    // Pi 4 aliases live in bits 31:30. 0xC0000000 is the uncached alias of
    // the low 1 GiB. QEMU's property channel returns a GPU-bus address in
    // one of those aliases, and the low 30 bits are the ARM address the
    // scanout actually reads. Masking a plain low address changes nothing.
    let pa = (bus & 0x3FFF_FFFF) as u64;
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
        // Invalidate only. dc civac would write a dirty line back on
        // top of the GPU's reply.
        drop_lines(virt as *const u8, msg.len() * 4);
        for (i, word) in msg.iter_mut().enumerate() {
            *word = dst.add(i).read_volatile();
        }
    }
    true
}

fn mbox_send(msg: u32, start: u64) -> bool {
    let base = crate::addr::phys_to_virt(MBOX_BASE);
    let mut spins = 0u32;
    loop {
        spins = spins.saturating_add(1);
        if spins > 1_000_000 || timed_out(start) {
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
        spins = spins.saturating_add(1);
        if spins > 2_000_000 || timed_out(start) {
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

fn drop_lines(ptr: *const u8, len: usize) {
    let start = (ptr as usize) & !63;
    let end = (ptr as usize).saturating_add(len);
    let mut line = start;
    while line < end {
        unsafe {
            core::arch::asm!("dc ivac, {line}", line = in(reg) line, options(nostack));
        }
        line = line.saturating_add(64);
    }
    unsafe {
        core::arch::asm!("dsb sy", options(nostack));
    }
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
    // The caller held IRQs off for the whole copy. A multi-megabyte
    // framebuffer scroll then stops this core's timer, so the other
    // cores cannot see it as stalled. Give the timer back when the
    // caller had IRQs enabled.
    relax_irq();
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
    let clean = unsafe {
        let screen = &mut *addr_of_mut!(SCREEN);
        if screen.row < PINNED_ROWS {
            screen.row = PINNED_ROWS;
        }
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
        screen.clean
    };
    if clean {
        unsafe {
            core::arch::asm!("dsb sy", options(nostack));
        }
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

fn draw_status(screen: &mut Screen) {
    let text = unsafe {
        let n = *addr_of!(HEADER_LEN);
        core::slice::from_raw_parts(addr_of!(HEADER).cast::<u8>(), n)
    };
    let pitch = screen.pitch as usize;
    let row_bytes = pitch * CELL_H as usize;
    let rows_bytes = row_bytes * STATUS_ROWS as usize;
    let mut off = 0;
    while off + 4 <= rows_bytes {
        unsafe {
            (screen.ptr.add(off) as *mut u32).write_volatile(0);
        }
        off += 4;
    }
    let color = ink(b"", screen.bgr);
    let cols = columns(screen);
    let mut row = 0u32;
    let mut col = 0usize;
    for &ch in text {
        if ch == b'\n' || ch == b'\r' {
            row += 1;
            col = 0;
            if row >= STATUS_ROWS {
                break;
            }
            continue;
        }
        if col >= cols {
            continue;
        }
        draw_glyph(screen, col as u32, row, ch, color);
        col += 1;
    }
}

fn scroll(screen: &Screen) {
    let pitch = screen.pitch as usize;
    let row_bytes = pitch * CELL_H as usize;
    let total = pitch * screen.height as usize;
    if row_bytes == 0 || total <= row_bytes * (PINNED_ROWS as usize + 1) {
        return;
    }
    let mut src = row_bytes * (PINNED_ROWS as usize + 1);
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
    if screen.clean {
        civac(screen.ptr, total);
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
    if screen.clean {
        civac(screen.ptr, total);
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
        let px = screen.ptr.add(off) as *mut u32;
        px.write_volatile(color);
        if screen.clean {
            core::arch::asm!("dc civac, {px}", px = in(reg) px, options(nostack));
        }
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
    } else if has_prefix(line, b"[FAIL]")
        || has_prefix(line, b"[ERROR]")
        || has_prefix(line, b"!!!")
        || has_prefix(line, b"=== EXCEPTION")
        || has_prefix(line, b"elfault")
        || has_prefix(line, b"kfault")
        || has_prefix(line, b"STACK OVERFLOW")
        || has_prefix(line, b"stack overflow")
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

fn clear_row(screen: &Screen, row: u32) {
    let pitch = screen.pitch as usize;
    let row_bytes = pitch * CELL_H as usize;
    let mut off = row as usize * row_bytes;
    let end = off + row_bytes;
    while off + 4 <= end {
        unsafe {
            (screen.ptr.add(off) as *mut u32).write_volatile(0);
        }
        off += 4;
    }
}

/// DAIF.I is bit 7. Clear it when the console caller had IRQs enabled.
fn relax_irq() {
    let saved = unsafe { *addr_of!(SAVED_DAIF) };
    if saved & 0x80 == 0 {
        unsafe {
            core::arch::asm!("msr daifclr, #2", options(nomem, nostack));
        }
    }
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
