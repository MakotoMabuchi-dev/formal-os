// src/kernel/mod.rs
// 将来 formal verification の対象となるロジックを集約する層。

use bootloader::BootInfo;
use crate::{arch, logging};

pub fn start(_boot_info: &'static BootInfo) {
    logging::info("kernel::start()");
    arch::halt_loop();
}
