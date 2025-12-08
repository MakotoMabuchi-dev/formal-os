#![no_std]
#![no_main]

// ─────────────────────────────────────────────
// formal-os: pre-formal verification kernel
//
// - フォーマル検証しやすい構造だけをカーネルに含める
// - unsafe は arch/mm の内部に閉じ込める
// ─────────────────────────────────────────────

mod arch;
mod logging;
mod kernel;
mod panic;
mod mm;    // ★ 物理メモリ管理モジュールを追加
mod mem;

use bootloader::{entry_point, BootInfo};

entry_point!(kernel_main);

fn kernel_main(boot_info: &'static BootInfo) -> ! {
    // ログ出力やアーキ依存初期化
    logging::init();
    arch::init(boot_info);

    logging::info("formal-os: kernel_main start");

    // フォーマル検証対象になりうる本体ロジックへ移譲
    kernel::start(boot_info);

    // ここへ戻ってくることは基本的に想定しないが、
    // 万一戻ってきても CPU を停止させておく。
    arch::halt_loop()
}
