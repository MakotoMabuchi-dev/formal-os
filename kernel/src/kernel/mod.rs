// src/kernel/mod.rs
//
// formal-os: 優先度付きプリエンプティブ＋ReadyQueue＋Blocked状態付きミニカーネル
//
// 機能概要:
// - Task: TaskId + TaskState(Ready/Running/Blocked) + runtime_ticks + time_slice_used + priority
// - ReadyQueue: Ready なタスク index のリングバッファ（順序は抽象化される）
// - WaitQueue: Blocked なタスク index のリングバッファ
// - quantum: 1タスクが連続で実行できる最大 tick 数
// - 優先度付きスケジューリング:
//   - ReadyQueue の中から「priority が最大」のタスクを選んで Running にする
// - tick():
//    1. KernelActivity の純粋状態遷移 (next_activity_and_action)
//    2. タイマ / フレーム割当 / メモリ操作（副作用）
//    3. Running タスクの runtime 更新
//    4. 疑似的 Blocked 処理（ときどき TaskState::Blocked にする）
//    5. quantum 消費と QuantumExpired イベント
//    6. 必要に応じてスケジューラ発動（優先度を考慮）
//    7. Timer tick で Blocked から 1タスクだけ Wake (WaitQueue → ReadyQueue)
// - 抽象イベントログ(LogEvent) にすべての状態遷移・スケジューリング・メモリ操作を記録し、dump_events() でダンプ。
// - メモリ管理については、MemAction(Map/Unmap) を発行し、apply_mem_action() で実装するための土台を整える。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, VirtPage};
use crate::mem::paging::{MemAction, PageFlags};

const MAX_TASKS: usize = 3;
const EVENT_LOG_CAP: usize = 256;

// デモ用に使う仮想ページ番号（ざっくり 0x100 番目のページ）
const DEMO_VIRT_PAGE_INDEX: u64 = 0x100;
// デモ用に使う物理フレーム番号（実際の物理メモリマップとはまだ連動させていない）
const DEMO_PHYS_FRAME_INDEX: u64 = 0x200;

//
// ──────────────────────────────────────────────
// TaskId / TaskState / Task
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub struct TaskId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,
}

#[derive(Clone, Copy)]
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub runtime_ticks: u64,   // タスクの累積実行時間
    pub time_slice_used: u64, // 現在の量子内で消費した tick 数
    pub priority: u8,         // ★ 優先度（大きいほど優先）
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
    WaitQueued(TaskId),
    WaitDequeued(TaskId),
    RuntimeUpdated(TaskId, u64),
    QuantumExpired(TaskId, u64),
    /// メモリ関連の抽象イベント（MemAction）が発生したことを記録する。
    MemActionApplied(MemAction),
}

//
// ──────────────────────────────────────────────
// KernelActivity（カーネル内部の状態マシン）
// ──────────────────────────────────────────────
//
// - Idle → UpdatingTimer → AllocatingFrame → MappingDemoPage → Idle → ... と 4 ステップで回る。
// - MappingDemoPage のフェーズで、デモ用の MemAction(Map) を 1 回発行する。

#[derive(Clone, Copy)]
pub enum KernelActivity {
    Idle,
    UpdatingTimer,
    AllocatingFrame,
    MappingDemoPage,
}

#[derive(Clone, Copy)]
enum KernelAction {
    None,
    UpdateTimer,
    AllocateFrame,
    /// メモリ操作フェーズを表す（どんな MemAction を発行するかは tick() 側で決める）。
    MemDemo,
}

//
// ──────────────────────────────────────────────
// KernelState（OS全体の状態）
// ──────────────────────────────────────────────
//

pub struct KernelState {
    phys_mem: PhysicalMemoryManager,

    tick_count: u64,
    time_ticks: u64,

    should_halt: bool,
    activity: KernelActivity,

    // タスク一覧
    tasks: [Task; MAX_TASKS],
    num_tasks: usize,
    current_task: usize, // 現在 Running のタスク index

    // ReadyQueue（タスク index のリングバッファ）
    ready_queue: [usize; MAX_TASKS],
    rq_head: usize,
    rq_tail: usize,
    rq_len: usize,

    // WaitQueue（Blocked なタスク index のリングバッファ）
    wait_queue: [usize; MAX_TASKS],
    wq_head: usize,
    wq_tail: usize,
    wq_len: usize,

    // 抽象イベントログ
    event_log: [Option<LogEvent>; EVENT_LOG_CAP],
    event_log_len: usize,

