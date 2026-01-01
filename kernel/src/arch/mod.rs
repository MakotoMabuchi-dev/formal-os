// kernel/src/arch/mod.rs
//
// アーキ依存部。unsafe をできるだけここに閉じ込める方針。
// - cpu: hlt_loop など CPU 固有処理
// - paging: CR3 / ページテーブル操作
// - virt_layout: 仮想アドレスレイアウト（low/high, alias, user slot）
// - interrupts: IDT, page fault など例外処理
// - gdt: GDT/TSS/IST
// - ring3: ring3 へ入るための最小 glue（iretq）
//
// 方針:
// - 例外が起きてもログが残るよう、割り込み初期化は早め。
// - paging を触る前に IDT を入れておく（デバッグしやすさ優先）。

pub mod cpu;
pub mod interrupts;
pub mod paging;
pub mod virt_layout;
pub mod gdt;

// ring3 は ring3 系 feature のときだけビルド（unused warning 対策）
#[cfg(any(feature = "ring3_demo", feature = "ring3_mailbox", feature = "ring3_mailbox_loop"))]
pub mod ring3;

use bootloader::BootInfo;

/// アーキ依存初期化処理
pub fn init(boot_info: &'static BootInfo) {
    interrupts::init();
    paging::init(boot_info);
}

/// CPU を停止させるループ
pub fn halt_loop() -> ! {
    cpu::halt_loop()
}
