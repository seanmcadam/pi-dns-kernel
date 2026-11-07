#![no_std]
#![no_main]

core::arch::global_asm!(include_str!("user.s"));

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
