#![no_std]
#![no_main]

extern crate alloc;

mod addr;
#[cfg(feature = "board-pi4")]
mod act;
mod board;
mod config_manager;
mod console;
mod cpu_stat;
mod dhcp_rx;
mod dns;
mod exception;
mod fault_log;
#[cfg(feature = "board-pi4")]
mod fb_con;
mod fs;
mod http;
mod gic;
mod interrupts;
mod mmu;
mod logger;
mod memory;
mod net;
mod panic;
mod percpu;
mod power;
mod smp;
mod spin;
mod supervisor;
mod task;
mod user_fd;
mod timer;

use memory::ALLOCATOR;

core::arch::global_asm!(include_str!("boot.s"));

// 1 only on the Pi 4 image. Assembly does not load this word. The linker
// script's BOOT_PI4_FLAG is patched into a movz, so a stale cache line
// cannot hide it and the load address does not matter.
#[used]
#[no_mangle]
#[cfg(feature = "board-pi4")]
pub static BOOT_PI4: u64 = 1;

#[used]
#[no_mangle]
#[cfg(not(feature = "board-pi4"))]
pub static BOOT_PI4: u64 = 0;

fn uart_write(s: &str) {
    console::write_str(s);
}

fn uart_serial(s: &str) {
    console::write_serial(s);
}

fn current_el() -> u64 {
    let el: u64;
    unsafe {
        core::arch::asm!(
            "mrs {el}, CurrentEL",
            el = out(reg) el,
            options(nomem, nostack)
        );
    }
    el >> 2
}

fn report_el(entry_el: u64) {
    uart_write("Entered at EL");
    uart_dec(entry_el);
    uart_write("\r\nRunning at EL");
    uart_dec(current_el());
    uart_write("\r\n");
}

