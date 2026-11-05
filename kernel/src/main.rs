#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;

mod config_manager;
mod dns;
mod fs;
mod http;
mod interrupts;
mod logger;
mod mailbox;
mod memory;
mod net;
mod panic;
mod timer;
mod uart;

use core::fmt::Write;
use memory::ALLOCATOR;
use uart::Uart;

#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    // Initialize UART for early logging
    let mut uart = Uart::new();
    uart.init();

    writeln!(uart, "\r\n=== Pi4 DNS Server Kernel ===\r").ok();
    writeln!(uart, "Booting...\r").ok();

    // Initialize memory allocator
    unsafe {
        ALLOCATOR.init();
    }
    writeln!(uart, "[OK] Memory allocator initialized\r").ok();

    // Initialize logger
    logger::init();
    log::info!("Logger initialized");

    // Initialize timer
    timer::init();
    log::info!("Timer initialized");

    // Initialize interrupts
    interrupts::init();
    log::info!("Interrupts initialized");

    // Load configuration from SD card
    log::info!("Initializing filesystem...");
    match fs::init() {
        Ok(()) => log::info!("Filesystem ready"),
        Err(e) => log::warn!("Filesystem init failed: {:?}, using defaults", e),
    }

    // Load config
    let config = config_manager::load_config();
    log::info!("Network mode: {:?}", config.network.mode);

    // Initialize network
    log::info!("Initializing network...");
    match net::init(&config.network) {
        Ok(()) => log::info!("Network initialized"),
        Err(e) => {
            log::error!("Network init failed: {:?}", e);
            log::error!("DNS server cannot start without network");
        }
    }

    // Initialize DNS server
    log::info!("Starting DNS server...");
    dns::init(&config.dns);
    log::info!("DNS server listening on port 53");

    // Initialize HTTP management interface
    log::info!("Starting HTTP server...");
    http::init(&config.http);
    log::info!("HTTP server listening on port {}", config.http.port);

    log::info!("=== Kernel initialized successfully ===");
    log::info!("DNS Server ready");

    // Main event loop
    loop {
        net::poll();
    }
}
