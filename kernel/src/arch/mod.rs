// kernel/src/arch/mod.rs
//
// アーキ依存部。unsafe をできるだけここに閉じ込める方針。
// - cpu: hlt_loop など CPU 固有処理
// - paging: CR3 / ページテーブル操作
// - virt_layout: 仮想アドレスレイアウト（low/high, alias, user slot）定義の集約

pub mod cpu;
pub mod paging;
pub mod virt_layout;

use bootloader::BootInfo;

/// アーキ依存初期化処理
pub fn init(boot_info: &'static BootInfo) {
    paging::init(boot_info);
}

/// CPU を停止させるループ
pub fn halt_loop() -> ! {
    cpu::halt_loop()
}
