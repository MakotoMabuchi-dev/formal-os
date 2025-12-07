// src/kernel/mod.rs
//
// KernelState に「抽象イベントログ (event_log)」を追加し、
// tick() や bootstrap() が画面出力とは独立した “論理イベント” を記録する。
// また、純粋な状態遷移関数 (next_activity_and_action) を設け、
// 状態遷移と副作用をきれいに分離することで、フォーマル検証向きの構造にする。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

//
// ★ LogEvent（抽象ログイベント）
//   画面出力とは独立して “何が起きたか” を記録するログ。
//   フォーマル検証時には、これが “実行軌跡（trace）” に対応する。
//
#[derive(Clone, Copy)]
pub enum LogEvent {
    TickStarted(u64),
    TimerUpdated(u64),
    FrameAllocated,
}

//
// ★ KernelActivity（カーネルが現在行っている活動状態）
//   Idle → UpdatingTimer → AllocatingFrame → Idle … というサイクルで遷移する。
//   これは OS の「状態遷移機械」の最小構成。
//
#[derive(Clone, Copy)]
pub enum KernelActivity {
    Idle,
    UpdatingTimer,
    AllocatingFrame,
}

//
// ★ KernelAction（純粋な状態遷移によって決まる “副作用” の種類）
//   副作用は tick() の中で実際に実行される。
//   状態遷移関数は副作用を行わず、「次状態」と「やるべきアクション」を返すだけ。
//
#[derive(Clone, Copy)]
enum KernelAction {
    None,
    UpdateTimer,
    AllocateFrame,
}

//
// ★ KernelState（カーネル全体の状態）
//   - 物理メモリ管理
//   - tick カウンタ
//   - 時刻カウンタ
//   - 現在の活動状態
//   - 停止フラグ
//   - 抽象イベントログ（画面出力と独立した“論理ログ”）
//
pub struct KernelState {
    phys_mem: PhysicalMemoryManager,
    tick_count: u64,
    time_ticks: u64,
    should_halt: bool,
    activity: KernelActivity,

    event_log: [Option<LogEvent>; 64],
    event_log_len: usize,
}

impl KernelState {
    /// BootInfo から KernelState を構築する
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);
        KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            activity: KernelActivity::Idle,

            event_log: [None; 64],
            event_log_len: 0,
        }
    }

    /// event_log へ 1 件 push（最大 64 件）
    fn push_event(&mut self, ev: LogEvent) {
        if self.event_log_len < self.event_log.len() {
            self.event_log[self.event_log_len] = Some(ev);
            self.event_log_len += 1;
        }
    }

    /// 起動直後に一度だけ行う処理
    pub fn bootstrap(&mut self) {
        logging::info("KernelState::bootstrap()");

        for _ in 0..5 {
            match self.phys_mem.allocate_frame() {
                Some(_) => {
                    logging::info("allocated usable frame (bootstrap)");
                    self.push_event(LogEvent::FrameAllocated);
                }
                None => {
                    logging::error("no more frames in bootstrap");
                    self.should_halt = true;
                    break;
                }
            }
        }
    }

    /// 1 tick（OS の 1 ステップ）の処理
    /// - 状態遷移（純粋関数）
    /// - 副作用の実行
    /// - 抽象ログの push
    /// を行う
    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        // 画面表示（副作用）
        logging::info("KernelState::tick()");
        logging::info_u64(" tick_count", self.tick_count);

        // ★ 抽象ログ（TickStarted）を記録
        self.push_event(LogEvent::TickStarted(self.tick_count));

        // ★ 純粋な状態遷移関数で「次の activity」と「この tick の action」を得る
        let (next_activity, action) = next_activity_and_action(self.activity);

        // ★ アクションに応じて副作用を実行し、抽象ログを記録
        match action {
            KernelAction::None => {
                logging::info(" action = None");
            }
            KernelAction::UpdateTimer => {
                logging::info(" action = UpdateTimer");

                self.time_ticks += 1;
                logging::info_u64(" time_ticks", self.time_ticks);

                self.push_event(LogEvent::TimerUpdated(self.time_ticks));
            }
            KernelAction::AllocateFrame => {
                logging::info(" action = AllocateFrame");

                match self.phys_mem.allocate_frame() {
                    Some(_) => {
                        logging::info(" allocated usable frame (tick)");
                        self.push_event(LogEvent::FrameAllocated);
                    }
                    None => {
                        logging::error(" no more usable frames; halting later");
                        self.should_halt = true;
                    }
                }
            }
        }

        // ★ 次の状態へ遷移
        self.activity = next_activity;
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    /// ★ 抽象イベントログを VGA にダンプ
    /// フォーマル検証では、このログが “実行軌跡（trace）” になる
    pub fn dump_events(&self) {
        logging::info("=== KernelState Event Log Dump ===");

        for i in 0..self.event_log_len {
            if let Some(ev) = self.event_log[i] {
                log_event_to_vga(ev);
            }
        }

        logging::info("=== End of Event Log ===");
    }
}

/// LogEvent を VGA に出力する
fn log_event_to_vga(ev: LogEvent) {
    match ev {
        LogEvent::TickStarted(n) => {
            logging::info("EVENT: TickStarted");
            logging::info_u64(" tick", n);
        }
        LogEvent::TimerUpdated(t) => {
            logging::info("EVENT: TimerUpdated");
            logging::info_u64(" time", t);
        }
        LogEvent::FrameAllocated => {
            logging::info("EVENT: FrameAllocated");
        }
    }
}

/// ★純粋な状態遷移関数（Next 関係）
/// 副作用を含まない！ ただの状態→(次状態, アクション)
fn next_activity_and_action(current: KernelActivity)
                            -> (KernelActivity, KernelAction)
{
    match current {
        KernelActivity::Idle => {
            (KernelActivity::UpdatingTimer, KernelAction::None)
        }
        KernelActivity::UpdatingTimer => {
            (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer)
        }
        KernelActivity::AllocatingFrame => {
            (KernelActivity::Idle, KernelAction::AllocateFrame)
        }
    }
}

/// カーネル開始点
pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    let mut kstate = KernelState::new(boot_info);

    kstate.bootstrap();

    let max_ticks = 30;
    for _ in 0..max_ticks {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
            break;
        }
        kstate.tick();
    }

    // ★ tick が終わったら、抽象イベントログを画像出力にダンプ
    kstate.dump_events();

    arch::halt_loop();
}
