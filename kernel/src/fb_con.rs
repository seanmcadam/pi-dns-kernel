//! Pi 4 HDMI text console.
//!
//! Every byte `console::write_str` sends to the PL011 is also drawn here,
//! once the framebuffer is up. Bytes from before that are kept in a fixed
//! buffer and replayed. The painter does not allocate, and it does not take
//! the scheduler, file, config, filesystem, or log locks. If this core
//! already holds the console, or another core does, the extra copy is
//! skipped. The UART write has already happened.
//!
//! The mailbox buffer is 64-byte aligned in `.dma.mbox`, which the linker
//! places ahead of every other DMA object. The coarse map is write-back,
//! so the whole buffer is cleaned before the call and invalidated after
//! it. A clean after the reply would write a dirty neighbor back over
//! the GPU's answer, which is why the buffer does not share a line.
//! The GPU bus address is the physical address with `0xC0000000` set. A
//! refused call is tried once more with the raw physical address. A
//! returned address whose top bits are a Pi 4 alias becomes an ARM
//! address with `bus & 0x3FFFFFFF`. The reply is read twice and both
//! copies are logged. A mismatch does not discard a reply: the copy a
//! single read would have kept is the one that is used. GET is accepted
//! the way the picture-producing path accepted it, on the response code
//! and a width and height in range. If that GET is unusable, the same
//! attempt runs the older exchange (message bytes only, one deciding
//! read) and then keep or same-size SET_PHYS. Pixels are stored through
//! the coarse map and the cache is cleaned, then the strict map moves
//! them to a non-cacheable window. A mailbox that never answers gives up
//! on a counter timeout and a spin budget and leaves the firmware
//! scanout alone.
//!
//! This has not been run on a Pi 4. QEMU raspi4b emulates the property
//! channel and the framebuffer.
//!
//! A soft reboot first puts the HDMI PHY and DVP in the state Linux leaves
//! them (`vc5_hdmi_phy_disable`, DVP reset pulsed, 108 MHz clocks left
//! running). A watchdog does not reset that block, and `PM_RSTC` full
//! reset cannot be made to look like a power-on: the hardware sets the
//! watchdog reason bits as the reset happens. The kernel then sends
//! `NOTIFY_REBOOT` (`0x00030048`, no payload), blanks and releases the
//! framebuffer, and powers off HDMI domain 6 and scaler domain 4. The
//! mailbox calls share a one-second budget. A miss does not stop the
//! watchdog. The register and mailbox words are written to `HDMISHUT.TXT`
//! so the next boot can show them in `hdmi_last_shut`. Halt and panic do
//! not do this: those paths do not reboot, and the fault text has to stay
//! on the panel.
//!
//! The legacy `SET_POWER_STATE` id 5 is I2C1, not HDMI. 0.1.69 cycled it
//! and the firmware refused it. 0.1.70 used the real domain ids and the
//! firmware accepted them, but EDID was still zeros after the watchdog
//! because the PHY and hot-plug block had not been reset. Pi 4's PM block
//! does not implement the old HDMI power switches.
//!
//! If the physical size is missing, the console resets the PHY and DVP,
//! cycles domains 6 and 4, reads EDID from the mailbox and from the HDMI
//! DDC block, and queries the size again. A size the firmware reports is
//! kept. A mode parsed from a real EDID block is tried before 1280x720
//! and 640x480. Blank is only sent on the way into a reset. None of this
//! runs on virt or Pi 5.

use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

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
/// Release the firmware framebuffer. No value bytes.
const TAG_RELEASE: u32 = 0x0004_8001;
/// Blank the firmware scanout. Value is 1 to blank.
const TAG_BLANK: u32 = 0x0004_0002;
/// One 128-byte EDID block. Value is the block number, then 128 bytes.
const TAG_GET_EDID: u32 = 0x0003_0020;
/// Linux sends this with no payload before it arms the watchdog.
const TAG_NOTIFY_REBOOT: u32 = 0x0003_0048;
/// New power-domain interface. Value is the firmware domain, then on/off.
const TAG_SET_DOMAIN: u32 = 0x0003_8030;
/// Firmware domain for HDMI. The device-tree index is 5; the mailbox
/// index is one higher. Legacy `SET_POWER_STATE` id 5 is I2C1.
const DOM_HDMI: u32 = 6;
/// Firmware domain for the HVS video scaler. The device-tree index is 3.
const DOM_SCALER: u32 = 4;

/// `/status` codes. 0 none, 1 up, 2 fallback mode, 3 released for reset.
const HDMI_NONE: u32 = 0;
const HDMI_UP: u32 = 1;
const HDMI_FALLBACK: u32 = 2;
const HDMI_RELEASED: u32 = 3;

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

const _: () = assert!(core::mem::align_of::<Prop>() == 64);
const _: () = assert!(core::mem::size_of::<Prop>() % 64 == 0);

#[link_section = ".dma.mbox"]
static mut PROP: Prop = Prop { words: [0; 48] };

