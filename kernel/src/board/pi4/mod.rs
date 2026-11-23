//! Raspberry Pi 4 (BCM2711). QEMU `raspi4b` (QEMU 9 or newer) can boot this
//! image far enough to exercise the UART, the GIC-400, the spin table, and
//! EMMC2. GENET is not in that machine, so a revision read there fails and
//! the image continues without a NIC. On a Pi 4 the same image drives GENET.
//!
//! The link script is `link.ld` in this directory. The firmware loads the raw
//! image at physical `0x80000`. The link address is `0xFFFF000000080000`.

mod emmc;
mod genet;
mod hdmi_task {
    use core::ptr::{addr_of, addr_of_mut};
    include!("../hdmi_task.rs");
}
mod mbox {
    include!("../mailbox.rs");
}
mod smp;
mod uart;

/// HDMI0 at child `0x7ef00700`. The bcm2711 soc ranges add `0xFE000000 - 0x7E000000`.
/// Linux `vc5` `HDMI_HOTPLUG` is offset `0x1a8`, bit 0. The older sample at
/// `0x1c` is still read. HDMI1 is the same block at `0x7ef05700`. The 35-bit
/// alias of HDMI0 hot-plug is `0x4_7EF008A8`.
const HDMI0_HOTPLUG_LEGACY: u64 = 0xFEF0_071C;
const HDMI0_HOTPLUG: u64 = 0xFEF0_08A8;
const HDMI1_HOTPLUG_LEGACY: u64 = 0xFEF0_571C;
const HDMI1_HOTPLUG: u64 = 0xFEF0_58A8;
const HDMI0_HOTPLUG_HI: u64 = 0x4_7EF0_08A8;
/// BCM2711 mailbox at `0x7E00B880`, same translation. Channel 8 is the property channel.
const MBOX_BASE: u64 = 0xFE00_B880;

use crate::board::{Board, IrqHit};
use crate::net::NetworkMode;

pub struct Pi4;

pub fn set_speed_cap(cap: u32) {
    genet::set_speed_cap(cap);
}

const DEVICE_RANGES: &[(u64, u64)] = &[
    // Low-peripheral window. Mailbox, UART, GENET, EMMC, HDMI, HVS.
    (0xFC00_0000, 0x1_0000_0000),
    // Full 35-bit alias of the HDMI block (bus `0x7EE00000..0x7F000000`).
    // One 2 MiB device page. Reads that fault stay inside the probe.
    (0x4_7EE0_0000, 0x4_7F00_0000),
];

/// BCM2711 GIC-400 SPI 157 and 158. The driver arms these after a real
/// revision read. QEMU raspi4b never raises them.
const GENET_IRQ0: u32 = 32 + 157;
const GENET_IRQ1: u32 = 32 + 158;
/// BCM2711 EMMC2, SPI 126. QEMU raspi4b raises this line. Untested on hardware.
const EMMC_IRQ: u32 = 32 + 126;

impl Board for Pi4 {
    const NAME: &'static str = "pi4";
    const GICD_PHYS: u64 = 0xFF84_1000;
    const GICC_PHYS: u64 = 0xFF84_2000;
    const UART_PHYS: u64 = 0xFE20_1000;
    const TIMER_IRQ: u32 = 27;
    const RAM_START: u64 = 0;
    /// Stop at the peripheral window. Untested on hardware.
    const RAM_END: u64 = 0xFC00_0000;

    type Nic = genet::GenetDevice;
    type Disk = emmc::DiskDev;

