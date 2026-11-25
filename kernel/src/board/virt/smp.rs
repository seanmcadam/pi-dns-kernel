//! PSCI `CPU_ON` for QEMU `virt`. The conduit is HVC.

const PSCI_CPU_ON: u64 = 0xC400_0003;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

pub fn start_secondary(cpu: usize, entry: u64, entry_el: u64) {
    // MPIDR_EL1 affinity fields only. Bit 24 (MT) and bit 31 are not an id.
    let hwid = mpidr_el1() & 0xff_00ff_ffff;
    let target = (hwid & !0xff) | (cpu as u64);
    let _ = entry_el;
    let ret = psci(PSCI_CPU_ON, target, entry, cpu as u64, true);
    if ret != 0 {
        crate::console::write_str("PSCI CPU_ON failed for core ");
        crate::console::write_str(digit(cpu));
        crate::console::write_str(" ret=");
        hex_out(ret);
        crate::console::write_str("\r\n");
    }
}

/// PSCI `SYSTEM_OFF` or `SYSTEM_RESET` through HVC. QEMU virt exits or resets
/// the guest. This does not return when the firmware accepts the call.
/// `true` means the guest is still running: EL2 swallowed HVC, or the call
/// came back. It never targets the host.
/// PSCI reset with no log lock. `true` means the guest is still running.
pub fn reset_now() -> bool {
    if el2_blocks_psci() {
        return true;
    }
    let _ = psci(PSCI_SYSTEM_RESET, 0, 0, 0, true);
    true
}

pub fn system_power(reset: bool) -> bool {
    if el2_blocks_psci() {
        crate::disklog::line("power: psci blocked");
        return true;
    }
    let fid = if reset {
        PSCI_SYSTEM_RESET
    } else {
        PSCI_SYSTEM_OFF
    };
    crate::disklog::line(if reset {
        "power: psci reset"
    } else {
        "power: psci off"
    });
    let ret = psci(fid, 0, 0, 0, true);
    crate::console::write_str("power: psci ");
    hex_out(ret);
    crate::console::write_str("\r\n");
    true
}

pub fn el2_blocks_psci() -> bool {
    let pfr: u64;
    unsafe {
        core::arch::asm!(
            "mrs {pfr}, id_aa64pfr0_el1",
            pfr = out(reg) pfr,
            options(nomem, nostack),
        );
    }
    let el2 = (pfr >> 8) & 0xf;
    el2 != 0 && el2 != 0xf
}

fn mpidr_el1() -> u64 {
    let value: u64;
    unsafe {
        core::arch::asm!(
            "mrs {value}, mpidr_el1",
            value = out(reg) value,
            options(nomem, nostack)
        );
    }
    value
}

fn psci(fid: u64, a1: u64, a2: u64, a3: u64, hvc: bool) -> u64 {
    let ret: u64;
    unsafe {
        if hvc {
            core::arch::asm!(
                "hvc #0",
                inlateout("x0") fid => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                out("x16") _,
                options(nostack),
            );
        } else {
            core::arch::asm!(
                "smc #0",
                inlateout("x0") fid => ret,
                in("x1") a1,
                in("x2") a2,
                in("x3") a3,
                out("x16") _,
                options(nostack),
            );
        }
    }
    ret
}

fn hex_out(n: u64) {
    let mut buf = [0u8; 18];
    let mut end = minfmt::push_bytes(&mut buf, 0, b"0x");
    end = minfmt::push_hex(&mut buf, end, n, 16);
    let text = unsafe { core::str::from_utf8_unchecked(&buf[..end]) };
    crate::console::write_str(text);
}

fn digit(n: usize) -> &'static str {
    match n {
        0 => "0",
        1 => "1",
        2 => "2",
        3 => "3",
        4 => "4",
        _ => "?",
    }
}