const MBOX_TIMEOUT: u8 = 1;
const MBOX_ALIGN: u8 = 2;
const MBOX_MISMATCH: u8 = 3;
static MBOX_FAIL: AtomicU8 = AtomicU8::new(0);

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
static HDMI_STATE: AtomicU32 = AtomicU32::new(HDMI_NONE);
static HDMI_RECOVERIES: AtomicU32 = AtomicU32::new(0);
const DIAG_CAP: usize = 80;
static mut DIAG: [u8; DIAG_CAP] = [0; DIAG_CAP];
static DIAG_LEN: AtomicU8 = AtomicU8::new(0);
const SHUT_CAP: usize = 160;
static mut SHUT: [u8; SHUT_CAP] = [0; SHUT_CAP];
static SHUT_LEN: AtomicU8 = AtomicU8::new(0);
static EDID_W: AtomicU32 = AtomicU32::new(0);
static EDID_H: AtomicU32 = AtomicU32::new(0);
const SHUT_FILE: &str = "HDMISHUT.TXT";
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

/// state, width, height, recoveries. Width and height are the mode last
/// programmed, including a fallback. Recoveries count power-cycle attempts.
pub fn console_status() -> (u32, u32, u32, u32) {
    (
        HDMI_STATE.load(Ordering::Relaxed),
        MODE_W.load(Ordering::Relaxed),
        MODE_H.load(Ordering::Relaxed),
        HDMI_RECOVERIES.load(Ordering::Relaxed),
    )
}

/// Copy the latest mailbox summary. Empty means this boot has not recorded
/// one yet; the caller prints `none`.
pub fn copy_diag(out: &mut [u8]) -> usize {
    let n = DIAG_LEN.load(Ordering::Acquire) as usize;
    if n == 0 {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        return n;
    }
    let n = n.min(out.len()).min(DIAG_CAP);
    unsafe {
        core::ptr::copy_nonoverlapping(addr_of!(DIAG).cast::<u8>(), out.as_mut_ptr(), n);
    }
    n
}

/// Previous reboot's shutdown record. Empty until `HDMISHUT.TXT` is read
/// or this boot is about to reset.
pub fn copy_last_shut(out: &mut [u8]) -> usize {
    copy_text(
        out,
        addr_of!(SHUT).cast::<u8>(),
        SHUT_LEN.load(Ordering::Acquire) as usize,
        SHUT_CAP,
    )
}

/// Read `HDMISHUT.TXT` after the filesystem is mounted. A missing file
/// leaves `hdmi_last_shut` as `none`.
pub fn load_last_shut() {
    if !matches!(crate::fs::file_exists(SHUT_FILE), Ok(true)) {
        return;
    }
    let mut raw = [0u8; SHUT_CAP];
    let Ok(n) = crate::fs::read_named(SHUT_FILE, &mut raw) else {
        return;
    };
    set_shut(core::str::from_utf8(&raw[..n]).unwrap_or(""));
}

/// Reset the HDMI block, tell the firmware a restart is coming, and power
/// the HDMI and scaler domains off. IRQs stay unmasked. The mailbox calls
/// give up after one second. The watchdog still runs after this. The
/// summary is stored for `/status` and for `HDMISHUT.TXT`.
pub fn prepare_reboot() {
    report_shut("hdmi shut start", true);
    let hw = crate::hdmi_hw::quiesce();
    log_hw("hdmi shut hpd", &hw, true);
    log_hw_regs("hdmi shut reg", &hw, true);
    let start = crate::timer::get_ticks();
    let budget = crate::timer::frequency().max(1);
    let notify = shut_step(start, budget, &notify_msg());
    log_shut("hdmi shut notify", notify, true);
    let blank = shut_step(start, budget, &blank_msg(1));
    log_shut("hdmi shut blank", blank, true);
    let release = shut_step(start, budget, &release_msg());
    log_shut("hdmi shut release", release, false);
    let hdmi = shut_step(start, budget, &domain_msg(TAG_SET_DOMAIN, DOM_HDMI, 0));
    log_shut("hdmi shut d6", hdmi, false);
    let scaler = shut_step(start, budget, &domain_msg(TAG_SET_DOMAIN, DOM_SCALER, 0));
    log_shut("hdmi shut d4", scaler, false);
    let rsts = crate::hdmi_hw::pm_rsts();
    let mut note = Note::new();
    note.push("h0=");
    note.hex32(hw.h0);
    note.push(" h1=");
    note.hex32(hw.h1);
    note.push(" phy=");
    note.hex32(hw.phy0);
    note.push(" dvp=");
    note.hex32(hw.dvp);
    note.push(" clk=");
    note.hex32(hw.clk);
    note.push(" n=");
    note.hex32(notify.1);
    note.push(" b=");
    note.hex32(blank.1);
    note.push(" r=");
    note.hex32(release.1);
    note.push(" d6=");
    note.hex32(hdmi.1);
    note.push(":");
    note.hex32(hdmi.2);
    note.push(" d4=");
    note.hex32(scaler.1);
    note.push(":");
    note.hex32(scaler.2);
    note.push(" rsts=");
    note.hex32(rsts);
    let mut line = Line::new();
    line.push("n=");
    line.hex32(notify.1);
    line.push(" b=");
    line.hex32(blank.1);
    line.push(" r=");
    line.hex32(release.1);
    line.push(" d6=");
    line.hex32(hdmi.1);
    line.push(":");
    line.hex32(hdmi.2);
    line.push(" d4=");
    line.hex32(scaler.1);
    line.push(":");
    line.hex32(scaler.2);
    set_diag(line.text());
    set_shut(note.text());
    READY.store(false, Ordering::Release);
    HDMI_STATE.store(HDMI_RELEASED, Ordering::Relaxed);
    store_shut(note.text());
}

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