    fn device_ranges() -> &'static [(u64, u64)] {
        DEVICE_RANGES
    }

    fn early_init() {
        uart::init_pi4();
    }

    fn start_secondary(cpu: usize, entry: u64, entry_el: u64) {
        smp::start_secondary(cpu, entry, entry_el);
    }

    fn el2_blocks_secondaries() -> bool {
        false
    }

    fn default_network() -> NetworkMode {
        default_network()
    }

    fn probe_nic() -> Option<Self::Nic> {
        genet::GenetDevice::new()
    }

    fn nic_name() -> &'static str {
        "GENET"
    }

    fn packet_counts() -> (u32, u32) {
        genet::packet_counts()
    }

    fn packet_bytes() -> (u64, u64) {
        genet::packet_bytes()
    }

    fn rx_pending() -> bool {
        genet::rx_pending()
    }

    fn rx_load() -> (u64, u64, u64) {
        genet::rx_load()
    }

    fn nic_faults() -> (u64, u64, u64) {
        genet::nic_faults()
    }

    fn retarget_nic(cpu: usize) {
        genet::retarget(cpu);
    }

    fn probe_disk() -> bool {
        emmc::probe()
    }

    fn disk() -> Self::Disk {
        emmc::DiskDev
    }

    fn service_nic() {
        genet::service_link();
    }

    fn arm_nic() {
        genet::rearm_irq();
    }

    fn nic_link() -> bool {
        genet::link_is_up()
    }

    fn write_nic_status(out: &mut [u8]) -> usize {
        genet::write_status(out)
    }

    fn write_nic_diag(out: &mut [u8]) -> usize {
        genet::write_diag(out)
    }

    fn handle_irq(id: u32) -> IrqHit {
        // Packet data is polled. RX masks the GENET summary so the level
        // line drops before EOI. The poll unmasks it. The GIC line stays
        // enabled: disabling it and then forgetting the re-arm left the
        // interrupt dead for the rest of the boot.
        if id == GENET_IRQ0 || id == GENET_IRQ1 {
            let which = if id == GENET_IRQ0 { 0 } else { 1 };
            // TX done is cleared here and left unmasked. Waking on it made
            // every transmit schedule another poll, so the owner never slept.
            match genet::take_irq(which) {
                genet::NicIrq::Wake => return IrqHit::Net,
                genet::NicIrq::TxOnly => return IrqHit::None,
            }
        }
        if id == EMMC_IRQ {
            crate::gic::disable_id(id);
            return IrqHit::Block;
        }
        IrqHit::None
    }

    fn hdmi_present() -> bool {
        true
    }

    fn hdmi_pages() -> u32 {
        crate::board::HDMI_PAGES
    }

    fn hdmi_hpd() -> i64 {
        read_hotplug()
    }

    fn hdmi_mailbox(words: &mut [u32]) -> i64 {
        mbox::run(MBOX_BASE, words)
    }

    fn hdmi_image() -> Option<crate::board::HdmiImage> {
        Some(hdmi_task::image())
    }

    fn hdmi_code_ptr() -> *mut u8 {
        hdmi_task::code_ptr()
    }

    fn hdmi_code_bytes() -> usize {
        crate::board::HDMI_CODE_BYTES
    }

    fn hdmi_console() -> (u32, u32, u32, u32) {
        crate::fb_con::console_status()
    }

    fn hdmi_diag(out: &mut [u8]) -> usize {
        crate::fb_con::copy_diag(out)
    }

    fn hdmi_last_shut(out: &mut [u8]) -> usize {
        crate::fb_con::copy_last_shut(out)
    }

    fn hdmi_regs(out: &mut [u8]) -> usize {
        crate::fb_con::copy_regs(out)
    }

    fn hdmi_port(out: &mut [u8]) -> usize {
        crate::fb_con::copy_port(out)
    }

    fn hdmi_vclog(out: &mut [u8]) -> usize {
        crate::fb_con::copy_vclog(out)
    }

    fn hdmi_vclog_bytes() -> u32 {
        crate::fb_con::vclog_bytes()
    }

    fn hdmi_vclog_status(out: &mut [u8]) -> usize {
        crate::fb_con::copy_vclog_status(out)
    }

    fn hdmi_vclog_at(offset: u64, out: &mut [u8]) -> usize {
        crate::fb_con::copy_vclog_at(offset, out)
    }

    fn power(reset: bool) -> bool {
        watchdog_power(reset)
    }

    fn power_tryboot() -> bool {
        // Linux sets this with the mailbox tag SET_REBOOT_FLAGS (bit 0)
        // before the watchdog reset. It is not a PM_RSTS partition bit.
        // The next boot clears it before the firmware-log read. On Pi 4
        // rev 1.0 and 1.1 the flag is in EEPROM and survives power loss
        // until that clear. A refused mailbox must not reset, or the
        // machine would boot config.txt while the caller was told this
        // was a tryboot.
        if !set_reboot_flag(1) {
            return true;
        }
        let refused = watchdog_power(true);
        if refused {
            let _ = set_reboot_flag(0);
        }
        refused
    }

    fn disarm_tryboot() {
        disarm_tryboot_early();
    }

    fn tryboot_latched() -> u32 {
        TRY_LATCH.load(Ordering::Acquire)
    }

    fn tryboot_disarmed() -> bool {
        TRY_CLEAR.load(Ordering::Acquire) == 1
    }
}

