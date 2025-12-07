#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let vga_buffer = 0xb8000 as *mut u8;

    let message = b"Hello formal-os!";
    let color: u8 = 0x0f;

    unsafe {
        for (i, &ch) in message.iter().enumerate() {
            *vga_buffer.add(i * 2) = ch;
            *vga_buffer.add(i * 2 + 1) = color;
        }
    }

    loop {}
}
