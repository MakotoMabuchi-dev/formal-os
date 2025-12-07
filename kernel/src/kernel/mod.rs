// src/kernel/mod.rs
//
// KernelState に「活動状態（KernelActivity）」と「簡易タイマ(time_ticks)」を持たせ、
// tick() が Idle → UpdatingTimer → AllocatingFrame → Idle ... という
// 3 状態のサイクルで遷移する小さな状態機械を構成する。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

/// カーネルが現在行っている“活動”。
///
/// - Idle: 何もすることがない待機状態
/// - UpdatingTimer: 時刻やタイマを更新する状態（擬似）
/// - AllocatingFrame: 物理フレームを 1 つ確保する状態
#[derive(Clone, Copy)]
pub enum KernelActivity {
    Idle,
    UpdatingTimer,
    AllocatingFrame,
}

/// カーネル全体の状態。
pub struct KernelState {
    phys_mem: PhysicalMemoryManager,
    tick_count: u64,
    time_ticks: u64,    // 簡易タイマ: UpdatingTimer 状態のたびに増える
    should_halt: bool,
    activity: KernelActivity,
}

impl KernelState {
    /// 起動直後に BootInfo から KernelState を構築する。
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);
        KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            // 最初は Idle からスタートし、1 回目の tick で UpdatingTimer へ進む
            activity: KernelActivity::Idle,
        }
    }

    /// 起動時に一度だけ行う初期処理。
    pub fn bootstrap(&mut self) {
        logging::info("KernelState::bootstrap()");

        for _ in 0..5 {
            match self.phys_mem.allocate_frame() {
                Some(_) => logging::info("allocated usable frame (bootstrap)"),
                None => {
                    logging::error("no more frames in bootstrap");
                    self.should_halt = true;
                    break;
                }
            }
        }
    }

    /// OS が tick ごとに状態遷移を行う。
    ///
    /// 状態遷移の仕様（Next 関係）:
    ///
    /// - (Idle,     ok) → (UpdatingTimer, ok)
    /// - (UpdatingTimer, ok) → (AllocatingFrame, ok)
    /// - (AllocatingFrame, ok & frame 取得成功) → (Idle, ok)
    /// - (AllocatingFrame, ok & frame 取得失敗) → (Idle, should_halt = true)
    /// - (任意, should_halt = true) → 変化なし（tick は何もしない）
    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        logging::info("KernelState::tick()");
        logging::info_u64(" tick_count", self.tick_count);

        match self.activity {
            KernelActivity::Idle => {
                logging::info(" activity = Idle (nothing to do)");
                // 次のステップではタイマ更新へ
                self.activity = KernelActivity::UpdatingTimer;
            }

            KernelActivity::UpdatingTimer => {
                logging::info(" activity = UpdatingTimer (update time)");

                // 簡易タイマ: time_ticks をインクリメントしてログに出す
                self.time_ticks += 1;
                logging::info_u64(" time_ticks", self.time_ticks);

                // 次のステップではフレーム割り当てへ
                self.activity = KernelActivity::AllocatingFrame;
            }

            KernelActivity::AllocatingFrame => {
                logging::info(" activity = AllocatingFrame (allocating)");

                match self.phys_mem.allocate_frame() {
                    Some(_) => {
                        logging::info(" allocated usable frame (tick)");
                        // フレーム取得に成功したら Idle に戻る
                        self.activity = KernelActivity::Idle;
                    }
                    None => {
                        logging::error(" no more usable frames; halting later");
                        // これ以上フレームを取れないなら、以降の tick は何もしない
                        self.should_halt = true;
                        // activity はとりあえず Idle にしておく
                        self.activity = KernelActivity::Idle;
                    }
                }
            }
        }
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }
}

/// カーネル起動時のエントリポイント。
pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    let mut kstate = KernelState::new(boot_info);

    kstate.bootstrap();

    // デモ用: 最大 30 tick まで進める。
    let max_ticks = 30;
    for _ in 0..max_ticks {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
            break;
        }
        kstate.tick();
    }

    arch::halt_loop();
}
