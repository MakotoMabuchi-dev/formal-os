// src/kernel/mod.rs
//
// 【新要素】TaskId と簡易スケジューラを導入。
// KernelState に「擬似タスク」を追加し、tick ごとに Running タスクを切り替える。
// これにより、フォーマル検証しやすい “状態機械としての OS” がさらに明確になる。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

//
// ★ TaskId: 擬似タスク
//
#[derive(Clone, Copy)]
pub struct TaskId(pub u64);

//
// ★ LogEvent（抽象ログイベント）
//
#[derive(Clone, Copy)]
pub enum LogEvent {
    TickStarted(u64),
    TimerUpdated(u64),
    FrameAllocated,
    TaskSwitched(TaskId),   // ★ 新イベント
}

//
// ★ KernelActivity（カーネルが現在行っている活動状態）
//
#[derive(Clone, Copy)]
pub enum KernelActivity {
    Idle,
    UpdatingTimer,
    AllocatingFrame,
}

//
// ★ KernelAction（この tick で行うべき副作用）
#[derive(Clone, Copy)]
enum KernelAction {
    None,
    UpdateTimer,
    AllocateFrame,
}

//
// ★ KernelState（カーネル全体の状態）
//   今回:
//     - tasks: 擬似タスク一覧
//     - current_task: 今動作中のタスクインデックス
//     - TaskSwitched イベントの追加
//
pub struct KernelState {
    phys_mem: PhysicalMemoryManager,

    tick_count: u64,
    time_ticks: u64,

    should_halt: bool,
    activity: KernelActivity,

    // ★ 擬似タスク
    tasks: [TaskId; 3],
    num_tasks: usize,
    current_task: usize,

    // ★ 抽象イベントログ
    event_log: [Option<LogEvent>; 128],
    event_log_len: usize,
}

impl KernelState {
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);

        KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            activity: KernelActivity::Idle,

            // ★ 擬似タスク初期化
            tasks: [TaskId(1), TaskId(2), TaskId(3)],
            num_tasks: 3,
            current_task: 0,

            event_log: [None; 128],
            event_log_len: 0,
        }
    }

    fn push_event(&mut self, ev: LogEvent) {
        if self.event_log_len < self.event_log.len() {
            self.event_log[self.event_log_len] = Some(ev);
            self.event_log_len += 1;
        }
    }

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

    /// ★ 最小スケジューラ：
    /// current_task を 1 歩進める（ラウンドロビン）
    fn schedule_next_task(&mut self) {
        self.current_task = (self.current_task + 1) % self.num_tasks;
        let next_id = self.tasks[self.current_task];

        logging::info(" switched to task");
        logging::info_u64(" task_id", next_id.0);

        // ★ 抽象ログにも記録
        self.push_event(LogEvent::TaskSwitched(next_id));
    }

    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        // ===== Tick メタログ =====
        logging::info("KernelState::tick()");
        logging::info_u64(" tick_count", self.tick_count);
        self.push_event(LogEvent::TickStarted(self.tick_count));

        logging::info_u64(" running_task", self.tasks[self.current_task].0);

        // ===== 状態遷移 =====
        let (next_activity, action) = next_activity_and_action(self.activity);

        // ===== 副作用 =====
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

        // ===== ★ スケジューラの呼び出し =====
        self.schedule_next_task();

        // ===== 次の状態へ移行 =====
        self.activity = next_activity;
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    /// ★ 抽象ログを VGA 出力にダンプ
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

//// LogEvent を VGA に出力（副作用）
//
fn log_event_to_vga(ev: LogEvent) {
    match ev {
        LogEvent::TickStarted(n) => {
            logging::info("EVENT: TickStarted");
            logging::info_u64(" tick", n);
        }
        LogEvent::TimerUpdated(n) => {
            logging::info("EVENT: TimerUpdated");
            logging::info_u64(" time", n);
        }
        LogEvent::FrameAllocated => {
            logging::info("EVENT: FrameAllocated");
        }
        LogEvent::TaskSwitched(tid) => {
            logging::info("EVENT: TaskSwitched");
            logging::info_u64(" task", tid.0);
        }
    }
}

/// ★純粋な状態遷移関数
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

    // 終了前に抽象ログを表示
    kstate.dump_events();

    arch::halt_loop();
}
