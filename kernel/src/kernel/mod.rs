// src/kernel/mod.rs
//
// KernelState に「活動状態（KernelActivity）」と「簡易タイマ (time_ticks)」を持たせ、
// tick() 内で「純粋な状態遷移 (KernelActivity -> KernelActivity, KernelAction)」と
// 「副作用 (タイマ更新・フレーム確保)」を分離する。
// これにより、状態遷移部分だけをフォーマル検証の対象としやすくなる。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

/// カーネルが現在行っている“活動”
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

/// 状態遷移が指示する「この tick で行うべきアクション」
///
/// - None: 何もしない
/// - UpdateTimer: 簡易タイマを 1 ステップ進める
/// - AllocateFrame: 物理フレームを 1 つ確保する
#[derive(Clone, Copy)]
enum KernelAction {
    None,
    UpdateTimer,
    AllocateFrame,
}

/// カーネル全体の状態。
pub struct KernelState {
    phys_mem: PhysicalMemoryManager,
    tick_count: u64,
    time_ticks: u64,    // 簡易タイマ: UpdateTimer アクションのたびに増える
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
            // 最初は Idle からスタート
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
    /// 状態遷移仕様（Next 関係）は `next_activity_and_action` に集約する：
    ///
    ///   Idle           → (UpdatingTimer,  KernelAction::None)
    ///   UpdatingTimer  → (AllocatingFrame, KernelAction::UpdateTimer)
    ///   AllocatingFrame→ (Idle,          KernelAction::AllocateFrame)
    ///
    /// tick() 自体は、
    /// - 現在の activity をログ出力
    /// - 上記の純粋関数から「次の activity と action」を取得
    /// - action に応じた副作用を実行
    /// - activity を更新
    ///
    /// という流れになっている。
    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        logging::info("KernelState::tick()");
        logging::info_u64(" tick_count", self.tick_count);

        // 現在の活動状態をログに出す（観測用）
        match self.activity {
            KernelActivity::Idle => {
                logging::info(" activity(now) = Idle");
            }
            KernelActivity::UpdatingTimer => {
                logging::info(" activity(now) = UpdatingTimer");
            }
            KernelActivity::AllocatingFrame => {
                logging::info(" activity(now) = AllocatingFrame");
            }
        }

        // ★ 純粋な状態遷移関数を呼び出して、「次の活動状態」と「今回のアクション」を得る
        let (next_activity, action) = next_activity_and_action(self.activity);

        // ★ アクションに応じて副作用を実行
        match action {
            KernelAction::None => {
                logging::info(" action = None");
            }
            KernelAction::UpdateTimer => {
                logging::info(" action = UpdateTimer");
                self.time_ticks += 1;
                logging::info_u64(" time_ticks", self.time_ticks);
            }
            KernelAction::AllocateFrame => {
                logging::info(" action = AllocateFrame");
                match self.phys_mem.allocate_frame() {
                    Some(_) => {
                        logging::info(" allocated usable frame (tick)");
                    }
                    None => {
                        logging::error(" no more usable frames; halting later");
                        self.should_halt = true;
                    }
                }
            }
        }

        // ★ 最後に、次の活動状態へ遷移
        self.activity = next_activity;
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }
}

/// 純粋な状態遷移関数。
///
/// - 入力: 現在の KernelActivity
/// - 出力: (次の KernelActivity, この tick で実行すべき KernelAction)
///
/// ここには副作用（メモリ確保やログ出力など）は一切含めない。
fn next_activity_and_action(current: KernelActivity) -> (KernelActivity, KernelAction) {
    match current {
        KernelActivity::Idle => {
            // Idle の次は UpdatingTimer だが、この tick では何もしない
            (KernelActivity::UpdatingTimer, KernelAction::None)
        }
        KernelActivity::UpdatingTimer => {
            // UpdatingTimer の tick ではタイマ更新を行い、次は AllocatingFrame へ
            (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer)
        }
        KernelActivity::AllocatingFrame => {
            // AllocatingFrame の tick ではフレーム割り当てを行い、次は Idle へ
            (KernelActivity::Idle, KernelAction::AllocateFrame)
        }
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