/// Info lines for the mailbox result. Call after the logger is installed.
/// The same text is posted when the log task starts, so telnet keeps it.
pub fn report() {
    crate::logger::replay_early();
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
            } else {
                save_line("hdmi nc map failed");
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
    // Setting another size, or the blank tag, while a size exists made
    // this board drop HDMI to no signal. Take the existing buffer first.
    // Set that same size when the firmware will not hand the buffer over.
    // A missing size (0x0, or no reply) is the only path that power-cycles
    // and, last, asks for a fallback mode.
    note_prop();
    let mut misses = 0u32;
    let mut attempt = 0u32;
    loop {
        TRY_N.store(attempt, Ordering::Relaxed);
        if let Some(fallback) = take_size(attempt) {
            if misses == 0 {
                set_diag("kept");
            }
            HDMI_STATE.store(
                if fallback { HDMI_FALLBACK } else { HDMI_UP },
                Ordering::Relaxed,
            );
            return true;
        }
        match display_console::hdmi_step(false, misses) {
            display_console::HdmiStep::Keep => return true,
            display_console::HdmiStep::PowerCycle => {
                HDMI_RECOVERIES.fetch_add(1, Ordering::Relaxed);
                recover_power();
                misses = 1;
            }
            display_console::HdmiStep::QueryAgain => {
                save_line("hdmi recover query");
                wait_ms(50);
                misses = 2;
            }
            display_console::HdmiStep::Fallback => {
                if ask_fallback(attempt) {
                    HDMI_STATE.store(HDMI_FALLBACK, Ordering::Relaxed);
                    return true;
                }
                misses = 3;
            }
            display_console::HdmiStep::Stop => break,
        }
        attempt = attempt.saturating_add(1);
    }
    save_line("hdmi none retry 4");
    false
}

/// `Some(true)` is a fallback mode. `Some(false)` kept the firmware size.
fn take_size(attempt: u32) -> Option<bool> {
    clear_probe();
    begin_capture();
    if let Some((w, h)) = current_size() {
        if finish_size(w, h, attempt, false) {
            return Some(false);
        }
    } else if let Some((w, h)) = current_size_pr50() {
        if finish_size(w, h, attempt, true) {
            return Some(false);
        }
    }
    emit_probe(attempt);
    dump_capture();
    None
}

fn ask_fallback(attempt: u32) -> bool {
    let edid_w = EDID_W.load(Ordering::Relaxed);
    let edid_h = EDID_H.load(Ordering::Relaxed);
    let mut sizes = [
        (0u32, 0u32),
        (display_console::HDMI_FALLBACK_W, display_console::HDMI_FALLBACK_H),
        (display_console::HDMI_LAST_W, display_console::HDMI_LAST_H),
    ];
    if edid_w != 0 && edid_h != 0 {
        sizes[0] = (edid_w, edid_h);
    }
    for (w, h) in sizes {
        if w == 0 || h == 0 {
            continue;
        }
        let mut line = Line::new();
        if w == edid_w && h == edid_h {
            line.push("hdmi edid mode ");
        } else {
            line.push("hdmi fallback ");
        }
        line.dec(w as u64);
        line.push("x");
        line.dec(h as u64);
        save_line(line.text());
        if finish_size(w, h, attempt, false) {
            return true;
        }
    }
    false
}

fn recover_power() {
    // Only reached when the firmware reported no size. Reset the PHY
    // while the domain is still on, cycle domains 6 and 4, then release
    // the PHY and read DDC. Legacy SET_POWER_STATE id 5 is I2C1.
    let pre = crate::hdmi_hw::quiesce();
    log_hw("hdmi recover hpd", &pre, false);
    let d6_off = run_tag(&domain_msg(TAG_SET_DOMAIN, DOM_HDMI, 0));
    log_boot("hdmi recover d6 off", d6_off);
    wait_ms(20);
    let d6_on = run_tag(&domain_msg(TAG_SET_DOMAIN, DOM_HDMI, 1));
    log_boot("hdmi recover d6 on", d6_on);
    wait_ms(50);
    let d4_off = run_tag(&domain_msg(TAG_SET_DOMAIN, DOM_SCALER, 0));
    log_boot("hdmi recover d4 off", d4_off);
    wait_ms(20);
    let d4_on = run_tag(&domain_msg(TAG_SET_DOMAIN, DOM_SCALER, 1));
    log_boot("hdmi recover d4 on", d4_on);
    wait_ms(50);
    let post = crate::hdmi_hw::boot_recover();
    log_hw("hdmi recover hpd2", &post, false);
    log_hw_regs("hdmi recover reg", &post, false);
    let edid = log_edid();
    if edid.2 != 0 && edid.3 != 0 {
        EDID_W.store(edid.2, Ordering::Relaxed);
        EDID_H.store(edid.3, Ordering::Relaxed);
    } else if post.edid_w != 0 && post.edid_h != 0 {
        EDID_W.store(post.edid_w, Ordering::Relaxed);
        EDID_H.store(post.edid_h, Ordering::Relaxed);
    }
    let mut line = Line::new();
    line.push("d6=");
    line.hex32(d6_on.1);
    line.push(":");
    line.hex32(d6_on.2);
    line.push(" d4=");
    line.hex32(d4_on.1);
    line.push(":");
    line.hex32(d4_on.2);
    line.push(" e=");
    line.hex32(edid.0);
    line.push(":");
    line.hex32(edid.1);
    line.push(" h0=");
    line.hex32(post.h0);
    set_diag(line.text());
}

