// src/kernel/mod.rs
//
// formal-os: 優先度付きプリエンプティブ＋ReadyQueue＋Blocked状態付きミニカーネル
//
// - Task: TaskId + TaskState + AddressSpaceId
// - AddressSpace: root_page_frame + logical mappings
// - Task0 のみ実ページテーブルへの反映（map_to/unmap）
// - switch_address_space により「アドレス空間切替」の抽象イベントを発行する
//

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};
use crate::mem::paging::{MemAction, PageFlags};
use crate::mem::address_space::{AddressSpace, AddressSpaceError};
use x86_64::registers::control::Cr3;

const MAX_TASKS: usize = 3;
const EVENT_LOG_CAP: usize = 256;

// デモ用: 0x0010_0000 (1MiB) → 仮想ページ index
const DEMO_VIRT_PAGE_INDEX: u64 = 0x100;
// デモ用: 0x0020_0000 (2MiB) → 物理フレーム index
const DEMO_PHYS_FRAME_INDEX: u64 = 0x200;

//
// ──────────────────────────────────────────────
// TaskId / TaskState / Task / AddressSpaceId
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub struct TaskId(pub u64);

#[derive(Clone, Copy)]
pub struct AddressSpaceId(pub usize);

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
    pub runtime_ticks: u64,
    pub time_slice_used: u64,
    pub priority: u8,

    /// このタスクが属する論理アドレス空間のID。
    pub address_space_id: AddressSpaceId,
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

    /// MemActionApplied:
    /// - task:        この MemAction を起こしたタスクID
    /// - address_space: 対象となる AddressSpaceId
    /// - action:      Map(page, frame, flags) または Unmap(page)
    /// - このイベントは「抽象メモリモデルに対する操作」を表し、
    ///   実際のページテーブル更新は arch::paging 側で処理される。
    MemActionApplied {
        task: TaskId,
        address_space: AddressSpaceId,
        action: MemAction,
    },
}

//
// ──────────────────────────────────────────────
// KernelActivity / KernelAction
// ──────────────────────────────────────────────
//

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

    // Taskごとの論理アドレス空間
    address_spaces: [AddressSpace; MAX_TASKS],

    // タスク一覧
    tasks: [Task; MAX_TASKS],
    num_tasks: usize,
    current_task: usize,

    // ReadyQueue（タスク index のリングバッファ）
    ready_queue: [usize; MAX_TASKS],
    rq_head: usize,
    rq_tail: usize,
    rq_len: usize,

    // WaitQueue（Blocked タスク index のリングバッファ）
    wait_queue: [usize; MAX_TASKS],
    wq_head: usize,
    wq_tail: usize,
    wq_len: usize,

    // 抽象イベントログ
    event_log: [Option<LogEvent>; EVENT_LOG_CAP],
    event_log_len: usize,

    // 量子
    quantum: u64,

    // MemDemo (Map/Unmap 切替)
    mem_demo_mapped: [bool; MAX_TASKS],
}