    // 量子（1タスクが連続で実行できる最大 tick 数）
    quantum: u64,
}

impl KernelState {
    /// カーネルの初期状態を構築する。
    /// - 3 つのタスクを用意し、それぞれに priority を設定
    /// - Task 1 を Running、Task 2,3 を Ready としてスタート
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);

        // ★ priority を付与（例: Task1=1, Task2=3, Task3=2）
        let tasks = [
            Task {
                id: TaskId(1),
                state: TaskState::Running,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 1,
            },
            Task {
                id: TaskId(2),
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 3, // 一番高い
            },
            Task {
                id: TaskId(3),
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 2,
            },
        ];

        // ReadyQueue: 最初は [2, 3] が Ready（順序は後で priority で再解釈）
        let ready_queue = [1, 2, 0];
        let rq_len = 2usize;

        KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            activity: KernelActivity::Idle,

            tasks,
            num_tasks: MAX_TASKS,
            current_task: 0, // Task 1 が最初に Running

            ready_queue,
            rq_head: 0,
            rq_tail: rq_len,
            rq_len,

            wait_queue: [0; MAX_TASKS],
            wq_head: 0,
            wq_tail: 0,
            wq_len: 0,

            event_log: [None; EVENT_LOG_CAP],
            event_log_len: 0,

            quantum: 5,
        }
    }

    fn push_event(&mut self, ev: LogEvent) {
        if self.event_log_len < self.event_log.len() {
            self.event_log[self.event_log_len] = Some(ev);
            self.event_log_len += 1;
        }
    }

    /// ブート時に物理フレームをいくつか確保してみるデモ処理。
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

    fn enqueue_ready(&mut self, idx: usize) {
        if self.rq_len >= MAX_TASKS {
            return;
        }
        self.ready_queue[self.rq_tail] = idx;
        self.rq_tail = (self.rq_tail + 1) % MAX_TASKS;
        self.rq_len += 1;

        let tid = self.tasks[idx].id;
        self.push_event(LogEvent::ReadyQueued(tid));
    }

    /// ReadyQueue から「priority が最大のタスク index」を選んで取り出す。
    fn dequeue_ready_highest_priority(&mut self) -> Option<usize> {
        if self.rq_len == 0 {
            return None;
        }

        // ReadyQueue 内から最も priority の高いタスクを探す
        let mut best_pos: usize = self.rq_head;
        let mut best_idx: usize = self.ready_queue[self.rq_head];
        let mut best_prio: u8 = self.tasks[best_idx].priority;

        for offset in 1..self.rq_len {
            let pos = (self.rq_head + offset) % MAX_TASKS;
            let idx = self.ready_queue[pos];
            let prio = self.tasks[idx].priority;

            if prio > best_prio {
                best_prio = prio;
                best_idx = idx;
                best_pos = pos;
            }
        }

        // best_pos から 1 要素取り出す。
        // 「最後の要素」を best_pos に移して穴を埋める簡易実装。
        let last_pos = (self.rq_head + self.rq_len - 1) % MAX_TASKS;
        self.ready_queue[best_pos] = self.ready_queue[last_pos];

        // tail と長さを更新
        self.rq_tail = (self.rq_head + self.rq_len - 1) % MAX_TASKS;
        self.rq_len -= 1;

        let tid = self.tasks[best_idx].id;
        self.push_event(LogEvent::ReadyDequeued(tid));

        Some(best_idx)
    }

    //
    // ──────────────────────────────────────────────
    // WaitQueue 操作（Blocked タスク）
    // ──────────────────────────────────────────────
    //

    fn enqueue_wait(&mut self, idx: usize) {
        if self.wq_len >= MAX_TASKS {
            return;
        }
        self.wait_queue[self.wq_tail] = idx;
        self.wq_tail = (self.wq_tail + 1) % MAX_TASKS;
        self.wq_len += 1;

        let tid = self.tasks[idx].id;
        self.push_event(LogEvent::WaitQueued(tid));
    }

    fn dequeue_wait(&mut self) -> Option<usize> {
        if self.wq_len == 0 {
            return None;
        }
        let idx = self.wait_queue[self.wq_head];
        self.wq_head = (self.wq_head + 1) % MAX_TASKS;
        self.wq_len -= 1;

        let tid = self.tasks[idx].id;
        self.push_event(LogEvent::WaitDequeued(tid));
        Some(idx)
    }

    //
    // ──────────────────────────────────────────────
    // スケジューラ：Running → ReadyQueue, ReadyQueue → Running（優先度付き）
    // ──────────────────────────────────────────────
    //

    fn schedule_next_task(&mut self) {
        let prev_idx = self.current_task;
        let prev_id = self.tasks[prev_idx].id;

        // Running → Ready に戻して ReadyQueue へ（Block されている場合はここまで来ない前提）
        if self.tasks[prev_idx].state == TaskState::Running {
            self.tasks[prev_idx].state = TaskState::Ready;
            self.tasks[prev_idx].time_slice_used = 0;
            self.push_event(LogEvent::TaskStateChanged(prev_id, TaskState::Ready));
            self.enqueue_ready(prev_idx);
        }

        // ReadyQueue から「priority 最大」のタスクを選ぶ
        if let Some(next_idx) = self.dequeue_ready_highest_priority() {
            let next_id = self.tasks[next_idx].id;

            self.tasks[next_idx].state = TaskState::Running;
            self.tasks[next_idx].time_slice_used = 0;
            self.current_task = next_idx;

            logging::info(" switched to task");
            logging::info_u64(" task_id", next_id.0);
            logging::info_u64(" priority", self.tasks[next_idx].priority as u64);

            self.push_event(LogEvent::TaskSwitched(next_id));
            self.push_event(LogEvent::TaskStateChanged(
                next_id,
                TaskState::Running,
            ));
        } else {
            // ReadyQueue が空なら、実行可能なタスクが無い
            logging::info(" no ready tasks; scheduler idle");
        }
    }

    //
    // ──────────────────────────────────────────────
    // Running タスクの runtime を 1 tick 増やす
    // ──────────────────────────────────────────────
    //

    fn update_runtime(&mut self) {
        let idx = self.current_task;
        let tid = self.tasks[idx].id;

        self.tasks[idx].runtime_ticks += 1;

        logging::info_u64(" runtime_ticks", self.tasks[idx].runtime_ticks);

        self.push_event(LogEvent::RuntimeUpdated(
            tid,
            self.tasks[idx].runtime_ticks,
        ));
    }

    //
    // ──────────────────────────────────────────────
    // Running タスクの time_slice_used を 1 tick 増やし、
    // quantum を超えたら QuantumExpired イベントを記録してスケジューリングする。
    // ──────────────────────────────────────────────
    //

    fn update_time_slice_and_maybe_schedule(&mut self) {
        let idx = self.current_task;
        let tid = self.tasks[idx].id;

        self.tasks[idx].time_slice_used += 1;
        let used = self.tasks[idx].time_slice_used;

        logging::info_u64(" time_slice_used", used);

        if used >= self.quantum {
            logging::info(" quantum expired; scheduling next task");
            self.push_event(LogEvent::QuantumExpired(tid, used));
            self.schedule_next_task();
        }
    }

    //
    // ──────────────────────────────────────────────
    // 疑似的な Blocked 処理:
    // ──────────────────────────────────────────────
    //

    fn maybe_block_current_task(&mut self) {
        // デモ用ルール:
        // - tick_count が 0 でなく、かつ 7 の倍数のとき
        // - かつ 現在の task_id が 2 のとき
        if self.tick_count != 0
            && self.tick_count % 7 == 0
            && self.tasks[self.current_task].id.0 == 2
        {
            let idx = self.current_task;
            let tid = self.tasks[idx].id;

            logging::info(" blocking current task (fake I/O wait)");
            self.tasks[idx].state = TaskState::Blocked;
            self.tasks[idx].time_slice_used = 0;

            self.push_event(LogEvent::TaskStateChanged(tid, TaskState::Blocked));

            // WaitQueue に入れて、次のタスクへ切り替え
            self.enqueue_wait(idx);
            self.schedule_next_task();
        }
    }

    //
    // ──────────────────────────────────────────────
    // 疑似的な Wake 処理:
    // ──────────────────────────────────────────────
    //

    fn maybe_wake_one_task(&mut self) {
        if let Some(idx) = self.dequeue_wait() {
            let tid = self.tasks[idx].id;

            logging::info(" waking 1 blocked task");
            self.tasks[idx].state = TaskState::Ready;

            self.push_event(LogEvent::TaskStateChanged(tid, TaskState::Ready));

            // ReadyQueue に戻しておく
            self.enqueue_ready(idx);
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

        // 副作用：タイマ / フレーム割り当て / メモリ操作
        match action {
            KernelAction::None => {
                logging::info(" action = None");
            }
            KernelAction::UpdateTimer => {
                logging::info(" action = UpdateTimer");
                self.time_ticks += 1;
                logging::info_u64(" time_ticks", self.time_ticks);
                self.push_event(LogEvent::TimerUpdated(self.time_ticks));

                // Timer 更新のタイミングで 1タスクだけ Wake してみる
                self.maybe_wake_one_task();
            }
            KernelAction::AllocateFrame => {
                logging::info(" action = AllocateFrame");
                if let Some(_) = self.phys_mem.allocate_frame() {
                    logging::info(" allocated usable frame (tick)");
                    self.push_event(LogEvent::FrameAllocated);
                } else {
                    logging::error(" no more usable frames; halting later");
                    self.should_halt = true;
                }
            }
            KernelAction::MemDemo => {
                logging::info(" action = MemDemo");

                // ★ ここで実際の MemAction を組み立てる。
                //   今はデモとして「固定の仮想ページ → 固定の物理フレームに READ/WRITE でマップ」という意味にする。
                let page = VirtPage::from_index(DEMO_VIRT_PAGE_INDEX);
                let frame = PhysFrame::from_index(DEMO_PHYS_FRAME_INDEX);
                let flags = PageFlags::PRESENT | PageFlags::WRITABLE;

                let mem_action = MemAction::Map { page, frame, flags };

                apply_mem_action(mem_action);
                self.push_event(LogEvent::MemActionApplied(mem_action));
            }
        }

        // Running タスクの累積 runtime を更新
        self.update_runtime();

        // 疑似的に「時々 Blocked にする」
        self.maybe_block_current_task();

        // quantum 消費と、必要ならスケジューラ発動（優先度付き）
        self.update_time_slice_and_maybe_schedule();

        // 次の KernelActivity へ
        self.activity = next_activity;
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    //
    // ──────────────────────────────────────────────
    // 抽象イベントログを VGA にダンプ
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
// LogEvent → VGA 出力（副作用）
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
                TaskState::Blocked => logging::info(" to BLOCKED"),
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
        LogEvent::WaitQueued(tid) => {
            logging::info("EVENT: WaitQueued");
            logging::info_u64(" task", tid.0);
        }
        LogEvent::WaitDequeued(tid) => {
            logging::info("EVENT: WaitDequeued");
            logging::info_u64(" task", tid.0);
        }
        LogEvent::RuntimeUpdated(tid, rt) => {
            logging::info("EVENT: RuntimeUpdated");
            logging::info_u64(" task", tid.0);
            logging::info_u64(" runtime", rt);
        }
        LogEvent::QuantumExpired(tid, used) => {
            logging::info("EVENT: QuantumExpired");
            logging::info_u64(" task", tid.0);
            logging::info_u64(" used_ticks", used);
        }
        LogEvent::MemActionApplied(action) => {
            logging::info("EVENT: MemActionApplied");
            match action {
                MemAction::Map { page, frame, flags } => {
                    logging::info(" mem_action = Map");
                    logging::info_u64(" virt_page_index", page.number);
                    logging::info_u64(" phys_frame_index", frame.number);
                    logging::info_u64(" flags_bits", flags.bits());
                }
                MemAction::Unmap { page } => {
                    logging::info(" mem_action = Unmap");
                    logging::info_u64(" virt_page_index", page.number);
                }
            }
        }
    }
}

