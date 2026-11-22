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
/// `HDMI_HOTPLUG` is offset `0x1c`, bit 0. Neither the address nor the bit has
/// been confirmed on hardware. HDMI1 is the same block at `0x7ef05700`.
const HDMI0_HOTPLUG: u64 = 0xFEF0_071C;
const HDMI1_HOTPLUG: u64 = 0xFEF0_571C;
/// BCM2711 mailbox at `0x7E00B880`, same translation. Channel 8 is the property channel.
const MBOX_BASE: u64 = 0xFE00_B880;

use crate::board::{Board, IrqHit};
use crate::net::NetworkMode;

pub struct Pi4;

pub fn set_speed_cap(cap: u32) {
    genet::set_speed_cap(cap);
}

const DEVICE_RANGES: &[(u64, u64)] = &[(0xFC00_0000, 0x1_0000_0000)];

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

    fn power(reset: bool) -> bool {
        watchdog_power(reset)
    }

    fn power_tryboot() -> bool {
        // Linux sets this with the mailbox tag SET_REBOOT_FLAGS (bit 0)
        // before the watchdog reset. It is not a PM_RSTS partition bit.
        // The bootloader consumes the flag and loads tryboot.txt once.
        // A refused mailbox must not reset, or the machine would boot
        // config.txt while the caller was told this was a tryboot.
        if !set_reboot_flag(1) {
            return true;
        }
        let refused = watchdog_power(true);
        if refused {
            let _ = set_reboot_flag(0);
        }
        refused
    }

    fn tryboot_flag() -> Option<u32> {
        get_reboot_flag()
    }
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
    words[0] = 7 * 4;
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
    let ok = rc == 0 && words[1] == 0x8000_0000 && words[4] & 0x8000_0000 != 0;
    crate::disklog::line(if ok {
        "power: mbox 00038064 ok"
    } else {
        "power: mbox 00038064 refused"
    });
    ok
}

/// `GET_REBOOT_FLAGS`. `None` means the mailbox did not answer. The value
/// is the flag word. Bit 0 set means the firmware still has tryboot armed.
fn get_reboot_flag() -> Option<u32> {
    checkpoint();
    let mut words = [0u32; 8];
    words[0] = 7 * 4;
    words[2] = 0x0003_0064;
    words[3] = 4;
    let rc = crate::interrupts::with_irqs(|| {
        checkpoint();
        mbox::run(MBOX_BASE, &mut words)
    });
    checkpoint();
    if rc != 0 || words[1] != 0x8000_0000 || words[4] & 0x8000_0000 == 0 {
        return None;
    }
    Some(words[5])
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
        let rsts = if reset {
            rsts_raw & !PM_RSTS_HALT
        } else {
            (rsts_raw & !PM_RSTS_HALT) | PM_RSTS_HALT
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
    let a = crate::exception::read_u32_catching(dev_reg(HDMI0_HOTPLUG));
    let b = crate::exception::read_u32_catching(dev_reg(HDMI1_HOTPLUG));
    if a.is_none() && b.is_none() {
        return -1;
    }
    let connected = a.unwrap_or(0) & 1 != 0 || b.unwrap_or(0) & 1 != 0;
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
