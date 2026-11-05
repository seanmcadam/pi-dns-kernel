use crate::uart::Uart;
use core::fmt::Write;
use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut uart = Uart::new();
    writeln!(uart, "\r\n!!! KERNEL PANIC !!!\r").ok();
    writeln!(uart, "{}\r", info).ok();

    loop {
        cortex_a::asm::wfe();
    }
}
