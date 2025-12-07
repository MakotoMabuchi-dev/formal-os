// src/logging/mod.rs
// VGA だけを使った最小ログ基盤。

mod vga;

pub fn init() {
    vga::init();
}

pub fn info(msg: &str) {
    vga::write_line("[INFO] ");
    vga::write_line(msg);
}

pub fn error(msg: &str) {
    vga::write_line("[ERROR] ");
    vga::write_line(msg);
}