fn log_boot(label: &str, reply: (bool, u32, u32)) {
    let mut line = Line::new();
    line.push(label);
    line.push(" ");
    line.hex32(reply.1);
    line.push(" ");
    line.hex32(reply.2);
    save_line(line.text());
}

fn log_edid() -> (u32, u32, u32, u32) {
    let mut msg = [0u32; 40];
    msg[0] = 39 * 4;
    msg[2] = TAG_GET_EDID;
    msg[3] = 4 + 128;
    let (ok, reply) = exchange_tag(&msg[..39]);
    let mut block = [0u8; 128];
    if ok {
        let mut i = 0;
        while i < 32 {
            block[i * 4..i * 4 + 4].copy_from_slice(&reply[6 + i].to_le_bytes());
            i += 1;
        }
    }
    let timing = edid::preferred(&block);
    let header = timing.is_some();
    let code = reply[1];
    let first = reply[6];
    let mut line = Line::new();
    line.push(if header {
        "hdmi recover edid yes "
    } else {
        "hdmi recover edid no "
    });
    line.hex32(code);
    line.push(" ");
    line.hex32(first);
    save_line(line.text());
    let (w, h) = timing
        .map(|item| (u32::from(item.width), u32::from(item.height)))
        .unwrap_or((0, 0));
    (code, first, w, h)
}

fn notify_msg() -> [u32; 6] {
    let mut msg = [0u32; 6];
    msg[0] = 24;
    msg[2] = TAG_NOTIFY_REBOOT;
    msg
}

fn domain_msg(tag: u32, id: u32, on: u32) -> [u32; 8] {
    let mut msg = [0u32; 8];
    msg[0] = 32;
    msg[2] = tag;
    msg[3] = 8;
    msg[5] = id;
    msg[6] = on;
    msg
}

fn shut_step(start: u64, budget: u64, template: &[u32]) -> (bool, u32, u32) {
    crate::act::note_core(crate::percpu::id());
    if crate::timer::get_ticks().wrapping_sub(start) > budget {
        save_line("hdmi shut budget");
        return (false, 0, 0);
    }
    crate::interrupts::with_irqs(|| run_tag(template))
}

fn log_shut(label: &str, reply: (bool, u32, u32), draw: bool) {
    let mut line = Line::new();
    line.push(label);
    line.push(" ");
    line.hex32(reply.1);
    line.push(" ");
    line.hex32(reply.2);
    report_shut(line.text(), draw);
}

fn report_shut(text: &str, draw: bool) {
    save_line(text);
    crate::logger::announce(text);
    if !draw {
        return;
    }
    let held = lock();
    if held && READY.load(Ordering::Acquire) {
        paint(text.as_bytes());
        paint(b"\n");
    }
    unlock(held);
}

fn set_diag(text: &str) {
    store_text(text, addr_of_mut!(DIAG).cast(), DIAG_CAP, &DIAG_LEN);
}

fn set_shut(text: &str) {
    store_text(text, addr_of_mut!(SHUT).cast(), SHUT_CAP, &SHUT_LEN);
}

fn store_text(text: &str, dst: *mut u8, cap: usize, len: &AtomicU8) {
    let mut n = 0;
    unsafe {
        core::ptr::write_bytes(dst, 0, cap);
        for byte in text.bytes() {
            if n + 1 >= cap {
                break;
            }
            if byte == b'\n' || byte == b'\r' {
                break;
            }
            if !(byte.is_ascii_graphic() || byte == b' ') || byte == b'"' || byte == b'\\' {
                continue;
            }
            dst.add(n).write(byte);
            n += 1;
        }
    }
    len.store(n as u8, Ordering::Release);
}

fn copy_text(out: &mut [u8], src: *const u8, n: usize, cap: usize) -> usize {
    if n == 0 {
        let text = b"none";
        let n = text.len().min(out.len());
        out[..n].copy_from_slice(&text[..n]);
        return n;
    }
    let n = n.min(out.len()).min(cap);
    unsafe {
        core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), n);
    }
    n
}

fn store_shut(text: &str) {
    let bytes = text.as_bytes();
    let n = bytes.len().min(SHUT_CAP - 1);
    let mut raw = [0u8; SHUT_CAP];
    raw[..n].copy_from_slice(&bytes[..n]);
    raw[n] = b'\n';
    if crate::percpu::live() {
        crate::act::note_core(crate::percpu::id());
    }
    let saved = crate::interrupts::with_irqs(|| {
        crate::fs::write_named(SHUT_FILE, &raw[..n + 1], false).is_ok()
    });
    if crate::percpu::live() {
        crate::act::note_core(crate::percpu::id());
    }
    report_shut(
        if saved {
            "hdmi shut file ok"
        } else {
            "hdmi shut file fail"
        },
        false,
    );
}

fn log_hw(label: &str, ports: &crate::hdmi_hw::Ports, draw: bool) {
    let mut line = Line::new();
    line.push(label);
    line.push(" h0=");
    line.hex32(ports.h0);
    line.push(" h1=");
    line.hex32(ports.h1);
    line.push(" leg=");
    line.hex32(ports.legacy0);
    emit_hw(line.text(), draw);
}

