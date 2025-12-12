// src/kernel/mod.rs
//
// formal-os: 優先度付きプリエンプティブ＋ReadyQueue＋Blocked状態付きミニカーネル
//
// - Task: TaskId + TaskState + AddressSpaceId
// - AddressSpace: kind (Kernel/User) + root_page_frame + logical mappings
// - Task0 のみ実ページテーブルへの反映（map_to/unmap）
// - switch_address_space(root) で「アドレス空間切替」の抽象イベントを出す
//
// [設計上の不変条件（このモジュールにおける仕様）]
//
// 1. AddressSpaceId と kind の関係
//    - AddressSpaceId(0) は常に Kernel アドレス空間（kind = Kernel）。
//    - AddressSpaceId(1..=N-1) は User アドレス空間（kind = User）。
//
// 2. root_page_frame に関する仕様（プロトタイプ段階）
//    - Kernel アドレス空間 (id=0) だけが root_page_frame = Some(…) を持つ。
//    - User アドレス空間 (id>=1) は root_page_frame = None のまま（論理空間のみ）。
//
// 3. Task と AddressSpace の対応（現プロトタイプ仕様）
//    - tasks[i].address_space_id == AddressSpaceId(i) が成り立つ。（i < num_tasks）
//    - Task index 0（TaskId(1)）は Kernel アドレス空間 (AddressSpaceId(0)) に属する。
//    - Task index 1 以降（TaskId(2..)) は User アドレス空間 (AddressSpaceId(1..)) に属する。
//
// 4. スケジューラ/キューの整合性（デバッグで検証する）
//    - Running は常に current_task のみ
//    - ReadyQueue には Ready のみ、WaitQueue には Blocked のみ
//    - 各タスクは Ready/Wait に重複して入らない
//
// 5. 仮想アドレスレイアウト（論理マッピングの段階）
//    - User アドレス空間は high-half(kernel 空間) を使わない
//
// これらは debug_check_invariants() によってログ出力ベースで検証される。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};
use crate::mem::paging::{MemAction, PageFlags};
use crate::mem::address_space::{AddressSpace, AddressSpaceError, AddressSpaceKind};
use crate::mem::layout::KERNEL_SPACE_START;
use x86_64::registers::control::Cr3;

const MAX_TASKS: usize = 3;
const EVENT_LOG_CAP: usize = 256;

// プロトタイプ用の固定 ID（設計仕様をコードで表現するための定数）
const KERNEL_ASID_INDEX: usize = 0;          // AddressSpaces[0] は Kernel
const FIRST_USER_ASID_INDEX: usize = 1;      // AddressSpaces[1..] は User

const TASK0_INDEX: usize = 0;
const TASK1_INDEX: usize = 1;
const TASK2_INDEX: usize = 2;

const TASK0_ID: TaskId = TaskId(1);
const TASK1_ID: TaskId = TaskId(2);
const TASK2_ID: TaskId = TaskId(3);

// デモ用: 0x0010_0000 (1MiB) → 仮想ページ index
const DEMO_VIRT_PAGE_INDEX: u64 = 0x100;