/// Mailbox flag bit 0, and bit 1 when `PM_RSTS` partition bits were set.
static TRY_LATCH: AtomicU32 = AtomicU32::new(0);
/// 1 after `SET_REBOOT_FLAGS` 0 is accepted.
static TRY_CLEAR: AtomicU32 = AtomicU32::new(0);

use core::sync::atomic::{AtomicU32, Ordering};

fn disarm_tryboot_early() {
    let part = clear_partition_bits();
    let flag = reboot_mbox(0x0003_0064, None);
    let mut latch = 0u32;
    if flag.is_some_and(|word| word & 1 != 0) {
        latch |= 1;
    }
    if part != 0 {
        latch |= 2;
    }
    TRY_LATCH.store(latch, Ordering::Release);
    let cleared = reboot_mbox(0x0003_8064, Some(0)).is_some();
    TRY_CLEAR.store(u32::from(cleared), Ordering::Release);
    let rev = reboot_mbox(0x0001_0002, None);
    let mut line = [0u8; 72];
    let mut n = 0;
    push_bytes(&mut line, &mut n, b"tryboot flag ");
    match flag {
        Some(word) => push_hex(&mut line, &mut n, word),
        None => push_bytes(&mut line, &mut n, b"none"),
    }
    push_bytes(&mut line, &mut n, b" part ");
    push_hex(&mut line, &mut n, part);
    push_bytes(
        &mut line,
        &mut n,
        if cleared {
            b" clear ok"
        } else {
            b" clear fail"
        },
    );
    push_bytes(&mut line, &mut n, b" rev ");
    match rev {
        Some(word) => push_hex(&mut line, &mut n, word),
        None => push_bytes(&mut line, &mut n, b"none"),
    }
    if let Ok(text) = core::str::from_utf8(&line[..n]) {
        crate::disklog::line(text);
    }
}

/// Even bits 0, 2, 4, 6, 8 and 10 are the boot partition. Linux clears
/// them on every restart. A non-zero value left in the register is what
/// the bootloader reads on the next reset. This write does not touch
/// `PM_RSTC` or `PM_WDOG`, so it does not reset the board.
fn clear_partition_bits() -> u32 {
    let rsts_p = dev_reg(0xFE10_0000 + 0x20);
    let Some(rsts) = crate::exception::read_u32_catching(rsts_p) else {
        return 0;
    };
    let part = rsts & 0x555;
    if part != 0 {
        unsafe {
            core::ptr::write_volatile(rsts_p, 0x5a00_0000 | (rsts & 0xffff_faaa));
            core::arch::asm!("dsb sy", options(nostack));
        }
    }
    part
}

fn push_bytes(buf: &mut [u8], n: &mut usize, bytes: &[u8]) {
    for byte in bytes {
        if *n >= buf.len() {
            return;
        }
        buf[*n] = *byte;
        *n += 1;
    }
}

fn push_hex(buf: &mut [u8], n: &mut usize, value: u32) {
    let digits = b"0123456789abcdef";
    let raw = value.to_be_bytes();
    for byte in raw {
        if *n + 2 > buf.len() {
            return;
        }
        buf[*n] = digits[(byte >> 4) as usize];
        buf[*n + 1] = digits[(byte & 0xf) as usize];
        *n += 2;
    }
}