fn log_hw_regs(label: &str, ports: &crate::hdmi_hw::Ports, draw: bool) {
    let mut line = Line::new();
    line.push(label);
    line.push(" phy=");
    line.hex32(ports.phy0);
    line.push(" dvp=");
    line.hex32(ports.dvp);
    line.push(" clk=");
    line.hex32(ports.clk);
    emit_hw(line.text(), draw);
    if ports.edid_w != 0 && ports.edid_h != 0 {
        let mut mode = Line::new();
        mode.push(label);
        mode.push(" ddc ");
        mode.dec(ports.edid_w as u64);
        mode.push("x");
        mode.dec(ports.edid_h as u64);
        emit_hw(mode.text(), draw);
    }
}

fn emit_hw(text: &str, draw: bool) {
    if draw {
        report_shut(text, true);
    } else {
        save_line(text);
    }
}

struct Note {
    buf: [u8; SHUT_CAP],
    n: usize,
}

impl Note {
    fn new() -> Self {
        Self {
            buf: [0; SHUT_CAP],
            n: 0,
        }
    }

    fn push(&mut self, text: &str) {
        for byte in text.bytes() {
            if self.n + 1 >= SHUT_CAP {
                break;
            }
            self.buf[self.n] = byte;
            self.n += 1;
        }
    }

    fn hex32(&mut self, value: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut shift = 28i32;
        while shift >= 0 {
            self.push(core::str::from_utf8(&[HEX[((value >> shift) & 0xf) as usize]]).unwrap_or("0"));
            shift -= 4;
        }
    }

    fn text(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.n]).unwrap_or("hdmi shut")
    }
}

fn blank_msg(state: u32) -> [u32; 7] {
    let mut msg = [0u32; 7];
    msg[0] = 28;
    msg[2] = TAG_BLANK;
    msg[3] = 4;
    msg[5] = state;
    msg
}

fn release_msg() -> [u32; 6] {
    let mut msg = [0u32; 6];
    msg[0] = 24;
    msg[2] = TAG_RELEASE;
    msg
}

/// `(ok, response code, value word)`. The value word is the state for a
/// domain tag and zero when the tag has no value. A timeout is code 0.
/// The second try uses the raw physical address after the bus alias is
/// refused. The template is restored first because a failed exchange
/// leaves the buffer half written.
fn run_tag(template: &[u32]) -> (bool, u32, u32) {
    let (ok, msg) = exchange_tag(template);
    let value = if template.len() > 6 { msg[6] } else { 0 };
    (ok, msg[1], value)
}

/// Last mailbox buffer, with the success bit. A timeout keeps the earlier
/// reply when the bus-alias try wrote one, so a refused tag still shows
/// its response code.
fn exchange_tag(template: &[u32]) -> (bool, [u32; 48]) {
    if template.is_empty() || template.len() > 48 {
        return (false, [0; 48]);
    }
    let mut msg = [0u32; 48];
    let n = template.len();
    msg[..n].copy_from_slice(template);
    if exchange(&mut msg[..n], true) && msg[1] == REQ_OK {
        return (true, msg);
    }
    let missed = msg;
    msg[..n].copy_from_slice(template);
    if exchange(&mut msg[..n], false) {
        return (msg[1] == REQ_OK, msg);
    }
    (false, missed)
}

fn wait_ms(ms: u64) {
    let freq = crate::timer::frequency().max(1);
    let start = crate::timer::get_ticks();
    let need = (freq / 1000).saturating_mul(ms);
    while crate::timer::get_ticks().wrapping_sub(start) < need {
        core::hint::spin_loop();
    }
}

fn finish_size(w: u32, h: u32, attempt: u32, pr50: bool) -> bool {
    if pr50 {
        PR50.store(true, Ordering::Relaxed);
    }
    let kept = match keep_mode(w, h) {
        Ok(mode) => {
            emit_probe(attempt);
            Some(mode)
        }
        Err(_) => {
            emit_probe(attempt);
            None
        }
    };
    let mode = if let Some(mode) = kept {
        Some(mode)
    } else {
        let mode = allocate(w, h);
        if mode.is_some() {
            emit_probe(attempt);
        }
        mode
    };
    PR50.store(false, Ordering::Relaxed);
    if let Some(mode) = mode {
        finish(mode, attempt)
    } else {
        false
    }
}

struct Mode {
    width: u32,
    height: u32,
    pitch: u32,
    pa: u64,
    bytes: u64,
    bgr: bool,
}