fn uart_dec(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 {
        uart_write("0");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    uart_write(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

fn uart_serial_dec(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    if n == 0 {
        uart_serial("0");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    uart_serial(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

unsafe extern "C" {
    /// Packed by `a72_cpu_setup`. Bit 0 ran, bit 1 CPUECTLR trapped,
    /// bit 2 SMPEN reads as 1, bit 3 CPUACTLR trapped, bit 4 prefetch off.
    static A72_RESULT: u64;
}

/// Cortex-A72 only. The boot probe reads CPUECTLR and CPUACTLR, at EL2
/// when the core is still there. This does not touch those registers:
/// an EL1 read is UNDEFINED unless ACTLR_EL2 and ACTLR_EL3 both allow it.
/// Pi 4 MIDR is 0x410FD083 (r0p3). The armstub leaves SMPEN set and does
/// not set CPUACTLR bit 32. QEMU's cortex-a72 keeps neither bit.
fn note_a72_smp() {
    let midr: u64;
    unsafe {
        core::arch::asm!("mrs {midr}, midr_el1", midr = out(reg) midr, options(nomem, nostack));
    }
    let arm = (midr >> 24) & 0xff == 0x41;
    let a72 = (midr >> 4) & 0xfff == 0xd08;
    if !arm || !a72 {
        return;
    }
    let result = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(A72_RESULT)) };
    #[cfg(feature = "board-pi4")]
    pin_a72(result, midr);
    if result & 1 == 0 {
        uart_write("[WARN] A72 probe did not run\r\n");
        return;
    }
    if result & 2 != 0 {
        uart_write("[WARN] A72 CPUECTLR trapped\r\n");
    } else if result & 4 != 0 {
        uart_write("[OK] A72 SMPEN\r\n");
    } else {
        uart_write("[WARN] A72 SMPEN clear\r\n");
    }
    let variant = (midr >> 20) & 0xf;
    let revision = midr & 0xf;
    if variant == 0 && revision <= 3 {
        if result & 8 != 0 {
            uart_write("[WARN] A72 CPUACTLR trapped\r\n");
        } else if result & 16 != 0 {
            uart_write("[OK] A72 no instruction prefetch\r\n");
        } else {
            uart_write("[WARN] A72 instruction prefetch still on\r\n");
        }
    }
}

/// Short form of the probe for the pinned HDMI row. The UART lines above
/// are also copied to the scrolling log, which has moved them off the
/// panel by the time the first status line is drawn.
#[cfg(feature = "board-pi4")]
fn pin_a72(result: u64, midr: u64) {
    let mut buf = [0u8; 32];
    let mut n = 0;
    let mut app = |text: &str| {
        for byte in text.bytes() {
            if n >= buf.len() {
                return;
            }
            buf[n] = byte;
            n += 1;
        }
    };
    app("A72 ");
    if result & 1 == 0 {
        app("none");
    } else if result & 2 != 0 {
        app("ectlr-trap");
    } else if result & 4 == 0 {
        app("smpen-clear");
    } else {
        app("smpen");
    }
    let variant = (midr >> 20) & 0xf;
    let revision = midr & 0xf;
    if result & 1 != 0 && variant == 0 && revision <= 3 {
        if result & 8 != 0 {
            app(" actlr-trap");
        } else if result & 16 != 0 {
            app(" pref-off");
        } else {
            app(" pref-ON");
        }
    }
    if let Ok(text) = core::str::from_utf8(&buf[..n]) {
        act::set_a72_text(text);
    }
}

#[no_mangle]
pub extern "C" fn kernel_main(entry_el: u64) -> ! {
    // The framebuffer is programmed before the UART and before the strict
    // map. A hang in either of those used to leave the firmware rainbow up.
    #[cfg(feature = "board-pi4")]
    {
        let _ = fb_con::start_early();
    }
    board::early_init();
    #[cfg(feature = "board-pi4")]
    fb_con::announce();

    uart_write("\r\n=== Pi4 DNS Server Kernel ===\r\n");
    uart_write("board ");
    uart_write(board::NAME);
    uart_write("\r\n");
    report_el(entry_el);

    uart_write("[0] Enabling MMU and caches...\r\n");
    mmu::init();
    #[cfg(feature = "board-pi4")]
    fb_con::switch_to_nc();
    uart_write("[OK] MMU and caches enabled\r\n");
    note_a72_smp();

    percpu::init_bsp();

    uart_write("[1] Starting memory allocator...\r\n");
    unsafe {
        ALLOCATOR.init();
    }
    uart_write("[OK] Memory allocator initialized\r\n");

    uart_write("[2] Initializing logger...\r\n");
    logger::init();
    uart_write("[OK] Logger initialized\r\n");

    uart_write("[3] Initializing timer...\r\n");
    timer::init();
    uart_write("[OK] Timer initialized\r\n");

    uart_write("[4] Initializing interrupts...\r\n");
    interrupts::init();
    uart_write("[OK] Interrupts initialized\r\n");

    uart_write("[4a] Starting secondary cores...\r\n");
    smp::boot(entry_el);
    uart_write("cores online ");
    uart_dec(smp::online() as u64);
    uart_write("\r\n");

    uart_write("[4b] Running EL0 tests...\r\n");
    task::run_boot_tests();
    uart_write("[OK] EL0 tests finished\r\n");

    uart_write("[5] Loading config...\r\n");
    match fs::init() {
        Ok(()) => uart_write("[OK] Filesystem mounted\r\n"),
        Err(_) => uart_write("[OK] No filesystem, using defaults\r\n"),
    }
    let config = config_manager::load_config();
    uart_write("[OK] Config loaded\r\n");

    uart_write(dhcp_rx::self_check_line());
    uart_write("\r\n");
    uart_write("[6] Initializing network...\r\n");
    // A missing NIC skips DNS, HTTP, and telnet. The log task still starts.
    // QEMU virt has a NIC. raspi4b has no GENET, and the Pi 5 image does
    // not drive RP1 Ethernet.
    let net_up = match net::init(&config.network) {
        Ok(()) => {
            uart_write("[OK] Network initialized\r\n");
            true
        }
        Err(_) => {
            uart_write("[WARN] No NIC, DNS HTTP and telnet not started\r\n");
            false
        }
    };

    if net_up {
        uart_write("[7] Initializing DNS server...\r\n");
        dns::init(&config.dns);
        uart_write("[OK] DNS server initialized\r\n");

        uart_write("[8] Initializing HTTP server...\r\n");
        http::init(&config.http);
        uart_write("[OK] HTTP server initialized\r\n");
    } else {
        uart_write("[7] Skipping DNS, HTTP, and telnet without a NIC\r\n");
    }

    uart_write("[9] Creating sockets...\r\n");
    match supervisor::launch(task::LOG_SLOT) {
        Ok(()) => uart_write("[OK] Log task running\r\n"),
        Err(()) => uart_write("[ERROR] Log task failed\r\n"),
    }
    if net_up {
        match supervisor::launch(task::DNS_SLOT) {
            Ok(()) => uart_write("[OK] DNS user program running\r\n"),
            Err(()) => uart_write("[ERROR] DNS user program failed\r\n"),
        }
        match supervisor::launch(task::HTTP_SLOT) {
            Ok(()) => uart_write("[OK] HTTP socket created\r\n"),
            Err(()) => uart_write("[ERROR] HTTP socket failed\r\n"),
        }
        match supervisor::launch(task::TELNET_SLOT) {
            Ok(()) => uart_write("[OK] Telnet log listening on port 23\r\n"),
            Err(()) => uart_write("[ERROR] Telnet log failed\r\n"),
        }
        if config.display.vnc {
            match supervisor::launch(task::VNC_SLOT) {
                Ok(()) => uart_write("[OK] VNC task running\r\n"),
                Err(()) => uart_write("[WARN] VNC task failed\r\n"),
            }
        }

        uart_write("\r\n=== Kernel Initialized Successfully ===\r\n");
        uart_write("DNS server listening on port 53\r\n");
        uart_write("HTTP server listening on port 80\r\n");
        uart_write("DNS Server ready\r\n");
        uart_write("Ready for queries...\r\n\r\n");
    } else {
        uart_write("\r\n=== Kernel Initialized Without a NIC ===\r\n");
    }
    if config.display.hdmi && crate::board::hdmi_present() {
        match supervisor::launch(task::HDMI_SLOT) {
            Ok(()) => uart_write("[OK] HDMI task running\r\n"),
            Err(()) => uart_write("[WARN] HDMI task failed\r\n"),
        }
    }

    #[cfg(feature = "board-pi4")]
    net::refresh_banner();

    let mut next_status = timer::millis() + 2000;
    #[cfg(feature = "board-pi4")]
    act::enter_loop();
    loop {
        net::poll();
        task::poll_dns();
        task::poll_http();
        task::poll_log();
        task::poll_telnet();
        task::poll_spin();
        task::poll_display();
        let _ = task::schedule_one();
        net::poll();
        power::poll();
        supervisor::tick();

        let now = timer::millis();
        #[cfg(feature = "board-pi4")]
        act::note_progress(now);
        // The owner sets this while it is inside smoltcp. Two seconds
        // without a return is the livelock on the photo (IDLE stays 0,
        // the first RX is never reclaimed). Freeze the panel and stop.
        #[cfg(feature = "board-pi4")]
        if net::service::drive_age(now) > 2_000 {
            act::halt_with("owner stuck in NIC poll\nDHCP or RX callback did not return\nno reboot");
        }
        if now >= next_status {
            next_status = now + 2000;
            let (rx, tx) = net::packet_counts();
            let (nic, blk) = interrupts::irq_counts();
            let (busy, idle) = net::service::owner_time();
            uart_write("[status] rx=");
            uart_dec(rx as u64);
            uart_write(" tx=");
            uart_dec(tx as u64);
            uart_write(" ms=");
            uart_dec(now);
            uart_write(" nic=");
            uart_dec(nic);
            uart_write(" blk=");
            uart_dec(blk);
            uart_write(" busy=");
            uart_dec(busy);
            uart_write(" idle=");
            uart_dec(idle);
            uart_write("\r\n");
            // Repeat the lines the integration test greps for, in case the
            // serial client connects after the boot banner has already gone by.
            // Serial only: the same bytes on HDMI read as a second entry
            // into the boot path (EL2 after "DNS Server ready").
            if net_up {
                uart_serial("DNS server listening on port 53\r\n");
                uart_serial("HTTP server listening on port 80\r\n");
                uart_serial("DNS Server ready\r\n");
            }
            uart_serial("MMU and caches enabled\r\n");
            uart_serial("cores online ");
            uart_serial_dec(smp::online() as u64);
            uart_serial("\r\n");
            #[cfg(feature = "board-pi4")]
            net::refresh_banner();
            uart_serial("Entered at EL");
            uart_serial_dec(entry_el);
            uart_serial("\r\nRunning at EL");
            uart_serial_dec(current_el());
            uart_serial("\r\n");
            task::print_summary();
        }
    }
}
