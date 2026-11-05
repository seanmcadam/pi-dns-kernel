use crate::uart_pl011::Pl011Uart;
use core::fmt::Write;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut uart = Pl011Uart::new();
    uart.init();
    writeln!(uart, "\r\n!!! KERNEL PANIC !!!\r").ok();
    writeln!(uart, "{}\r", info).ok();
    
    loop {
        cortex_a::asm::wfe();
    }
}
