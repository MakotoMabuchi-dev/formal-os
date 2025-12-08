// src/arch/mod.rs
//
// アーキ依存部。unsafe をここに局所化する方針。
// - cpu: hlt_loop など、CPU 固有の最低限の処理
// - paging: ページテーブル操作や CR3 周り（今後ここに集約する）

pub mod cpu;
pub mod paging;

use bootloader::BootInfo;

/// アーキ依存初期化処理。
/// - paging::init で BootInfo から得られる情報（physical_memory_offset）を保存しておく。
pub fn init(boot_info: &'static BootInfo) {
    paging::init(boot_info);
}

/// CPU を停止させるループ。
/// - カーネル終了時はここに来て HLT し続ける。
pub fn halt_loop() -> ! {
    cpu::halt_loop()
}
