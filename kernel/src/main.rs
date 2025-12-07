#![no_std]
#![no_main]

// ───────────────────────────────────────────────────────────
// formal-os: pre-formal verification kernel (前身版)
//
// - フォーマル検証しやすい構造だけを残す
// - unsafe を arch 層に封じ込める
// - カーネルロジック(kernel/)は純粋で単純な形を維持する
// ───────────────────────────────────────────────────────────

mod arch;
mod logging;
mod kernel;
mod panic;

use bootloader::{entry_point, BootInfo};

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    logging::init();
    arch::init(boot_info);

    logging::info("formal-os: kernel_main start");

    kernel::start(boot_info);

    arch::halt_loop()
}
