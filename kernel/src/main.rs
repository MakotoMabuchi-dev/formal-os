// kernel/src/main.rs
#![no_std]
#![no_main]

// nightly: x86-interrupt ABI
#![feature(abi_x86_interrupt)]

// ─────────────────────────────────────────────
// formal-os: pre-formal verification kernel
//
// - フォーマル検証しやすい「状態機械 + 抽象イベント」中心にする
// - unsafe は arch 側に閉じ込め、kernel 側は状態遷移を明示する
// ─────────────────────────────────────────────

mod arch;
mod kernel;
mod logging;
mod mem;
mod mm;
mod panic;
mod types;

use bootloader::{entry_point, BootInfo};

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    logging::init();
    arch::init(boot_info);

    logging::info("formal-os: kernel_main start");

    // カーネル本体（low entry -> high-alias -> KernelState loop）
    kernel::start(boot_info);

    // 基本は戻らない想定だが、万一戻ってきても止める
    arch::halt_loop()
}
