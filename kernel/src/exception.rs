//! Exception reports.
//!
//! `exception_handler` is the target of every vector in `exception.s` except
//! the IRQ and EL0 sync entries. It prints a fault report on the UART and
//! halts. It does not allocate: the fault may be inside the allocator. It
//! does not wait on a lock. One short line is copied into the log queue
//! only when that lock is free and the log task is up. The UART report is
//! the record that survives a halt. `LOG.TXT` is updated only if some other
//! core still runs the log task and drains the queue. A nested fault prints
//! one fixed line and halts, without walking the register frame again.

use core::ptr::addr_of;

core::arch::global_asm!(include_str!("exception.s"));

extern "C" {
    static __text_start: u8;
}

fn text_start() -> u64 {
    addr_of!(__text_start) as u64
}

fn guard_page() -> u64 {
    crate::mmu::guard_page()
}

fn put(s: &str) {
    crate::console::write_str(s);
}

fn hex64(mut value: u64) {
    put("0x");
    let mut buf = [0u8; 16];
    for slot in buf.iter_mut().rev() {
        let nibble = (value & 0xf) as u8;
        *slot = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
        value >>= 4;
    }
    put(unsafe { core::str::from_utf8_unchecked(&buf) });
}

fn reg(name: &str) -> u64 {
    let value: u64;
    unsafe {
        match name {
            "esr" => core::arch::asm!("mrs {}, esr_el1", out(reg) value),
            "elr" => core::arch::asm!("mrs {}, elr_el1", out(reg) value),
            "far" => core::arch::asm!("mrs {}, far_el1", out(reg) value),
            "spsr" => core::arch::asm!("mrs {}, spsr_el1", out(reg) value),
            _ => value = 0,
        }
    }
    value
}

fn vector_name(kind: u64) -> &'static str {
    match kind {
        0 => "Synchronous, current EL, SP0",
        1 => "IRQ, current EL, SP0 (stub, IRQs stay masked)",
        2 => "FIQ, current EL, SP0 (stub)",
        3 => "SError, current EL, SP0",
        4 => "Synchronous, current EL, SP_EL1",
        5 => "IRQ, current EL, SP_EL1 (stub, IRQs stay masked)",
        6 => "FIQ, current EL, SP_EL1 (stub)",
        7 => "SError, current EL, SP_EL1",
        8 => "Synchronous, lower EL, AArch64",
        9 => "IRQ, lower EL, AArch64 (stub, IRQs stay masked)",
        10 => "FIQ, lower EL, AArch64 (stub)",
        11 => "SError, lower EL, AArch64",
        12 => "Synchronous, lower EL, AArch32",
        13 => "IRQ, lower EL, AArch32 (stub, IRQs stay masked)",
        14 => "FIQ, lower EL, AArch32 (stub)",
        15 => "SError, lower EL, AArch32",
        _ => "Unknown vector",
    }
}

fn ec_name(ec: u64) -> &'static str {
    match ec {
        0x00 => "Unknown reason",
        0x01 => "Trapped WFI or WFE",
        0x07 => "Trapped SIMD or FP",
        0x0e => "Illegal execution state",
        0x15 => "SVC from AArch64",
        0x20 => "Instruction abort from a lower EL",
        0x21 => "Instruction abort from the current EL",
        0x22 => "PC alignment fault",
        0x24 => "Data abort from a lower EL",
        0x25 => "Data abort from the current EL",
        0x26 => "SP alignment fault",
        0x30 => "Breakpoint from a lower EL",
        0x31 => "Breakpoint from the current EL",
        0x3c => "BRK instruction",
        _ => "Unclassified",
    }
}

fn fault_status(dfsc: u64) -> &'static str {
    match dfsc {
        0b000100 | 0b000101 | 0b000110 | 0b000111 => "Translation fault",
        0b001001 | 0b001010 | 0b001011 => "Access flag fault",
        0b001101 | 0b001110 | 0b001111 => "Permission fault",
        0b000000 | 0b000001 | 0b000010 | 0b000011 => "Address size fault",
        0b100001 => "Alignment fault",
        0b010000 | 0b010100 => "External abort",
        _ => "Other fault status",
    }
}

