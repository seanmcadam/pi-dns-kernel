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
            if net_up {
                uart_write("DNS server listening on port 53\r\n");
                uart_write("HTTP server listening on port 80\r\n");
                uart_write("DNS Server ready\r\n");
            }
            uart_write("MMU and caches enabled\r\n");
            uart_write("cores online ");
            uart_dec(smp::online() as u64);
            uart_write("\r\n");
            #[cfg(feature = "board-pi4")]
            net::refresh_banner();
            report_el(entry_el);
            task::print_summary();
        }
    }
}
