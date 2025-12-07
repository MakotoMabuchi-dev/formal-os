// src/kernel/mod.rs
//
// Task の状態（Ready / Running）を導入し、
// tickごとに Running → Ready → Running … と切り替える簡易スケジューラを構築。
// LogEvent に TaskStateChanged を追加し、OS の内部状態遷移を可視化する。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

//
// ──────────────────────────────────────────────
// Task, TaskState, TaskId
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub struct TaskId(pub u64);

#[derive(Clone, Copy)]
pub enum TaskState {
    Ready,
    Running,
}

#[derive(Clone, Copy)]
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
}

//
// ──────────────────────────────────────────────
// LogEvent（抽象イベント）
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub enum LogEvent {
    TickStarted(u64),
    TimerUpdated(u64),
    FrameAllocated,
    TaskSwitched(TaskId),
    TaskStateChanged(TaskId, TaskState),   // ★ 新イベント
}

//
// ──────────────────────────────────────────────
// KernelActivity（カーネルの状態マシン）
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub enum KernelActivity {
    Idle,
    UpdatingTimer,
    AllocatingFrame,
}

#[derive(Clone, Copy)]
enum KernelAction {
    None,
    UpdateTimer,
    AllocateFrame,
}

//
// ──────────────────────────────────────────────
// KernelState（カーネル全体の状態）
// ──────────────────────────────────────────────
//

pub struct KernelState {
    phys_mem: PhysicalMemoryManager,

    tick_count: u64,
    time_ticks: u64,

    should_halt: bool,
    activity: KernelActivity,

    // ★ Task array
    tasks: [Task; 3],
    num_tasks: usize,
    current_task: usize,

    // ★ 抽象イベントログ
    event_log: [Option<LogEvent>; 256],
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

            // ★ Task 初期化：最初のタスクだけ Running
            tasks: [
                Task { id: TaskId(1), state: TaskState::Running },
                Task { id: TaskId(2), state: TaskState::Ready },
                Task { id: TaskId(3), state: TaskState::Ready },
            ],
            num_tasks: 3,
            current_task: 0, // Task 1 が最初に実行中

            event_log: [None; 256],
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

    //
    // ──────────────────────────────────────────────
    // ★ 簡易スケジューラ：状態遷移ベースで Running タスクを切り替える
    // ──────────────────────────────────────────────
    //
    fn schedule_next_task(&mut self) {
        let prev_task = self.current_task;

        // Running → Ready
        self.tasks[prev_task].state = TaskState::Ready;
        self.push_event(LogEvent::TaskStateChanged(
            self.tasks[prev_task].id,
            TaskState::Ready,
        ));

        // 次のタスクへ移動（ラウンドロビン）
        let next = (self.current_task + 1) % self.num_tasks;

        // Ready → Running
        self.tasks[next].state = TaskState::Running;
        self.current_task = next;

        logging::info(" switched to task");
        logging::info_u64(" task_id", self.tasks[next].id.0);

        self.push_event(LogEvent::TaskSwitched(self.tasks[next].id));
        self.push_event(LogEvent::TaskStateChanged(
            self.tasks[next].id,
            TaskState::Running,
        ));
    }

    //
    // ──────────────────────────────────────────────
    // tick（OS の 1 ステップ）
    // ──────────────────────────────────────────────
    //
    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        logging::info("KernelState::tick()");
        logging::info_u64(" tick_count", self.tick_count);

        self.push_event(LogEvent::TickStarted(self.tick_count));

        logging::info_u64(" running_task", self.tasks[self.current_task].id.0);

        let (next_activity, action) = next_activity_and_action(self.activity);

        // ★ 副作用
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

        // ★ タスク切り替え
        self.schedule_next_task();

        self.activity = next_activity;
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

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

//
// LogEvent を VGA に出力（副作用）
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
        LogEvent::TaskStateChanged(tid, state) => {
            logging::info("EVENT: TaskStateChanged");
            logging::info_u64(" task", tid.0);
            match state {
                TaskState::Ready => logging::info(" to READY"),
                TaskState::Running => logging::info(" to RUNNING"),
            }
        }
    }
}

//
// 純粋な状態遷移関数（Next）
//
fn next_activity_and_action(current: KernelActivity)
                            -> (KernelActivity, KernelAction)
{
    match current {
        KernelActivity::Idle =>
            (KernelActivity::UpdatingTimer, KernelAction::None),

        KernelActivity::UpdatingTimer =>
            (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer),

        KernelActivity::AllocatingFrame =>
            (KernelActivity::Idle, KernelAction::AllocateFrame),
    }
}

//
// カーネル開始点
//
pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    let mut kstate = KernelState::new(boot_info);

    kstate.bootstrap();

    let max_ticks = 40;
    for _ in 0..max_ticks {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
            break;
        }
        kstate.tick();
    }

    kstate.dump_events();

    arch::halt_loop();
}
