//! PSCI `CPU_ON` for the BCM2712. The device tree says `method = "smc"`
//! and `enable-method = "psci"`. There is no spin table.
//!
//! The CPU `reg` values are `0x000`, `0x100`, `0x200`, and `0x300`. Aff0 is
//! 0 on every core, so the target affinity is `cpu << 8`. The context id is
//! the linear cpu index, which `secondary_entry` receives in x0.
//!
//! This has not been run on a Pi 5. If the firmware enters at EL3, `boot.s`
//! parks the primary and secondaries are never started.

const PSCI_CPU_ON: u64 = 0xC400_0003;

pub fn start_secondary(cpu: usize, entry: u64, _entry_el: u64) {
    if cpu == 0 || cpu > 3 {
        return;
    }
    let target = (cpu as u64) << 8;
    let ret = psci_smc(PSCI_CPU_ON, target, entry, cpu as u64);
    if ret != 0 {
        crate::console::write_str("PSCI CPU_ON failed for core ");
        crate::console::write_str(digit(cpu));
        crate::console::write_str(" ret=");
        hex_out(ret);
        crate::console::write_str("\r\n");
    }
}

fn psci_smc(fid: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    unsafe {
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
    ret
}

fn digit(cpu: usize) -> &'static str {
    match cpu {
        1 => "1",
        2 => "2",
        3 => "3",
        _ => "?",
    }
}

fn hex_out(mut n: u64) {
    crate::console::write_str("0x");
    let mut buf = [0u8; 16];
    let mut i = 16;
    while i > 0 {
        i -= 1;
        let nib = (n & 0xf) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
        n >>= 4;
        if n == 0 {
            break;
        }
    }
    let text = core::str::from_utf8(&buf[i..]).unwrap_or("?");
    crate::console::write_str(text);
}
