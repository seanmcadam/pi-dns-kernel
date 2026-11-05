use core::fmt::Write;
use core::panic::PanicInfo;

struct UartWriter;

impl Write for UartWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for byte in s.bytes() {
            unsafe {
                core::ptr::write_volatile(0x0900_0000 as *mut u32, byte as u32);
            }
        }
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