fn finish(mode: Mode, retry: u32) -> bool {
    if !fb_reachable(mode.pa, mode.bytes) {
        let mut line = Line::new();
        line.push("hdmi unreachable pa ");
        line.hex32(mode.pa as u32);
        save_line(line.text());
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
    let mut line = Line::new();
    line.push("hdmi fb ");
    line.dec(mode.width as u64);
    line.push("x");
    line.dec(mode.height as u64);
    line.push(" p");
    line.dec(mode.pitch as u64);
    line.push(" pa ");
    line.hex32(mode.pa as u32);
    line.push(" bytes ");
    line.dec(mode.bytes);
    line.push(" retry ");
    line.dec(retry as u64);
    save_line(line.text());
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

enum KeepFail {
    Silent,
    Rejected,
}

struct Probe {
    get_code: u32,
    size_word: u32,
    width: u32,
    height: u32,
    step: u8,
    step_code: u32,
    pa: u32,
    pitch: u32,
}

static mut PROBE: Probe = Probe {
    get_code: 0,
    size_word: 0,
    width: 0,
    height: 0,
    step: 0,
    step_code: 0,
    pa: 0,
    pitch: 0,
};

/// Which bring-up attempt is talking to the mailbox. The raw dump uses it.
static TRY_N: AtomicU32 = AtomicU32::new(0);
/// Raw mailbox words for the attempt in progress. Dumped only when that
/// attempt does not produce a framebuffer. A picture keeps the one-line
/// summary and leaves these unread.
static mut SHOT_FIRST: [[u32; 48]; 8] = [[0; 48]; 8];
static mut SHOT_SECOND: [[u32; 48]; 8] = [[0; 48]; 8];
static mut SHOT_TRY: [u32; 8] = [0; 8];
static mut SHOT_PHASE: [u8; 8] = [0; 8];
static mut SHOT_ALIAS: [bool; 8] = [false; 8];
static SHOT_N: AtomicU32 = AtomicU32::new(0);
/// `g` checked GET, `p` older GET, `k` keep, `s` same-size set.
static PHASE: AtomicU8 = AtomicU8::new(b'g');
/// The older exchange: do not zero the property buffer, and clean only
/// the message bytes before the send.
static PR50: AtomicBool = AtomicBool::new(false);

fn clear_probe() {
    unsafe {
        *addr_of_mut!(PROBE) = Probe {
            get_code: 0,
            size_word: 0,
            width: 0,
            height: 0,
            step: 0,
            step_code: 0,
            pa: 0,
            pitch: 0,
        };
    }
}

fn note_get(code: u32, size_word: u32, width: u32, height: u32) {
    unsafe {
        let probe = &mut *addr_of_mut!(PROBE);
        probe.get_code = code;
        probe.size_word = size_word;
        probe.width = width;
        probe.height = height;
    }
}

fn note_step(step: u8, code: u32, bus: u32, pitch: u32) {
    unsafe {
        let probe = &mut *addr_of_mut!(PROBE);
        probe.step = step;
        probe.step_code = code;
        probe.pa = bus & 0x3FFF_FFFF;
        probe.pitch = pitch;
    }
}

fn note_prop() {
    let virt = addr_of!(PROP) as u64;
    let phys = crate::addr::virt_to_phys(virt) as u32;
    let mut line = Line::new();
    line.push("hdmi prop ");
    line.hex32(phys);
    if (virt as usize) % 64 == 0 {
        line.push(" align 64");
    } else {
        line.push(" misaligned");
    }
    save_line(line.text());
}

fn emit_probe(try_n: u32) {
    let probe = unsafe { &*addr_of!(PROBE) };
    let mut line = Line::new();
    line.push("hdmi t");
    line.dec(try_n as u64);
    line.push(" get ");
    line.hex32(probe.get_code);
    line.push(" sz ");
    line.hex32(probe.size_word);
    line.push(" ");
    line.dec(probe.width as u64);
    line.push("x");
    line.dec(probe.height as u64);
    if probe.get_code == 0 {
        line.push(" ");
        line.push(fail_name());
    }
    if probe.step == 1 {
        line.push(" keep ");
        line.hex32(probe.step_code);
    } else if probe.step == 2 {
        line.push(" set ");
        line.hex32(probe.step_code);
    }
    if probe.pa != 0 {
        line.push(" pa ");
        line.hex32(probe.pa);
        line.push(" p");
        line.dec(probe.pitch as u64);
    } else if probe.step != 0 && probe.step_code == 0 {
        line.push(" ");
        line.push(fail_name());
    }
    save_line(line.text());
}

fn fail_name() -> &'static str {
    match MBOX_FAIL.load(Ordering::Relaxed) {
        MBOX_ALIGN => "align",
        MBOX_MISMATCH => "mismatch",
        MBOX_TIMEOUT => "timeout",
        _ => "no-reply",
    }
}

fn save_line(text: &str) {
    crate::logger::save_early(text);
}

struct Line {
    buf: [u8; 80],
    n: usize,
}

impl Line {
    fn new() -> Self {
        Self { buf: [0; 80], n: 0 }
    }

    fn push(&mut self, text: &str) {
        for byte in text.bytes() {
            if self.n >= self.buf.len() {
                break;
            }
            self.buf[self.n] = byte;
            self.n += 1;
        }
    }

    fn hex32(&mut self, value: u32) {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut tmp = [0u8; 8];
        let mut shift = 28i32;
        let mut i = 0;
        while i < 8 {
            tmp[i] = HEX[((value >> shift) & 0xf) as usize];
            shift -= 4;
            i += 1;
        }
        self.push(core::str::from_utf8(&tmp).unwrap_or("00000000"));
    }

    fn dec(&mut self, mut value: u64) {
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        if value == 0 {
            self.push("0");
            return;
        }
        while value > 0 && i > 0 {
            i -= 1;
            tmp[i] = b'0' + (value % 10) as u8;
            value /= 10;
        }
        self.push(core::str::from_utf8(&tmp[i..]).unwrap_or("0"));
    }

    fn text(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.n]).unwrap_or("hdmi log")
    }
}

fn current_size() -> Option<(u32, u32)> {
    PHASE.store(b'g', Ordering::Relaxed);
    if let Some(size) = current_size_at(true) {
        return Some(size);
    }
    current_size_at(false)
}