impl KernelState {
    //
    // new()
    //
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);

        //
        // Task0 用 root_page_frame を CR3 から取得
        //
        let root_frame_for_task0: PhysFrame = {
            let (level_4_frame, _) = Cr3::read();
            let phys_u64 = level_4_frame.start_address().as_u64();
            let frame_index = phys_u64 / PAGE_SIZE;
            PhysFrame::from_index(frame_index)
        };

        //
        // Task 配列
        //
        let tasks = [
            Task {
                id: TaskId(1),
                state: TaskState::Running,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 1,
                address_space_id: AddressSpaceId(0),
            },
            Task {
                id: TaskId(2),
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 3,
                address_space_id: AddressSpaceId(1),
            },
            Task {
                id: TaskId(3),
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 2,
                address_space_id: AddressSpaceId(2),
            },
        ];

        //
        // KernelState を作る
        //
        let mut state = KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            activity: KernelActivity::Idle,

            address_spaces: [AddressSpace::new(); MAX_TASKS],

            tasks,
            num_tasks: MAX_TASKS,
            current_task: 0,

            ready_queue: [1, 2, 0],
            rq_head: 0,
            rq_tail: 2,
            rq_len: 2,

            wait_queue: [0; MAX_TASKS],
            wq_head: 0,
            wq_tail: 0,
            wq_len: 0,

            event_log: [None; EVENT_LOG_CAP],
            event_log_len: 0,

            quantum: 5,
            mem_demo_mapped: [false; MAX_TASKS],
        };

        // ★ Task0 の AddressSpace(0) に root_page_frame を設定
        state.address_spaces[0].root_page_frame = Some(root_frame_for_task0);

        state
    }

    fn push_event(&mut self, ev: LogEvent) {
        if self.event_log_len < EVENT_LOG_CAP {
            self.event_log[self.event_log_len] = Some(ev);
            self.event_log_len += 1;
        }
    }

    //
    // bootstrap
    //
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
    // ReadyQueue
    //
    fn enqueue_ready(&mut self, idx: usize) {
        if self.rq_len >= MAX_TASKS {
            return;
        }
        self.ready_queue[self.rq_tail] = idx;
        self.rq_tail = (self.rq_tail + 1) % MAX_TASKS;
        self.rq_len += 1;

        self.push_event(LogEvent::ReadyQueued(self.tasks[idx].id));
    }

    fn dequeue_ready_highest_priority(&mut self) -> Option<usize> {
        if self.rq_len == 0 {
            return None;
        }

        let mut best_pos = self.rq_head;
        let mut best_idx = self.ready_queue[self.rq_head];
        let mut best_prio = self.tasks[best_idx].priority;

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

        // 取り出し
        let last_pos = (self.rq_head + self.rq_len - 1) % MAX_TASKS;
        self.ready_queue[best_pos] = self.ready_queue[last_pos];

        self.rq_tail = last_pos;
        self.rq_len -= 1;

        self.push_event(LogEvent::ReadyDequeued(self.tasks[best_idx].id));
        Some(best_idx)
    }

    //
    // WaitQueue
    //
    fn enqueue_wait(&mut self, idx: usize) {
        if self.wq_len >= MAX_TASKS {
            return;
        }
        self.wait_queue[self.wq_tail] = idx;
        self.wq_tail = (self.wq_tail + 1) % MAX_TASKS;
        self.wq_len += 1;

        self.push_event(LogEvent::WaitQueued(self.tasks[idx].id));
    }

    fn dequeue_wait(&mut self) -> Option<usize> {
        if self.wq_len == 0 {
            return None;
        }
        let idx = self.wait_queue[self.wq_head];
        self.wq_head = (self.wq_head + 1) % MAX_TASKS;
        self.wq_len -= 1;

        self.push_event(LogEvent::WaitDequeued(self.tasks[idx].id));
        Some(idx)
    }

    //
    // スケジューラ
    //
    fn schedule_next_task(&mut self) {
        let prev_idx = self.current_task;
        let prev_id = self.tasks[prev_idx].id;

        if self.tasks[prev_idx].state == TaskState::Running {
            self.tasks[prev_idx].state = TaskState::Ready;
            self.tasks[prev_idx].time_slice_used = 0;
            self.push_event(LogEvent::TaskStateChanged(prev_id, TaskState::Ready));
            self.enqueue_ready(prev_idx);
        }

        if let Some(next_idx) = self.dequeue_ready_highest_priority() {
            let next_id = self.tasks[next_idx].id;
            let as_idx = self.tasks[next_idx].address_space_id.0;

            self.tasks[next_idx].state = TaskState::Running;
            self.tasks[next_idx].time_slice_used = 0;
            self.current_task = next_idx;

            logging::info(" switched to task");
            logging::info_u64(" task_id", next_id.0);

            // ★ AddressSpace の root_page_frame を参照して切替
            let root = self.address_spaces[as_idx].root_page_frame;
            arch::paging::switch_address_space(root);

            self.push_event(LogEvent::TaskSwitched(next_id));
            self.push_event(LogEvent::TaskStateChanged(next_id, TaskState::Running));
        } else {
            logging::info(" no ready tasks; scheduler idle");
        }
    }

    //
    // runtime 更新
    //
    fn update_runtime(&mut self) {
        let idx = self.current_task;
        let id = self.tasks[idx].id;

        self.tasks[idx].runtime_ticks += 1;
        logging::info_u64(" runtime_ticks", self.tasks[idx].runtime_ticks);

        self.push_event(LogEvent::RuntimeUpdated(
            id,
            self.tasks[idx].runtime_ticks,
        ));
    }

    //
    // time slice 更新
    //
    fn update_time_slice_and_maybe_schedule(&mut self) {
        let idx = self.current_task;
        let id = self.tasks[idx].id;

        self.tasks[idx].time_slice_used += 1;
        logging::info_u64(" time_slice_used", self.tasks[idx].time_slice_used);

        if self.tasks[idx].time_slice_used >= self.quantum {
            logging::info(" quantum expired; scheduling next task");
            self.push_event(LogEvent::QuantumExpired(id, self.tasks[idx].time_slice_used));
            self.schedule_next_task();
        }
    }

    //
    // 疑似 Blocked
    //
    fn maybe_block_current_task(&mut self) {
        if self.tick_count != 0
            && self.tick_count % 7 == 0
            && self.tasks[self.current_task].id.0 == 2
        {
            let idx = self.current_task;
            let id = self.tasks[idx].id;

            logging::info(" blocking current task (fake I/O wait)");

            self.tasks[idx].state = TaskState::Blocked;
            self.tasks[idx].time_slice_used = 0;

            self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));

            self.enqueue_wait(idx);
            self.schedule_next_task();
        }
    }

    //
    // 疑似 Wake
    //
    fn maybe_wake_one_task(&mut self) {
        if let Some(idx) = self.dequeue_wait() {
            let id = self.tasks[idx].id;
            logging::info(" waking 1 blocked task");

            self.tasks[idx].state = TaskState::Ready;

            self.push_event(LogEvent::TaskStateChanged(id, TaskState::Ready));
            self.enqueue_ready(idx);
        }
    }

    //
    // 実ページテーブル操作
    //
    fn apply_mem_action(&mut self, action: MemAction) {
        unsafe {
            arch::paging::apply_mem_action(action, &mut self.phys_mem);
        }
    }

    /// tick():
    /// - 現在の KernelActivity に応じて、(次の KernelActivity, KernelAction) を決める
    /// - Timer 更新 / Frame 割当 / MemDemo のいずれかを1ステップだけ行う
    /// - 抽象的には、次のような状態遷移:
    ///     Idle            -> UpdatingTimer
    ///     UpdatingTimer   -> AllocatingFrame
    ///     AllocatingFrame -> MappingDemoPage
    ///     MappingDemoPage -> Idle
    /// - すべての主要イベントは LogEvent に記録される。
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

        match action {
            KernelAction::None => {
                logging::info(" action = None");
            }
            KernelAction::UpdateTimer => {
                logging::info(" action = UpdateTimer");
                self.time_ticks += 1;
                logging::info_u64(" time_ticks", self.time_ticks);
                self.push_event(LogEvent::TimerUpdated(self.time_ticks));

                self.maybe_wake_one_task();
            }
            KernelAction::AllocateFrame => {
                logging::info(" action = AllocateFrame");
                if let Some(_) = self.phys_mem.allocate_frame() {
                    logging::info(" allocated usable frame (tick)");
                    self.push_event(LogEvent::FrameAllocated);
                } else {
                    logging::error(" no more usable frames");
                    self.should_halt = true;
                }
            }
            KernelAction::MemDemo => {
                logging::info(" action = MemDemo");

                let page = VirtPage::from_index(DEMO_VIRT_PAGE_INDEX);
                let frame = PhysFrame::from_index(DEMO_PHYS_FRAME_INDEX);
                let flags = PageFlags::PRESENT | PageFlags::WRITABLE;

                let task_idx = self.current_task;
                let task = self.tasks[task_idx];
                let task_id = task.id;

                let mem_action = if !self.mem_demo_mapped[task_idx] {
                    logging::info(" mem_demo: issuing Map (for current task)");
                    MemAction::Map { page, frame, flags }
                } else {
                    logging::info(" mem_demo: issuing Unmap (for current task)");
                    MemAction::Unmap { page }
                };

                //
                // ★ AddressSpaceId を使って該当空間を参照
                //
                let as_idx = task.address_space_id.0;
                let aspace = &mut self.address_spaces[as_idx];

                match aspace.apply(mem_action) {
                    Ok(()) => {
                        logging::info(" address_space.apply: OK");

                        self.mem_demo_mapped[task_idx] = !self.mem_demo_mapped[task_idx];

                        // ★ Task0 のときだけ実ページテーブル反映
                        if task_idx == 0 {
                            logging::info(" mem_demo: applying arch paging (Task0)");
                            self.apply_mem_action(mem_action);
                        } else {
                            logging::info(" mem_demo: skip arch paging (logical only)");
                        }

                        self.push_event(LogEvent::MemActionApplied {
                            task: task_id,
                            address_space: task.address_space_id,
                            action: mem_action,
                        });
                    }
                    Err(AddressSpaceError::AlreadyMapped) => {
                        logging::info(" address_space.apply: AlreadyMapped");
                    }
                    Err(AddressSpaceError::NotMapped) => {
                        logging::info(" address_space.apply: NotMapped");
                    }
                    Err(AddressSpaceError::CapacityExceeded) => {
                        logging::info(" address_space.apply: CapacityExceeded");
                    }
                }
            }
        }

        self.update_runtime();
        self.maybe_block_current_task();
        self.update_time_slice_and_maybe_schedule();

        self.activity = next_activity;

        // 簡易的な不変条件チェック（デバッグ用）
        self.debug_check_invariants();
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    //
    // dump_events()
    //
    pub fn dump_events(&self) {
        logging::info("=== KernelState Event Log Dump ===");

        for i in 0..self.event_log_len {
            if let Some(ev) = self.event_log[i] {
                log_event_to_vga(ev);
            }
        }

        logging::info("=== End of Event Log ===");

        //
        // AddressSpace Dump (per task)
        //
        logging::info("=== AddressSpace Dump (per task) ===");

        for i in 0..self.num_tasks {
            let task = self.tasks[i];
            logging::info(" Task AddressSpace:");
            logging::info_u64("  task_index", i as u64);
            logging::info_u64("  task_id", task.id.0);

            let as_idx = task.address_space_id.0;
            let aspace = &self.address_spaces[as_idx];

            // AddressSpace が持っている root_page_frame を表示
            match aspace.root_page_frame {
                Some(root) => logging::info_u64("  root_page_frame_index", root.number),
                None => logging::info("  root_page_frame_index = None"),
            }

            // AddressSpaceId も表示する（デバッグに有効）
            logging::info_u64("  address_space_id", as_idx as u64);

            let count = aspace.mapping_count();
            logging::info_u64("  mapping_count", count as u64);

            // マッピング情報を列挙
            aspace.for_each_mapping(|m| {
                logging::info("  MAPPING:");
                logging::info_u64("    virt_page_index", m.page.number);
                logging::info_u64("    phys_frame_index", m.frame.number);
                logging::info_u64("    flags_bits", m.flags.bits());
            });
        }

        logging::info("=== End of AddressSpace Dump ===");
    }

    /// 簡易的な不変条件チェック。
    /// - 各タスクの address_space_id が有効範囲内かどうか
    /// - 将来 invariant を増やしたいときはここに追加していく
    fn debug_check_invariants(&self) {
        for (idx, task) in self.tasks.iter().enumerate().take(self.num_tasks) {
            let as_id = task.address_space_id.0;
            if as_id >= MAX_TASKS {
                logging::error("INVARIANT VIOLATION: address_space_id out of range");
                logging::error(" offending_task_index");
                logging::info_u64(" offending_task_index", idx as u64);
                logging::info_u64(" as_id", as_id as u64);
            }

            // AddressSpace が必ず存在するという前提をチェック
            // （as_id < MAX_TASKS であることを想定）
            // ここでは単に参照してみて、ログだけ出しておく。
            if as_id < MAX_TASKS {
                let _ = &self.address_spaces[as_id];
            }
        }
    }
}

//
// ──────────────────────────────────────────────
// LogEvent → VGA 出力（副作用）
// ──────────────────────────────────────────────
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

        LogEvent::MemActionApplied { task, address_space, action } => {
            logging::info("EVENT: MemActionApplied");
            logging::info_u64(" task", task.0);
            logging::info_u64(" address_space_id", address_space.0 as u64);

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
// ──────────────────────────────────────────────
// KernelActivity → (次状態, アクション)
// ──────────────────────────────────────────────
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
            (KernelActivity::MappingDemoPage, KernelAction::AllocateFrame),

        KernelActivity::MappingDemoPage =>
            (KernelActivity::Idle, KernelAction::MemDemo),
    }
}


//
// ──────────────────────────────────────────────
// カーネル起動エントリ
// ──────────────────────────────────────────────
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
