// src/kernel/mod.rs
//
// Task の状態（Ready / Running）と ReadyQueue を導入した簡易スケジューラ版。
// - TaskId / Task / TaskState で擬似タスクを表現。
// - KernelState に ReadyQueue を持たせ、tick ごとに Running → ReadyQueue → Running を切り替える。
// - LogEvent に TaskSwitched / TaskStateChanged / ReadyQueued / ReadyDequeued を記録。
// - 状態遷移（KernelActivity）は純粋関数 next_activity_and_action で管理。
// - 副作用（タイマ更新・フレーム確保・タスク切り替え）は tick() 内で実行。
// - event_log に抽象イベントを貯め、dump_events() で最後にまとめて表示する。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

const MAX_TASKS: usize = 3;
const EVENT_LOG_CAP: usize = 256;

//
// ──────────────────────────────────────────────
// TaskId / TaskState / Task
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
// LogEvent（抽象イベントログ）
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub enum LogEvent {
    TickStarted(u64),
    TimerUpdated(u64),
    FrameAllocated,
    TaskSwitched(TaskId),
    TaskStateChanged(TaskId, TaskState),
    ReadyQueued(TaskId),
    ReadyDequeued(TaskId),
}

//
// ──────────────────────────────────────────────
// KernelActivity（カーネルの活動状態）
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

    // ★ タスク一覧
    tasks: [Task; MAX_TASKS],
    num_tasks: usize,
    current_task: usize, // 現在 Running のタスク index

    // ★ ReadyQueue（タスク index のリングバッファ）
    ready_queue: [usize; MAX_TASKS],
    rq_head: usize,
    rq_tail: usize,
    rq_len: usize,

    // ★ 抽象イベントログ
    event_log: [Option<LogEvent>; EVENT_LOG_CAP],
    event_log_len: usize,
}

impl KernelState {
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);

        // 最初のタスクだけ Running、残りは Ready とする
        let tasks = [
            Task { id: TaskId(1), state: TaskState::Running },
            Task { id: TaskId(2), state: TaskState::Ready },
            Task { id: TaskId(3), state: TaskState::Ready },
        ];

        // ReadyQueue には「現在 Running 以外のタスク」を入れておく
        let ready_queue = [1, 2, 0]; // 実際に使うのは rq_len 分だけ
        let rq_len = 2usize;

        KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            activity: KernelActivity::Idle,

            tasks,
            num_tasks: MAX_TASKS,
            current_task: 0, // Task 1 が最初の Running

            ready_queue,
            rq_head: 0,
            rq_tail: rq_len,
            rq_len,

            event_log: [None; EVENT_LOG_CAP],
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
    // ReadyQueue 操作
    // ──────────────────────────────────────────────
    //

    /// ReadyQueue にタスク index を enqueue
    fn enqueue_ready(&mut self, idx: usize) {
        if self.rq_len >= MAX_TASKS {
            return; // これ以上入らない（今回は無視）
        }
        self.ready_queue[self.rq_tail] = idx;
        self.rq_tail = (self.rq_tail + 1) % MAX_TASKS;
        self.rq_len += 1;

        let tid = self.tasks[idx].id;
        self.push_event(LogEvent::ReadyQueued(tid));
    }

    /// ReadyQueue からタスク index を dequeue
    fn dequeue_ready(&mut self) -> Option<usize> {
        if self.rq_len == 0 {
            return None;
        }
        let idx = self.ready_queue[self.rq_head];
        self.rq_head = (self.rq_head + 1) % MAX_TASKS;
        self.rq_len -= 1;

        let tid = self.tasks[idx].id;
        self.push_event(LogEvent::ReadyDequeued(tid));
        Some(idx)
    }

    //
    // ──────────────────────────────────────────────
    // 簡易スケジューラ：Running ↔ ReadyQueue
    // ──────────────────────────────────────────────
    //

    fn schedule_next_task(&mut self) {
        let prev_idx = self.current_task;
        let prev_id = self.tasks[prev_idx].id;

        // 現在 Running のタスクを Ready に戻し、ReadyQueue へ
        self.tasks[prev_idx].state = TaskState::Ready;
        self.push_event(LogEvent::TaskStateChanged(prev_id, TaskState::Ready));
        self.enqueue_ready(prev_idx);

        // ReadyQueue から次のタスクを取り出して Running にする
        if let Some(next_idx) = self.dequeue_ready() {
            let next_id = self.tasks[next_idx].id;

            self.tasks[next_idx].state = TaskState::Running;
            self.current_task = next_idx;

            logging::info(" switched to task");
            logging::info_u64(" task_id", next_id.0);

            self.push_event(LogEvent::TaskSwitched(next_id));
            self.push_event(LogEvent::TaskStateChanged(next_id, TaskState::Running));
        } else {
            // ReadyQueue が空の場合は、前のタスクを Running に戻す（フォールバック）
            self.tasks[prev_idx].state = TaskState::Running;
            self.push_event(LogEvent::TaskStateChanged(prev_id, TaskState::Running));
        }
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

        let running = self.tasks[self.current_task].id;
        logging::info_u64(" running_task", running.0);

        let (next_activity, action) = next_activity_and_action(self.activity);

        // 副作用
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

        // タスクスケジューラ発動
        self.schedule_next_task();

        // 次の活動状態へ
        self.activity = next_activity;
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    //
    // ──────────────────────────────────────────────
    // 抽象イベントログのダンプ
    // ──────────────────────────────────────────────
    //

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
// LogEvent → VGA 出力
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
        LogEvent::ReadyQueued(tid) => {
            logging::info("EVENT: ReadyQueued");
            logging::info_u64(" task", tid.0);
        }
        LogEvent::ReadyDequeued(tid) => {
            logging::info("EVENT: ReadyDequeued");
            logging::info_u64(" task", tid.0);
        }
    }
}

//
// 純粋な状態遷移関数（KernelActivity → (次状態, アクション))
//

fn next_activity_and_action(current: KernelActivity) -> (KernelActivity, KernelAction) {
    match current {
        KernelActivity::Idle => (KernelActivity::UpdatingTimer, KernelAction::None),
        KernelActivity::UpdatingTimer => (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer),
        KernelActivity::AllocatingFrame => (KernelActivity::Idle, KernelAction::AllocateFrame),
    }
}

//
// カーネル起動エントリ
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
