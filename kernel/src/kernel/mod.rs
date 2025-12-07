// src/kernel/mod.rs
//
// フォーマル検証の主対象になりうる「OS 本体ロジック」の入口。
// - ここから先にスレッド管理・アドレス空間・IPC などを段階的に追加していく。
// - 現時点では、PhysicalMemoryManager を通じて物理フレームが列挙できるかを確認する。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    // ★ 物理メモリマネージャを構築
    let mut phys_mem = PhysicalMemoryManager::new(boot_info);

    // ★ 最初の 5 フレームを試しに確保してみる
    for _ in 0..5 {
        match phys_mem.allocate_frame() {
            Some(frame) => {
                let addr = frame.start_address().as_u64();
                // ここでは「フレームが取れた」という事実だけログに出す。
                // （数値フォーマットは後のステップで拡張してもよい）
                logging::info("allocated usable frame");
                let _ = addr; // 今は未使用だが、将来のために保持
            }
            None => {
                logging::error("no more usable frames");
                break;
            }
        }
    }

    // 現時点ではこれ以上することがないので停止
    arch::halt_loop();
}
