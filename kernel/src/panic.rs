use core::fmt::Write;
use core::panic::PanicInfo;

struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        crate::console::write_str(s);
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let _ = writeln!(UartWriter, "\r\n!!! KERNEL PANIC !!!\r\n{}\r\n", info);
    loop {
        cortex_a::asm::wfe();
    }
}