fn iss_summary(ec: u64, iss: u64) {
    put("ISS: ");
    match ec {
        0x20 | 0x21 | 0x24 | 0x25 => {
            let dfsc = iss & 0x3f;
            if iss & (1 << 6) != 0 {
                put("write, ");
            } else {
                put("read, ");
            }
            put(fault_status(dfsc));
            put(" level ");
            let level = (dfsc & 0x3) as u8;
            let digit = [b'0' + level];
            put(unsafe { core::str::from_utf8_unchecked(&digit) });
        }
        _ => {
            put("raw ");
            hex64(iss);
        }
    }
    put("\r\n");
}

/// `exception.s` reads this. A non-zero value is the ELR to resume at after
/// one synchronous external abort, so a missing MMIO device can be probed.
#[no_mangle]
static mut PROBE_RECOVER: u64 = 0;

/// Read one word. An external abort returns `None` instead of halting.
///
/// The Pi 4 GENET probe is the caller. The virt image has no such probe.
#[allow(dead_code)]
///
/// Only the caller that stored `PROBE_RECOVER` is resumed. A fault on
/// another core during that window would resume there too, so this is for
/// the boot probe, before other cores run device code.
pub fn read_u32_catching(addr: *const u32) -> Option<u32> {
    let value: u32;
    let failed: u64;
    let recover: u64;
    unsafe {
        core::arch::asm!(
            "adr {recover}, 1f",
            "str {recover}, [{slot}]",
            "ldr {value:w}, [{addr}]",
            "str xzr, [{slot}]",
            "mov {failed}, xzr",
            "b 2f",
            "1:",
            "mov {failed}, #1",
            "mov {value:w}, wzr",
            "2:",
            addr = in(reg) addr,
            slot = in(reg) core::ptr::addr_of_mut!(PROBE_RECOVER),
            recover = out(reg) recover,
            value = out(reg) value,
            failed = out(reg) failed,
            out("x16") _,
            out("x17") _,
            options(nostack),
        );
    }
    let _ = recover;
    if failed == 0 {
        Some(value)
    } else {
        None
    }
}

/// Print the fault and stop. Called from `exception.s` with a saved frame.
#[no_mangle]
pub extern "C" fn exception_handler(frame: *const u64, kind: u64) -> ! {
    if crate::crash::busy() {
        put("\r\nnested fault log\r\n");
        halt();
    }
    if !crate::fault_log::enter() {
        put("\r\nnested fault log\r\n");
        halt();
    }
    let esr = reg("esr");
    let elr = reg("elr");
    let far = reg("far");
    let spsr = reg("spsr");
    let ec = (esr >> 26) & 0x3f;
    let iss = esr & 0x1ff_ffff;

    put("\r\n=== EXCEPTION ===\r\n");
    put("type: ");
    put(vector_name(kind));
    put("\r\nESR_EL1: ");
    hex64(esr);
    put("  EC=0x");
    let ec_byte = [hex_digit((ec >> 4) as u8), hex_digit((ec & 0xf) as u8)];
    put(unsafe { core::str::from_utf8_unchecked(&ec_byte) });
    put(" ");
    put(ec_name(ec));
    put("\r\n");
    iss_summary(ec, iss);
    put("ELR_EL1: ");
    hex64(elr);
    put("\r\nFAR_EL1: ");
    hex64(far);
    put("\r\nSPSR_EL1: ");
    hex64(spsr);
    let fault_sp = frame as u64 + 256;
    put("\r\nsp: ");
    hex64(fault_sp);
    put("\r\n");

    let frame = unsafe { core::slice::from_raw_parts(frame, 31) };
    let mut index = 0;
    while index < 31 {
        put("x");
        if index >= 10 {
            let tens = [b'0' + (index / 10) as u8];
            put(unsafe { core::str::from_utf8_unchecked(&tens) });
        }
        let ones = [b'0' + (index % 10) as u8];
        put(unsafe { core::str::from_utf8_unchecked(&ones) });
        put(": ");
        hex64(frame[index]);
        put("\r\n");
        index += 1;
    }
    let mut raw = [0u8; 80];
    let overflow = kernel_stack_overflow(far);
    let n = if overflow {
        crate::fault_log::write_stack_overflow(&mut raw, "kern", 0, ec, elr, far)
    } else {
        crate::fault_log::write_kfault(&mut raw, kind, ec, iss, elr, far)
    };
    if let Ok(line) = core::str::from_utf8(&raw[..n]) {
        put(line);
        put("\r\n");
        let _ = crate::logger::try_queue_fault(line);
    }
    put("halting\r\n");
    crate::crash::from_exception(esr, elr, far, spsr, fault_sp, frame);
    pin_and_halt(kind, ec, elr, far);
}