/// Property tag `tag`. `set` is the value word for a SET tag.
/// `Some` is a successful reply. The value is the reply word.
/// The buffer is 32 bytes so the length is 16-byte aligned. The firmware
/// accepts the reply when bit 31 of the buffer code and the tag code are set.
fn reboot_mbox(tag: u32, set: Option<u32>) -> Option<u32> {
    let mut words = [0u32; 8];
    words[0] = 8 * 4;
    words[2] = tag;
    words[3] = 4;
    if let Some(flag) = set {
        words[5] = flag;
    }
    let rc = mbox::run(MBOX_BASE, &mut words);
    if rc != 0 || words[1] & 0x8000_0000 == 0 || words[4] & 0x8000_0000 == 0 {
        return None;
    }
    Some(words[5])
}

/// VideoCore property tag `RPI_FIRMWARE_SET_REBOOT_FLAGS` (`0x00038064`).
/// The mailbox and the following watchdog run with IRQs unmasked and a
/// fresh stall checkpoint, so a slow property call cannot park the net core.
fn set_reboot_flag(flag: u32) -> bool {
    checkpoint();
    crate::disklog::line(if flag == 0 {
        "power: mbox 00038064 clear"
    } else {
        "power: mbox 00038064"
    });
    let mut words = [0u32; 8];
    words[0] = 8 * 4;
    words[1] = 0;
    words[2] = 0x0003_8064;
    words[3] = 4;
    words[4] = 0;
    words[5] = flag;
    words[6] = 0;
    let rc = crate::interrupts::with_irqs(|| {
        checkpoint();
        mbox::run(MBOX_BASE, &mut words)
    });
    checkpoint();
    let ok = rc == 0 && words[1] & 0x8000_0000 != 0 && words[4] & 0x8000_0000 != 0;
    crate::disklog::line(if ok {
        "power: mbox 00038064 ok"
    } else {
        "power: mbox 00038064 refused"
    });
    ok
}

/// BCM2711 power management at physical `0xFE100000` (bus `0x7E100000`).
///
/// PM_RSTC is `+0x1c`, PM_RSTS is `+0x20`, and PM_WDOG is `+0x24`. After
/// the MMU is on the CPU uses the kernel virtual form. A raw physical load
/// of PM_RSTS (`0xFE100020`) is a level-1 translation fault, and
/// `read_u32_catching` only resumes an external abort. The mailbox, UART,
/// GENET, EMMC, GIC, and HDMI mailbox already go through `phys_to_virt`.
/// HDMI hotplug uses the same helper. The watchdog sequence matches the
/// Linux Pi driver: partition bits `0x555` ask the firmware to halt, and a
/// full reset without those bits reboots. `WRCFG` `0x20` is
/// `PM_RSTC_WRCFG_FULL_RESET`, the same value Linux writes. The watchdog
/// reason bits in `PM_RSTS` are set by the reset itself, so this write
/// cannot make `start4.elf` treat the boot as a power-on. A missing
/// register is left alone.
/// `true` means the board is still up.
/// Watchdog full reset. Does not call the mailbox or the log lock.
pub fn crash_reset() -> bool {
    const PM_BASE: u64 = 0xFE10_0000;
    const PM_PASSWORD: u32 = 0x5a00_0000;
    const PM_RSTC_WRCFG_CLR: u32 = 0xffff_ffcf;
    const PM_RSTC_WRCFG_FULL_RESET: u32 = 0x20;
    let rsts_p = dev_reg(PM_BASE + 0x20);
    let rstc_p = dev_reg(PM_BASE + 0x1c);
    let wdog_p = dev_reg(PM_BASE + 0x24);
    unsafe {
        let rsts_raw = core::ptr::read_volatile(rsts_p);
        let rstc_raw = core::ptr::read_volatile(rstc_p);
        let rsts = rsts_raw & 0xffff_faaa;
        let rstc = (rstc_raw & PM_RSTC_WRCFG_CLR) | PM_PASSWORD | PM_RSTC_WRCFG_FULL_RESET;
        core::ptr::write_volatile(rsts_p, PM_PASSWORD | rsts);
        core::ptr::write_volatile(wdog_p, PM_PASSWORD | 10);
        core::ptr::write_volatile(rstc_p, rstc);
        core::arch::asm!("dsb sy", options(nostack));
    }
    let start = crate::timer::millis();
    while crate::timer::millis().saturating_sub(start) < 50 {
        core::hint::spin_loop();
    }
    true
}

