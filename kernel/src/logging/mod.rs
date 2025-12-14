mod vga;
mod serial;

use core::sync::atomic::{AtomicBool, Ordering};

static VGA_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn init() {
    vga::init();
    serial::init();
}

pub fn set_vga_enabled(enabled: bool) {
    VGA_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn is_vga_enabled() -> bool {
    VGA_ENABLED.load(Ordering::Relaxed)
}

pub fn info(msg: &str) {
    vga::write_prefixed_line("[INFO] ", msg);
    serial::write_prefixed_line("[INFO] ", msg);
}

pub fn error(msg: &str) {
    vga::write_prefixed_line("[ERROR] ", msg);
    serial::write_prefixed_line("[ERROR] ", msg);
}

pub fn info_u64(label: &str, value: u64) {
    info_kv(label, value);
}

pub fn info_kv(key: &str, value: u64) {
    let mut buf = [0u8; 21];
    let s = u64_to_decimal(value, &mut buf);

    if key.is_empty() {
        vga::write_str("[INFO] ");
        vga::write_line(s);

        serial::write_str("[INFO] ");
        serial::write_line(s);
        return;
    }

    vga::write_str("[INFO] ");
    vga::write_str(key);
    vga::write_str(" = ");
    vga::write_line(s);

    serial::write_str("[INFO] ");
    serial::write_str(key);
    serial::write_str(" = ");
    serial::write_line(s);
}

fn u64_to_decimal(mut value: u64, buf: &mut [u8; 21]) -> &str {
    if value == 0 {
        let last = buf.len() - 1;
        buf[last] = b'0';
        return unsafe { core::str::from_utf8_unchecked(&buf[last..]) };
    }

    let mut i = buf.len();
    while value > 0 {
        let digit = (value % 10) as u8;
        i -= 1;
        buf[i] = b'0' + digit;
        value /= 10;
    }

    unsafe { core::str::from_utf8_unchecked(&buf[i..]) }
}