/// The exchange that produced a picture: the property buffer is not
/// wiped, and only the message bytes are cleaned before the send.
fn current_size_pr50() -> Option<(u32, u32)> {
    PR50.store(true, Ordering::Relaxed);
    PHASE.store(b'p', Ordering::Relaxed);
    let size = if let Some(size) = current_size_at(true) {
        Some(size)
    } else {
        current_size_at(false)
    };
    PR50.store(false, Ordering::Relaxed);
    size
}

fn current_size_at(bus_alias: bool) -> Option<(u32, u32)> {
    let mut msg = [0u32; 8];
    msg[0] = 32;
    msg[2] = TAG_GET_PHYS;
    msg[3] = 8;
    if !exchange(&mut msg, bus_alias) {
        note_get(0, 0, 0, 0);
        return None;
    }
    // The response code and the dimensions decide. A size word other
    // than 32 used to zero the logged width and skip keep and set while
    // the code was still success. The picture-producing path did not
    // look at that word.
    let width = msg[5];
    let height = msg[6];
    note_get(msg[1], msg[0], width, height);
    if msg[1] == REQ_OK && (320..=3840).contains(&width) && (240..=2160).contains(&height) {
        Some((width, height))
    } else {
        None
    }
}

fn keep_mode(width: u32, height: u32) -> Result<Mode, KeepFail> {
    let first = keep_mode_at(width, height, true);
    if first.is_ok() {
        return first;
    }
    let second = keep_mode_at(width, height, false);
    if second.is_ok() {
        return second;
    }
    match (&first, &second) {
        (Err(KeepFail::Rejected), _) | (_, Err(KeepFail::Rejected)) => Err(KeepFail::Rejected),
        _ => Err(KeepFail::Silent),
    }
}

