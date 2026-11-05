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
mod uart_pl011;

use core::fmt::Write;
use memory::ALLOCATOR;

#[no_mangle]
pub extern "C" fn kernel_main() -> ! {
    // Initialize UART for early logging (use PL011 for QEMU virt machine)
    let mut uart = uart_pl011::Pl011Uart::new();
    uart.init();
    
    writeln!(uart, "\r\n=== Pi4 DNS Server Kernel ===\r").ok();
    writeln!(uart, "Booting on QEMU virt machine...\r").ok();

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
    
    // Initialize HTTP management interface
    log::info!("Starting HTTP server...");
    http::init(&config.http);
    
    // Create sockets after network is initialized
    net::with_stack(|stack, _device| {
        dns::init_sockets(stack).expect("Failed to create DNS socket");
        http::init_sockets(stack).expect("Failed to create HTTP socket");
    });
    
    log::info!("DNS server listening on port 53");
    log::info!("HTTP server listening on port {}", config.http.port);
    
    log::info!("=== Kernel initialized successfully ===");
    log::info!("DNS Server ready");
    
    // Main event loop
    loop {
        net::with_stack(|stack, device| {
            dns::poll(stack, device);
            http::poll(stack, device);
        });
        net::poll();
    }
}
