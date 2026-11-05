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

    writeln!(uart, "[1] Starting memory allocator...\r").ok();
    // Initialize memory allocator
    unsafe {
        ALLOCATOR.init();
    }
    writeln!(uart, "[OK] Memory allocator initialized\r").ok();
    
    writeln!(uart, "[2] Initializing logger...\r").ok();
    // Initialize logger
    logger::init();
    log::info!("Logger initialized");
    
    writeln!(uart, "[3] Initializing timer...\r").ok();
    // Initialize timer
    timer::init();
    log::info!("Timer initialized");
    
    writeln!(uart, "[4] Initializing interrupts...\r").ok();
    // Initialize interrupts
    interrupts::init();
    log::info!("Interrupts initialized");
    
    writeln!(uart, "[5] Loading config...\r").ok();
    // Load config
    let config = config_manager::load_config();
    log::info!("Network mode: {:?}", config.network.mode);
    log::info!("DNS records: {}", config.dns.records.len());
    
    writeln!(uart, "[6] Initializing network...\r").ok();
    // Initialize network
    log::info!("Initializing network...");
    match net::init(&config.network) {
        Ok(()) => {
            writeln!(uart, "[OK] Network initialized\r").ok();
            log::info!("Network initialized");
        }
        Err(e) => {
            writeln!(uart, "[ERROR] Network init failed: {:?}\r", e).ok();
            log::error!("Network init failed: {:?}", e);
            log::error!("Entering error loop...");
            loop {
                cortex_a::asm::wfe();
            }
        }
    }
    
    writeln!(uart, "[7] Initializing DNS server...\r").ok();
    // Initialize DNS server
    log::info!("Starting DNS server...");
    dns::init(&config.dns);
    
    writeln!(uart, "[8] Initializing HTTP server...\r").ok();
    // Initialize HTTP management interface
    log::info!("Starting HTTP server...");
    http::init(&config.http);
    
    writeln!(uart, "[9] Creating sockets...\r").ok();
    // Create sockets after network is initialized
    net::with_stack(|stack, _device| {
        writeln!(uart, "[9a] Creating DNS socket...\r").ok();
        match dns::init_sockets(stack) {
            Ok(()) => writeln!(uart, "[OK] DNS socket created\r").ok(),
            Err(e) => writeln!(uart, "[ERROR] DNS socket failed: {}\r", e).ok(),
        };
        
        writeln!(uart, "[9b] Creating HTTP socket...\r").ok();
        match http::init_sockets(stack) {
            Ok(()) => writeln!(uart, "[OK] HTTP socket created\r").ok(),
            Err(e) => writeln!(uart, "[ERROR] HTTP socket failed: {}\r", e).ok(),
        };
    });
    
    writeln!(uart, "\r\n").ok();
    writeln!(uart, "=== Kernel Initialized Successfully ===\r").ok();
    writeln!(uart, "DNS server: port 53\r").ok();
    writeln!(uart, "HTTP server: port 80\r").ok();
    writeln!(uart, "Ready for queries...\r").ok();
    writeln!(uart, "\r\n").ok();
    
    log::info!("DNS server listening on port 53");
    log::info!("HTTP server listening on port {}", config.http.port);
    log::info!("=== Kernel initialized successfully ===");
    log::info!("DNS Server ready");
    
    // Main event loop
    let mut counter = 0u64;
    loop {
        net::with_stack(|stack, device| {
            dns::poll(stack, device);
            http::poll(stack, device);
        });
        net::poll();
        
        counter += 1;
        if counter % 100000 == 0 {
            writeln!(uart, "[HEARTBEAT] Poll count: {}\r", counter).ok();
        }
    }
}