//
// メモリ操作の「実装側」
//
// - MemAction は「やりたいことの宣言」。
// - apply_mem_action() は「実際に何をするか（副作用）」をまとめる場所。
// - 今はまだログを出すだけのダミー実装。
//   将来ここでページテーブルを書き換える (unsafe) 処理を集約する。

// kernel/src/kernel/mod.rs 内の、既存の apply_mem_action をこれに差し替え

/// MemAction を実際のページテーブル操作へ渡すラッパ。
/// - 自身は safe な関数として残し、unsafe は arch::paging 側に閉じ込める。
fn apply_mem_action(action: MemAction) {
    unsafe {
        crate::arch::paging::apply_mem_action(action);
    }
}


//
// 純粋な KernelActivity → (次状態, アクション)
//

fn next_activity_and_action(current: KernelActivity) -> (KernelActivity, KernelAction) {
    match current {
        KernelActivity::Idle =>
            (KernelActivity::UpdatingTimer, KernelAction::None),
        KernelActivity::UpdatingTimer =>
            (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer),
        KernelActivity::AllocatingFrame =>
            (KernelActivity::MappingDemoPage, KernelAction::AllocateFrame),
        KernelActivity::MappingDemoPage =>
        // デモ用に、毎サイクル 1 回だけ MemDemo を発行してみる。
            (KernelActivity::Idle, KernelAction::MemDemo),
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