fn keep_mode_at(width: u32, height: u32, bus_alias: bool) -> Result<Mode, KeepFail> {
    PHASE.store(b'k', Ordering::Relaxed);
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
    let sent = (n * 4) as u32;
    msg[0] = sent;
    if !exchange(&mut msg[..n], bus_alias) {
        note_step(1, 0, 0, 0);
        return Err(KeepFail::Silent);
    }
    let depth = msg[depth_at + 3];
    let order = msg[order_at + 3];
    let bus = msg[alloc_at + 3];
    let gpu_bytes = msg[alloc_at + 4] as u64;
    let pitch = msg[pitch_at + 3];
    note_step(1, msg[1], bus, pitch);
    if msg[1] != REQ_OK {
        return Err(KeepFail::Rejected);
    }
    if depth != 32 || width < CELL_W || pitch < width.saturating_mul(4) || bus == 0 {
        return Err(KeepFail::Rejected);
    }
    let Some(bytes) = (pitch as u64).checked_mul(height as u64) else {
        return Err(KeepFail::Rejected);
    };
    if gpu_bytes < bytes {
        return Err(KeepFail::Rejected);
    }
    let pa = (bus & 0x3FFF_FFFF) as u64;
    if pa % 4096 != 0 {
        return Err(KeepFail::Rejected);
    }
    Ok(Mode {
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
    PHASE.store(b's', Ordering::Relaxed);
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
    let sent = (n * 4) as u32;
    msg[0] = sent;
    if !exchange(&mut msg[..n], bus_alias) {
        note_step(2, 0, 0, 0);
        return None;
    }
    let got_w = msg[5];
    let got_h = msg[6];
    let depth = msg[15];
    let order = msg[19];
    let bus = msg[alloc_at + 3];
    let gpu_bytes = msg[alloc_at + 4] as u64;
    let pitch = msg[pitch_at + 3];
    note_step(2, msg[1], bus, pitch);
    if msg[1] != REQ_OK {
        return None;
    }
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
    MBOX_FAIL.store(0, Ordering::Relaxed);
    if msg.is_empty() || msg.len() > 48 {
        MBOX_FAIL.store(MBOX_TIMEOUT, Ordering::Relaxed);
        log_mbox(bus_alias, "badlen");
        return false;
    }
    let start = crate::timer::get_ticks();
    let pr50 = PR50.load(Ordering::Relaxed);
    unsafe {
        let prop = addr_of_mut!(PROP);
        let virt = prop as usize;
        if virt % 64 != 0 {
            MBOX_FAIL.store(MBOX_ALIGN, Ordering::Relaxed);
            log_mbox(bus_alias, "align");
            return false;
        }
        let dst = addr_of_mut!((*prop).words).cast::<u32>();
        let words = core::mem::size_of::<Prop>() / 4;
        // The older path wrote the message over whatever was already in
        // the buffer and cleaned only those bytes. Wiping all 192 bytes
        // stays the first try. A GET that still has no size retries
        // with the older write.
        if !pr50 {
            for i in 0..words {
                dst.add(i).write_volatile(0);
            }
        }
        for (i, word) in msg.iter().enumerate() {
            dst.add(i).write_volatile(*word);
        }
        let full = core::mem::size_of::<Prop>();
        let clean = if pr50 { msg.len() * 4 } else { full };
        civac(virt as *const u8, clean);
        let phys = crate::addr::virt_to_phys(virt as u64);
        let mut bus = phys as u32;
        if bus_alias {
            bus |= 0xC000_0000;
        }
        if !mbox_send((bus & !0xF) | 8, start) {
            MBOX_FAIL.store(MBOX_TIMEOUT, Ordering::Relaxed);
            log_mbox(bus_alias, "timeout");
            return false;
        }
        // Invalidate only. dc civac would write a dirty line back on
        // top of the GPU's reply. Both copies of the whole buffer are
        // logged. A mismatch does not fail the call: the first copy is
        // what one read would have returned, unless only the second
        // copy has a usable GET size.
        drop_lines(virt as *const u8, full);
        let mut first = [0u32; 48];
        for i in 0..words {
            first[i] = dst.add(i).read_volatile();
        }
        drop_lines(virt as *const u8, full);
        let mut second = [0u32; 48];
        for i in 0..words {
            second[i] = dst.add(i).read_volatile();
        }
        stash_exchange(bus_alias, &first, &second);
        let n = msg.len();
        let mut mismatch = false;
        let mut i = 0;
        while i < words {
            if first[i] != second[i] {
                mismatch = true;
                break;
            }
            i += 1;
        }
        if mismatch {
            MBOX_FAIL.store(MBOX_MISMATCH, Ordering::Relaxed);
            log_mbox(bus_alias, "mismatch");
        }
        let phase = PHASE.load(Ordering::Relaxed);
        let use_second = mismatch
            && (phase == b'g' || phase == b'p')
            && !dims_ok(&first)
            && dims_ok(&second);
        let chosen = if use_second { &second } else { &first };
        let mut j = 0;
        while j < n {
            msg[j] = chosen[j];
            j += 1;
        }
    }
    true
}

fn dims_ok(words: &[u32; 48]) -> bool {
    words[1] == REQ_OK && (320..=3840).contains(&words[5]) && (240..=2160).contains(&words[6])
}

fn begin_capture() {
    SHOT_N.store(0, Ordering::Relaxed);
}

fn stash_exchange(bus_alias: bool, first: &[u32; 48], second: &[u32; 48]) {
    let n = SHOT_N.load(Ordering::Relaxed) as usize;
    if n >= 8 {
        return;
    }
    unsafe {
        *addr_of_mut!(SHOT_FIRST[n]) = *first;
        *addr_of_mut!(SHOT_SECOND[n]) = *second;
        *addr_of_mut!(SHOT_TRY[n]) = TRY_N.load(Ordering::Relaxed);
        *addr_of_mut!(SHOT_PHASE[n]) = PHASE.load(Ordering::Relaxed);
        *addr_of_mut!(SHOT_ALIAS[n]) = bus_alias;
    }
    SHOT_N.store((n + 1) as u32, Ordering::Relaxed);
}

fn dump_capture() {
    let n = SHOT_N.load(Ordering::Relaxed) as usize;
    let mut i = 0;
    while i < n && i < 8 {
        unsafe {
            let try_n = *addr_of!(SHOT_TRY[i]);
            let phase = *addr_of!(SHOT_PHASE[i]);
            let alias = *addr_of!(SHOT_ALIAS[i]);
            let first = &*addr_of!(SHOT_FIRST[i]);
            let second = &*addr_of!(SHOT_SECOND[i]);
            log_read(try_n, phase, alias, b'a', first);
            log_read(try_n, phase, alias, b'b', second);
            log_tags(try_n, phase, alias, first);
        }
        i += 1;
    }
}

fn log_read(try_n: u32, phase: u8, bus_alias: bool, which: u8, words: &[u32; 48]) {
    let mut index = 0;
    while index < words.len() {
        let mut line = Line::new();
        dump_prefix(&mut line, try_n, phase, bus_alias, which);
        let mut n = 0;
        while n < 8 && index < words.len() {
            if n != 0 {
                line.push(" ");
            }
            line.hex32(words[index]);
            index += 1;
            n += 1;
        }
        save_line(line.text());
    }
}

fn log_tags(try_n: u32, phase: u8, bus_alias: bool, words: &[u32; 48]) {
    let mut i = 2usize;
    let mut guard = 0u32;
    while i + 2 < words.len() && guard < 8 {
        let tag = words[i];
        if tag == 0 {
            break;
        }
        let buf_len = words[i + 1];
        let resp = words[i + 2];
        let mut line = Line::new();
        dump_prefix(&mut line, try_n, phase, bus_alias, b't');
        line.hex32(tag);
        line.push(" n ");
        line.hex32(buf_len);
        line.push(" r ");
        line.hex32(resp);
        save_line(line.text());
        let bytes = (buf_len & 0xffff) as usize;
        let step = 3 + bytes.div_ceil(4);
        let Some(next) = i.checked_add(step) else {
            break;
        };
        if next >= words.len() {
            break;
        }
        i = next;
        guard += 1;
    }
}

fn log_mbox(bus_alias: bool, name: &str) {
    let mut line = Line::new();
    dump_prefix(
        &mut line,
        TRY_N.load(Ordering::Relaxed),
        PHASE.load(Ordering::Relaxed),
        bus_alias,
        b'x',
    );
    line.push(name);
    save_line(line.text());
}

fn dump_prefix(line: &mut Line, try_n: u32, phase: u8, bus_alias: bool, which: u8) {
    line.push("t");
    line.dec(try_n as u64);
    let mut tag = [0u8; 3];
    tag[0] = phase;
    tag[1] = if bus_alias { b'A' } else { b'R' };
    tag[2] = which;
    line.push(core::str::from_utf8(&tag).unwrap_or("gAx"));
    line.push(" ");
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