//
// ──────────────────────────────────────────────
// TaskId / AddressSpaceId / TaskState / Task
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

    /// どのタスクが、どのアドレス空間に対して、どんな MemAction を起こしたか。
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

    // ReadyQueue（タスク index の配列 + len）
    ready_queue: [usize; MAX_TASKS],
    rq_len: usize,

    // WaitQueue（Blocked タスク index の配列 + len）
    wait_queue: [usize; MAX_TASKS],
    wq_len: usize,

    // 抽象イベントログ
    event_log: [Option<LogEvent>; EVENT_LOG_CAP],
    event_log_len: usize,

    // 量子
    quantum: u64,

    // MemDemo (Map/Unmap 切替)
    mem_demo_mapped: [bool; MAX_TASKS],

    // MemDemo: タスクごとにデモ用フレームを保持（初回Map時に割当）
    mem_demo_frame: [Option<PhysFrame>; MAX_TASKS],
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
                id: TASK0_ID,
                state: TaskState::Running,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 1,
                address_space_id: AddressSpaceId(KERNEL_ASID_INDEX),
            },
            Task {
                id: TASK1_ID,
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 3,
                address_space_id: AddressSpaceId(FIRST_USER_ASID_INDEX),
            },
            Task {
                id: TASK2_ID,
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 2,
                address_space_id: AddressSpaceId(FIRST_USER_ASID_INDEX + 1),
            },
        ];

        //
        // AddressSpace 配列
        // 0番: Kernel, 1・2番: User として初期化
        //
        let mut address_spaces = [
            AddressSpace::new_kernel(), // id = 0 : Kernel
            AddressSpace::new_user(),   // id = 1 : User
            AddressSpace::new_user(),   // id = 2 : User
        ];

        // Task0 の AddressSpace(0) に root_page_frame を設定
        address_spaces[KERNEL_ASID_INDEX].root_page_frame = Some(root_frame_for_task0);

        // Ready: task1, task2 を初期投入
        let ready_queue = [TASK1_INDEX, TASK2_INDEX, 0];
        let rq_len = 2;

        KernelState {
            phys_mem,
            tick_count: 0,
            time_ticks: 0,
            should_halt: false,
            activity: KernelActivity::Idle,

            address_spaces,
            tasks,
            num_tasks: MAX_TASKS,
            current_task: TASK0_INDEX,

            ready_queue,
            rq_len,

            wait_queue: [0; MAX_TASKS],
            wq_len: 0,

            event_log: [None; EVENT_LOG_CAP],
            event_log_len: 0,

            quantum: 5,
            mem_demo_mapped: [false; MAX_TASKS],
            mem_demo_frame: [None; MAX_TASKS],
        }
    }

    fn push_event(&mut self, ev: LogEvent) {
        if self.event_log_len < EVENT_LOG_CAP {
            self.event_log[self.event_log_len] = Some(ev);
            self.event_log_len += 1;
        }
    }

    //
    // 簡易的な不変条件チェック（デバッグ用）
    //
    fn debug_check_invariants(&self) {
        // ─────────────────────────────
        // 1. AddressSpace 周りの invariant
        // ─────────────────────────────

        {
            let kernel_as = &self.address_spaces[KERNEL_ASID_INDEX];

            if kernel_as.kind != AddressSpaceKind::Kernel {
                logging::error("INVARIANT VIOLATION: address_spaces[0] is not Kernel");
            }

            if kernel_as.root_page_frame.is_none() {
                logging::error("INVARIANT VIOLATION: kernel address space has no root_page_frame");
            }
        }

        for as_idx in FIRST_USER_ASID_INDEX..self.num_tasks {
            let aspace = &self.address_spaces[as_idx];

            if aspace.kind != AddressSpaceKind::User {
                logging::error("INVARIANT VIOLATION: user address space kind is not User");
                logging::info_u64(" offending_as_idx", as_idx as u64);
            }

            if aspace.root_page_frame.is_some() {
                logging::error("INVARIANT VIOLATION: user address space has root_page_frame (prototype spec expects None)");
                logging::info_u64(" offending_as_idx", as_idx as u64);
            }
        }

        for (idx, task) in self.tasks.iter().enumerate().take(self.num_tasks) {
            let as_id = task.address_space_id.0;

            if as_id >= MAX_TASKS {
                logging::error("INVARIANT VIOLATION: address_space_id out of range");
                logging::info_u64(" offending_task_index", idx as u64);
                logging::info_u64(" as_id", as_id as u64);
                continue;
            }

            if as_id != idx {
                logging::error("INVARIANT VIOLATION: task.address_space_id != task index (prototype spec)");
                logging::info_u64(" task_index", idx as u64);
                logging::info_u64(" task_address_space_id", as_id as u64);
            }

            let aspace = &self.address_spaces[as_id];

            if idx == TASK0_INDEX && aspace.kind != AddressSpaceKind::Kernel {
                logging::error("INVARIANT VIOLATION: Task0 is not in Kernel address space");
                logging::info_u64(" task_index", idx as u64);
                logging::info_u64(" as_id", as_id as u64);
            }

            if idx >= FIRST_USER_ASID_INDEX && aspace.kind != AddressSpaceKind::User {
                logging::error("INVARIANT VIOLATION: user task is not in User address space");
                logging::info_u64(" task_index", idx as u64);
                logging::info_u64(" as_id", as_id as u64);
            }
        }

        // ─────────────────────────────
        // 2. スケジューラ / キュー構造の invariant（完全版）
        // ─────────────────────────────

        // 2-1. current_task は必ず Running
        if self.current_task >= self.num_tasks {
            logging::error("INVARIANT VIOLATION: current_task index out of range");
        } else if self.tasks[self.current_task].state != TaskState::Running {
            logging::error("INVARIANT VIOLATION: current_task is not RUNNING");
            logging::info_u64(" current_task_index", self.current_task as u64);
        }

        // 2-2. Running は current_task だけ
        for (idx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            if idx == self.current_task {
                continue;
            }
            if t.state == TaskState::Running {
                logging::error("INVARIANT VIOLATION: multiple RUNNING tasks");
                logging::info_u64(" extra_running_task_index", idx as u64);
            }
        }

        // 2-3. ready_queue / wait_queue の内容を集計（重複チェック込み）
        let mut in_ready = [false; MAX_TASKS];
        let mut in_wait = [false; MAX_TASKS];

        // ReadyQueue: 0..rq_len
        for pos in 0..self.rq_len {
            let idx = self.ready_queue[pos];

            if idx >= self.num_tasks {
                logging::error("INVARIANT VIOLATION: ready_queue contains invalid task index");
                logging::info_u64(" queue_pos", pos as u64);
                logging::info_u64(" task_index", idx as u64);
                continue;
            }

            if in_ready[idx] {
                logging::error("INVARIANT VIOLATION: task appears multiple times in ready_queue");
                logging::info_u64(" task_index", idx as u64);
            }
            in_ready[idx] = true;

            if self.tasks[idx].state != TaskState::Ready {
                logging::error("INVARIANT VIOLATION: task in ready_queue is not READY");
                logging::info_u64(" task_index", idx as u64);
            }
        }

        // WaitQueue: 0..wq_len
        for pos in 0..self.wq_len {
            let idx = self.wait_queue[pos];

            if idx >= self.num_tasks {
                logging::error("INVARIANT VIOLATION: wait_queue contains invalid task index");
                logging::info_u64(" queue_pos", pos as u64);
                logging::info_u64(" task_index", idx as u64);
                continue;
            }

            if in_wait[idx] {
                logging::error("INVARIANT VIOLATION: task appears multiple times in wait_queue");
                logging::info_u64(" task_index", idx as u64);
            }
            in_wait[idx] = true;

            if self.tasks[idx].state != TaskState::Blocked {
                logging::error("INVARIANT VIOLATION: task in wait_queue is not BLOCKED");
                logging::info_u64(" task_index", idx as u64);
            }
        }

        // 2-4. Ready と Blocked が両方に入っていない
        for idx in 0..self.num_tasks {
            if in_ready[idx] && in_wait[idx] {
                logging::error("INVARIANT VIOLATION: task is in both ready_queue and wait_queue");
                logging::info_u64(" task_index", idx as u64);
            }
        }

        // 2-5. state と所属の逆方向チェック
        for idx in 0..self.num_tasks {
            match self.tasks[idx].state {
                TaskState::Running => {
                    if idx != self.current_task {
                        logging::error("INVARIANT VIOLATION: non-current task is RUNNING");
                        logging::info_u64(" task_index", idx as u64);
                    }
                    if in_ready[idx] || in_wait[idx] {
                        logging::error("INVARIANT VIOLATION: RUNNING task appears in ready/wait queue");
                        logging::info_u64(" task_index", idx as u64);
                    }
                }
                TaskState::Ready => {
                    if !in_ready[idx] {
                        logging::error("INVARIANT VIOLATION: READY task not in ready_queue");
                        logging::info_u64(" task_index", idx as u64);
                    }
                    if in_wait[idx] {
                        logging::error("INVARIANT VIOLATION: READY task in wait_queue");
                        logging::info_u64(" task_index", idx as u64);
                    }
                }
                TaskState::Blocked => {
                    if !in_wait[idx] {
                        logging::error("INVARIANT VIOLATION: BLOCKED task not in wait_queue");
                        logging::info_u64(" task_index", idx as u64);
                    }
                    if in_ready[idx] {
                        logging::error("INVARIANT VIOLATION: BLOCKED task in ready_queue");
                        logging::info_u64(" task_index", idx as u64);
                    }
                    if idx == self.current_task {
                        logging::error("INVARIANT VIOLATION: current_task is BLOCKED");
                        logging::info_u64(" task_index", idx as u64);
                    }
                }
            }
        }

        // ─────────────────────────────
        // 3. 仮想アドレスレイアウト invariant（論理マッピング）
        //    - User は high-half を使わない
        // ─────────────────────────────
        for as_idx in FIRST_USER_ASID_INDEX..self.num_tasks {
            let aspace = &self.address_spaces[as_idx];
            if aspace.kind != AddressSpaceKind::User {
                continue;
            }

            aspace.for_each_mapping(|m| {
                let virt_addr = m.page.number * PAGE_SIZE;
                if virt_addr >= KERNEL_SPACE_START {
                    logging::error("INVARIANT VIOLATION: user mapping in kernel-space range");
                    logging::info_u64(" as_idx", as_idx as u64);
                    logging::info_u64(" virt_page_index", m.page.number);
                    logging::info_u64(" virt_addr", virt_addr);
                }
            });
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
    // ReadyQueue / WaitQueue: 存在チェック（二重 enqueue 防止）
    //
    fn is_in_ready_queue(&self, idx: usize) -> bool {
        for pos in 0..self.rq_len {
            if self.ready_queue[pos] == idx {
                return true;
            }
        }
        false
    }

    fn is_in_wait_queue(&self, idx: usize) -> bool {
        for pos in 0..self.wq_len {
            if self.wait_queue[pos] == idx {
                return true;
            }
        }
        false
    }

    //
    // ReadyQueue
    //
    fn enqueue_ready(&mut self, idx: usize) {
        if self.rq_len >= MAX_TASKS {
            return;
        }
        if idx >= self.num_tasks {
            return;
        }
        if self.is_in_ready_queue(idx) {
            return;
        }
        if self.tasks[idx].state != TaskState::Ready {
            return;
        }

        self.ready_queue[self.rq_len] = idx;
        self.rq_len += 1;

        self.push_event(LogEvent::ReadyQueued(self.tasks[idx].id));
    }

    fn dequeue_ready_highest_priority(&mut self) -> Option<usize> {
        if self.rq_len == 0 {
            return None;
        }

        let mut best_pos = 0usize;
        let mut best_idx = self.ready_queue[0];
        let mut best_prio = self.tasks[best_idx].priority;

        for pos in 1..self.rq_len {
            let idx = self.ready_queue[pos];
            let prio = self.tasks[idx].priority;
            if prio > best_prio {
                best_prio = prio;
                best_idx = idx;
                best_pos = pos;
            }
        }

        // swap-remove
        let last_pos = self.rq_len - 1;
        self.ready_queue[best_pos] = self.ready_queue[last_pos];
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
        if idx >= self.num_tasks {
            return;
        }
        if self.is_in_wait_queue(idx) {
            return;
        }
        if self.tasks[idx].state != TaskState::Blocked {
            return;
        }

        self.wait_queue[self.wq_len] = idx;
        self.wq_len += 1;

        self.push_event(LogEvent::WaitQueued(self.tasks[idx].id));
    }

    fn dequeue_wait(&mut self) -> Option<usize> {
        if self.wq_len == 0 {
            return None;
        }

        // 順序は抽象化されているので、ここも swap-remove で OK
        let last_pos = self.wq_len - 1;
        let idx = self.wait_queue[last_pos];
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

            // AddressSpace の root_page_frame を参照して switch に渡す
            let root = self.address_spaces[as_idx].root_page_frame;
            arch::paging::switch_address_space(root);

            self.push_event(LogEvent::TaskSwitched(next_id));
            self.push_event(LogEvent::TaskStateChanged(next_id, TaskState::Running));
        } else {
            logging::info(" no ready tasks; scheduler idle");
        }
    }

    //
    // runtime 更新（この tick で実行したタスク ran_idx にだけ適用）
    //
    fn update_runtime_for(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_runtime_for: ran_idx out of range");
            return;
        }

        let id = self.tasks[ran_idx].id;

        self.tasks[ran_idx].runtime_ticks += 1;
        logging::info_u64(" runtime_ticks", self.tasks[ran_idx].runtime_ticks);

        self.push_event(LogEvent::RuntimeUpdated(
            id,
            self.tasks[ran_idx].runtime_ticks,
        ));
    }

    //
    // 疑似 Blocked（この tick で実行したタスク ran_idx に対して判定）
    // 戻り値: この tick で block が発生したら true（この場合 time_slice 更新はスキップ推奨）
    //
    fn maybe_block_task(&mut self, ran_idx: usize) -> bool {
        if ran_idx >= self.num_tasks {
            logging::error("maybe_block_task: ran_idx out of range");
            return false;
        }

        if ran_idx != self.current_task {
            logging::error("INVARIANT VIOLATION: ran_idx != current_task at block check");
            logging::info_u64(" ran_idx", ran_idx as u64);
            logging::info_u64(" current_task", self.current_task as u64);
            return false;
        }

        if self.tick_count != 0
            && self.tick_count % 7 == 0
            && self.tasks[ran_idx].id.0 == 2
        {
            let id = self.tasks[ran_idx].id;

            logging::info(" blocking current task (fake I/O wait)");
            self.tasks[ran_idx].state = TaskState::Blocked;
            self.tasks[ran_idx].time_slice_used = 0;

            self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));

            self.enqueue_wait(ran_idx);
            self.schedule_next_task();
            return true;
        }

        false
    }

    //
    // time slice 更新（この tick で実行したタスク ran_idx にだけ適用）
    //
    fn update_time_slice_for_and_maybe_schedule(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_time_slice_for_and_maybe_schedule: ran_idx out of range");
            return;
        }

        let id = self.tasks[ran_idx].id;

        self.tasks[ran_idx].time_slice_used += 1;
        logging::info_u64(" time_slice_used", self.tasks[ran_idx].time_slice_used);

        if self.tasks[ran_idx].time_slice_used >= self.quantum {
            logging::info(" quantum expired; scheduling next task");
            self.push_event(LogEvent::QuantumExpired(id, self.tasks[ran_idx].time_slice_used));

            if ran_idx == self.current_task {
                self.schedule_next_task();
            } else {
                logging::info(" quantum expired but task already switched in this tick; skip schedule");
            }
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

    //
    // MemDemo: Map に必要なフレームを「初回だけ」割り当てる
    //
    fn get_or_alloc_demo_frame(&mut self, task_idx: usize) -> Option<PhysFrame> {
        if task_idx >= self.num_tasks {
            return None;
        }

        if let Some(f) = self.mem_demo_frame[task_idx] {
            return Some(f);
        }

        match self.phys_mem.allocate_frame() {
            Some(raw_frame) => {
                let phys_u64 = raw_frame.start_address().as_u64();
                let frame_index = phys_u64 / PAGE_SIZE;

                let f = PhysFrame::from_index(frame_index);

                self.push_event(LogEvent::FrameAllocated);
                self.mem_demo_frame[task_idx] = Some(f);
                Some(f)
            }
            None => None,
        }
    }

    //
    // tick()
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

        // この tick で「実行した」タスク index を固定
        let ran_idx = self.current_task;

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
                    logging::error(" no more usable frames; halting later");
                    self.should_halt = true;
                }
            }
            KernelAction::MemDemo => {
                logging::info(" action = MemDemo");

                let page = VirtPage::from_index(DEMO_VIRT_PAGE_INDEX);
                let flags = PageFlags::PRESENT | PageFlags::WRITABLE;

                let task_idx = self.current_task;
                let task = self.tasks[task_idx];
                let task_id = task.id;

                let mem_action = if !self.mem_demo_mapped[task_idx] {
                    logging::info(" mem_demo: issuing Map (for current task)");

                    let frame = match self.get_or_alloc_demo_frame(task_idx) {
                        Some(f) => f,
                        None => {
                            logging::error(" mem_demo: no more usable frames");
                            self.should_halt = true;
                            self.activity = next_activity;
                            self.debug_check_invariants();
                            return;
                        }
                    };

                    MemAction::Map { page, frame, flags }
                } else {
                    logging::info(" mem_demo: issuing Unmap (for current task)");
                    MemAction::Unmap { page }
                };

                let as_idx = task.address_space_id.0;
                let aspace = &mut self.address_spaces[as_idx];

                match aspace.apply(mem_action) {
                    Ok(()) => {
                        logging::info(" address_space.apply: OK");

                        self.mem_demo_mapped[task_idx] = !self.mem_demo_mapped[task_idx];

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

        self.update_runtime_for(ran_idx);

        let blocked = self.maybe_block_task(ran_idx);
        if !blocked {
            self.update_time_slice_for_and_maybe_schedule(ran_idx);
        } else {
            logging::info(" skip time_slice update due to block in this tick");
        }

        self.activity = next_activity;

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

        logging::info("=== AddressSpace Dump (per task) ===");

        for i in 0..self.num_tasks {
            let task = self.tasks[i];

            logging::info(" Task AddressSpace:");
            logging::info_u64("  task_index", i as u64);
            logging::info_u64("  task_id", task.id.0);

            let as_idx = task.address_space_id.0;
            let aspace = &self.address_spaces[as_idx];

            match aspace.kind {
                AddressSpaceKind::Kernel => logging::info("  kind = Kernel"),
                AddressSpaceKind::User => logging::info("  kind = User"),
            }

            match aspace.root_page_frame {
                Some(root) => logging::info_u64("  root_page_frame_index", root.number),
                None => logging::info("  root_page_frame_index = None"),
            }

            logging::info_u64("  address_space_id", as_idx as u64);

            let count = aspace.mapping_count();
            logging::info_u64("  mapping_count", count as u64);

            aspace.for_each_mapping(|m| {
                logging::info("  MAPPING:");
                logging::info_u64("    virt_page_index", m.page.number);
                logging::info_u64("    phys_frame_index", m.frame.number);
                logging::info_u64("    flags_bits", m.flags.bits());
            });
        }

        logging::info("=== End of AddressSpace Dump ===");
    }
}

// ─────────────────────────────────────────────
// LogEvent → VGA 出力
// ─────────────────────────────────────────────

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

// ─────────────────────────────────────────────
// KernelActivity → (次状態, アクション)
// ─────────────────────────────────────────────

fn next_activity_and_action(current: KernelActivity) -> (KernelActivity, KernelAction) {
    match current {
        KernelActivity::Idle => (KernelActivity::UpdatingTimer, KernelAction::None),

        KernelActivity::UpdatingTimer => (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer),

        KernelActivity::AllocatingFrame => (KernelActivity::MappingDemoPage, KernelAction::AllocateFrame),

        KernelActivity::MappingDemoPage => (KernelActivity::Idle, KernelAction::MemDemo),
    }
}

// ─────────────────────────────────────────────
// カーネル起動エントリ
// ─────────────────────────────────────────────

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
