// src/kernel/mod.rs
//
// カーネル全体の状態を表す KernelState と、
// 起動時に一度だけ実行する bootstrap ロジックを定義する。
// 将来のスレッド管理・アドレス空間管理・IPC などは
// すべて KernelState のフィールドとして追加していく。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

/// カーネル全体の状態を表す構造体。
/// - 物理メモリ管理
/// - （将来）スケジューラ、プロセステーブル、IPC など
///
/// フォーマル検証の観点からは、
/// 「ある時点の OS の状態」を 1 つの KernelState 値として扱えるようにすることが目的。
pub struct KernelState {
    phys_mem: PhysicalMemoryManager,
    // TODO: 将来ここに scheduler や process_table などを追加していく
}

impl KernelState {
    /// 起動直後に BootInfo から KernelState を構築する。
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);
        KernelState { phys_mem }
    }

    /// 起動時に一度だけ行う初期処理。
    ///
    /// 現時点では「物理メモリから usable なフレームをいくつか確保してみる」
    /// デモだけを行う。
    pub fn bootstrap(&mut self) {
        logging::info("KernelState::bootstrap()");

        // デモとして最初の 5 フレームを確保してログ出力する。
        for _ in 0..5 {
            match self.phys_mem.allocate_frame() {
                Some(frame) => {
                    let addr = frame.start_address().as_u64();
                    logging::info("allocated usable frame");
                    // TODO: 将来ここで addr を 16 進数で表示できるようにする
                    let _ = addr; // 現時点では「確保できた」という事実だけ使う
                }
                None => {
                    logging::error("no more usable frames");
                    break;
                }
            }
        }
    }
}

/// カーネル起動時のエントリポイント。
/// - main.rs から呼び出されるのはこの関数だけ。
/// - ここで KernelState を生成し、bootstrap を 1 回実行してから停止する。
pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    // カーネル全体の状態を構築
    let mut kstate = KernelState::new(boot_info);

    // 起動時に 1 回だけ行う初期処理
    kstate.bootstrap();

    // 現時点ではこれ以上することがないので停止
    arch::halt_loop();
}