fn watchdog_power(reset: bool) -> bool {
    // Halt is not a restart: Linux only notifies the firmware on reboot,
    // and a halt should leave the fault or status text on the panel.
    // A refused mailbox does not skip the watchdog.
    if reset {
        crate::fb_con::prepare_reboot();
    }
    const PM_BASE: u64 = 0xFE10_0000;
    const PM_PASSWORD: u32 = 0x5a00_0000;
    const PM_RSTC_WRCFG_CLR: u32 = 0xffff_ffcf;
    const PM_RSTC_WRCFG_FULL_RESET: u32 = 0x20;
    const PM_RSTS_HALT: u32 = 0x555;
    let rsts_p = dev_reg(PM_BASE + 0x20);
    let rstc_p = dev_reg(PM_BASE + 0x1c);
    let wdog_p = dev_reg(PM_BASE + 0x24);
    checkpoint();
    crate::disklog::line(if reset {
        "power: watchdog reset"
    } else {
        "power: watchdog halt"
    });
    let armed = crate::interrupts::with_irqs(|| {
        checkpoint();
        let Some(rsts_raw) = crate::exception::read_u32_catching(rsts_p) else {
            return false;
        };
        let Some(rstc_raw) = crate::exception::read_u32_catching(rstc_p) else {
            return false;
        };
        // Linux `bcm2835_restart`: `PM_RSTS & 0xfffffaaa`, then the
        // partition bits (0 for a reboot, `0x555` for a halt),
        // `PM_WDOG = password | 10`, `PM_RSTC = password | (rstc & ~0x30) | 0x20`.
        let rsts = if reset {
            rsts_raw & 0xffff_faaa
        } else {
            (rsts_raw & 0xffff_faaa) | PM_RSTS_HALT
        };
        let rstc = (rstc_raw & PM_RSTC_WRCFG_CLR) | PM_PASSWORD | PM_RSTC_WRCFG_FULL_RESET;
        unsafe {
            core::ptr::write_volatile(rsts_p, PM_PASSWORD | rsts);
            core::ptr::write_volatile(wdog_p, PM_PASSWORD | 10);
            core::ptr::write_volatile(rstc_p, rstc);
            core::arch::asm!("dsb sy", options(nostack));
        }
        true
    });
    if !armed {
        crate::disklog::line("power: watchdog missing");
        return true;
    }
    checkpoint();
    crate::disklog::line("power: watchdog armed");
    let deadline = crate::timer::millis().saturating_add(50);
    while crate::timer::millis() < deadline {
        checkpoint();
        core::hint::spin_loop();
    }
    true
}

fn dev_reg(pa: u64) -> *mut u32 {
    crate::addr::phys_to_virt(pa) as *mut u32
}

fn checkpoint() {
    crate::act::note_core(crate::percpu::id());
}

fn read_hotplug() -> i64 {
    let addrs = [
        HDMI0_HOTPLUG,
        HDMI1_HOTPLUG,
        HDMI0_HOTPLUG_LEGACY,
        HDMI1_HOTPLUG_LEGACY,
        HDMI0_HOTPLUG_HI,
    ];
    let mut saw = false;
    let mut connected = false;
    for pa in addrs {
        let Some(raw) = crate::exception::read_u32_catching(dev_reg(pa)) else {
            continue;
        };
        saw = true;
        connected |= raw & 1 != 0;
    }
    if !saw {
        return -1;
    }
    if connected {
        1
    } else {
        0
    }
}

fn default_network() -> NetworkMode {
    #[cfg(feature = "dhcp-default")]
    {
        NetworkMode::Dhcp {
            fallback_ip: [10, 0, 2, 15],
            fallback_netmask: [255, 255, 255, 0],
            fallback_gateway: [10, 0, 2, 2],
            timeout_ms: 4_000,
        }
    }
    #[cfg(not(feature = "dhcp-default"))]
    {
        NetworkMode::Static {
            ip: [10, 0, 2, 15],
            netmask: [255, 255, 255, 0],
            gateway: [10, 0, 2, 2],
        }
    }
}