/// Leave the fault on the pinned HDMI rows and stop. Does not reboot.
fn kernel_stack_overflow(far: u64) -> bool {
    let page = far & !0xfff;
    page == guard_page()
        || crate::task::is_kstack_guard_va(far)
        || crate::smp::is_stack_guard_va(far)
}

fn pin_and_halt(kind: u64, ec: u64, elr: u64, far: u64) -> ! {
    let mut raw = [0u8; 180];
    let mut n = 0;
    if kernel_stack_overflow(far) {
        n = pin_push(&mut raw, n, b"STACK OVERFLOW kern");
    } else {
        n = pin_push(&mut raw, n, b"kern fault v=");
        n = pin_dec(&mut raw, n, kind);
    }
    n = pin_push(&mut raw, n, b"\nec=");
    n = pin_hex2(&mut raw, n, (ec & 0x3f) as u8);
    n = pin_push(&mut raw, n, b" elr ");
    n = pin_hex16(&mut raw, n, elr);
    n = pin_push(&mut raw, n, b"\nfar ");
    n = pin_hex16(&mut raw, n, far);
    let text = core::str::from_utf8(&raw[..n]).unwrap_or("kern fault");
    #[cfg(feature = "board-pi4")]
    crate::act::halt_with(text);
    #[cfg(not(feature = "board-pi4"))]
    {
        let _ = text;
        halt();
    }
}

fn pin_push(buf: &mut [u8], mut n: usize, bytes: &[u8]) -> usize {
    for byte in bytes {
        if n >= buf.len() {
            return n;
        }
        buf[n] = *byte;
        n += 1;
    }
    n
}

fn pin_hex2(buf: &mut [u8], n: usize, value: u8) -> usize {
    let digits = [hex_digit(value >> 4), hex_digit(value & 0xf)];
    pin_push(buf, n, &digits)
}

fn pin_hex16(buf: &mut [u8], n: usize, mut value: u64) -> usize {
    let mut tmp = [0u8; 16];
    let mut i = 16;
    while i > 0 {
        i -= 1;
        tmp[i] = hex_digit((value & 0xf) as u8);
        value >>= 4;
    }
    pin_push(buf, n, &tmp)
}

fn pin_dec(buf: &mut [u8], n: usize, mut value: u64) -> usize {
    let mut tmp = [0u8; 20];
    let mut i = 20;
    if value == 0 {
        i -= 1;
        tmp[i] = b'0';
    }
    while value > 0 && i > 0 {
        i -= 1;
        tmp[i] = b'0' + (value % 10) as u8;
        value /= 10;
    }
    pin_push(buf, n, &tmp[i..])
}

fn halt() -> ! {
    #[cfg(feature = "board-pi4")]
    crate::act::halt_blink();
    #[cfg(not(feature = "board-pi4"))]
    loop {
        unsafe {
            core::arch::asm!("wfe", options(nomem, nostack, preserves_flags));
        }
    }
}

fn hex_digit(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'a' + nibble - 10
    }
}

/// Authenticated debug path: read the unmapped page under the stack.
pub fn fault_guard() -> ! {
    put("debug fault: read stack guard\r\n");
    let addr = guard_page() as *const u8;
    unsafe {
        core::ptr::read_volatile(addr);
    }
    halt();
}

/// Authenticated debug path: store to kernel .text. The page is read-only
/// and execute-only from the data side, so this must be a permission fault.
pub fn fault_text() -> ! {
    put("debug fault: write kernel text\r\n");
    let addr = text_start() as *mut u8;
    unsafe {
        core::ptr::write_volatile(addr, 0);
    }
    halt();
}
