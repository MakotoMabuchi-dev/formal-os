// src/arch/mod.rs
// アーキ依存部。unsafe をここに局所化する方針。

pub mod cpu;

use bootloader::BootInfo;

pub fn init(_boot_info: &'static BootInfo) {
    // 当面は何もしない
}

pub fn halt_loop() -> ! {
    cpu::halt_loop()
}
