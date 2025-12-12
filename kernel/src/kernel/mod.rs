// kernel/src/kernel/mod.rs
//
// formal-os: 優先度付きプリエンプティブ＋ReadyQueue＋Blocked状態付きミニカーネル + IPC(Endpoint)
//
// 目的:
// - タスク状態遷移（Ready/Running/Blocked）とキュー整合性を、ログと invariant で追える形にする。
// - AddressSpace の分離（root(PML4) 違いで同一VAが別PAに解決される）を、translateログで示す。
// - BlockedReason を導入し、IPC（send/recv/reply）の待ちを自然に表現する。
// - Endpoint を追加し、同期 IPC（send/recv/reply）のプロトタイプを動かす。
// - Aステップ: 複数 sender による send を成立させ、reply_queue が複数要素になることをログで示す。
//
// 設計方針:
// - unsafe は arch 側に局所化し、kernel 側は状態遷移＋抽象イベント中心。
// - WaitQueue は「Blocked 全体」を保持する。
//   * Sleep の wake は “Sleep のみ” を対象にする（IPC の待ちをタイマで勝手に起こさない）。
// - tick 中に schedule が走って current_task が変わるのは自然に起こりうる。
//   * ただし time_slice 更新は「その tick の最後まで同じ task が RUNNING の場合のみ」行う。
//     （IPC などで tick 中にブロック/切替が起きた場合、time_slice を誤って加算しない）
//
// [IPC 仕様（このモジュールにおける約束事）]
// - Endpoint.recv_waiter = Some(tidx) のとき:
//     tasks[tidx].state == Blocked
//     tasks[tidx].blocked_reason == Some(IpcRecv{ep})
// - Endpoint.send_queue 内の tidx は:
//     tasks[tidx].state == Blocked
//     tasks[tidx].blocked_reason == Some(IpcSend{ep})
// - Endpoint.reply_queue 内の tidx は:
//     tasks[tidx].state == Blocked
//     tasks[tidx].blocked_reason == Some(IpcReply{ep, partner})
// - タイマ wake は Sleep のみ（Ipc* を起こすのは Endpoint の deliver/reply のみ）
// - IPC による wake は必ず wake_task_to_ready() 経由で行う（WaitQueueと整合させる）
//
// [IPC デモ(A): 2 send を溜めて reply_queue を観察]
// - Receiver(Task3) が recv を2回行い、Task2→Task1 の send をそれぞれ deliver させる。
// - その時点で reply_queue_len が 2 になる。
// - Receiver(Task3) が reply を2回行い、reply_queue_len が 2→1→0 になる。
// - 送信者の起床順序は “抽象化” される（swap-remove のため順序は仕様化しない）。

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

const MAX_ENDPOINTS: usize = 2;

// 固定 ID
const KERNEL_ASID_INDEX: usize = 0;
const FIRST_USER_ASID_INDEX: usize = 1;

const TASK0_INDEX: usize = 0; // TaskId(1)
const TASK1_INDEX: usize = 1; // TaskId(2)
const TASK2_INDEX: usize = 2; // TaskId(3)

const TASK0_ID: TaskId = TaskId(1);
const TASK1_ID: TaskId = TaskId(2);
const TASK2_ID: TaskId = TaskId(3);

// MemDemo: Task別の仮想ページ
const DEMO_VIRT_PAGE_INDEX_TASK0: u64 = 0x100; // 0x0010_0000
const DEMO_VIRT_PAGE_INDEX_USER:  u64 = 0x110; // 0x0011_0000  ← Task1/Task2 共通

const IPC_DEMO_EP0: EndpointId = EndpointId(0);

//
// ──────────────────────────────────────────────
// Task / IPC types
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TaskId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AddressSpaceId(pub usize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EndpointId(pub usize);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockedReason {
    Sleep,
    IpcRecv { ep: EndpointId },
    IpcSend { ep: EndpointId },
    IpcReply { partner: TaskId, ep: EndpointId },
}

#[derive(Clone, Copy)]
pub struct Task {
    pub id: TaskId,
    pub state: TaskState,
    pub runtime_ticks: u64,
    pub time_slice_used: u64,
    pub priority: u8,

    pub address_space_id: AddressSpaceId,
    pub blocked_reason: Option<BlockedReason>,

    // IPC demo helper
    pub last_msg: Option<u64>,
    pub pending_send_msg: Option<u64>,
}

//
// ──────────────────────────────────────────────
// Endpoint（reply_queue 版）
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub struct Endpoint {
    pub id: EndpointId,

    pub recv_waiter: Option<usize>,

    pub send_queue: [usize; MAX_TASKS],
    pub sq_len: usize,

    pub reply_queue: [usize; MAX_TASKS],
    pub rq_len: usize,
}

impl Endpoint {
    pub const fn new(id: EndpointId) -> Self {
        Endpoint {
            id,
            recv_waiter: None,
            send_queue: [0; MAX_TASKS],
            sq_len: 0,
            reply_queue: [0; MAX_TASKS],
            rq_len: 0,
        }
    }

    fn send_queue_contains(&self, idx: usize) -> bool {
        for pos in 0..self.sq_len {
            if self.send_queue[pos] == idx {
                return true;
            }
        }
        false
    }

    fn reply_queue_contains(&self, idx: usize) -> bool {
        for pos in 0..self.rq_len {
            if self.reply_queue[pos] == idx {
                return true;
            }
        }
        false
    }

    fn enqueue_sender(&mut self, idx: usize) {
        if self.sq_len >= MAX_TASKS {
            return;
        }
        if self.send_queue_contains(idx) {
            return;
        }
        self.send_queue[self.sq_len] = idx;
        self.sq_len += 1;
    }

    fn dequeue_sender(&mut self) -> Option<usize> {
        if self.sq_len == 0 {
            return None;
        }
        // swap-remove
        let last = self.sq_len - 1;
        let idx = self.send_queue[last];
        self.sq_len -= 1;
        Some(idx)
    }

    fn enqueue_reply_waiter(&mut self, idx: usize) {
        if self.rq_len >= MAX_TASKS {
            return;
        }
        if self.reply_queue_contains(idx) {
            return;
        }
        self.reply_queue[self.rq_len] = idx;
        self.rq_len += 1;
    }

    fn remove_reply_waiter_at(&mut self, pos: usize) -> Option<usize> {
        if pos >= self.rq_len {
            return None;
        }
        // swap-remove（順序は抽象化）
        let last = self.rq_len - 1;
        let idx = self.reply_queue[pos];
        self.reply_queue[pos] = self.reply_queue[last];
        self.rq_len -= 1;
        Some(idx)
    }
}

//
// ──────────────────────────────────────────────
// LogEvent
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

    MemActionApplied {
        task: TaskId,
        address_space: AddressSpaceId,
        action: MemAction,
    },

    // IPC
    IpcRecvCalled { task: TaskId, ep: EndpointId },
    IpcRecvBlocked { task: TaskId, ep: EndpointId },
    IpcSendCalled { task: TaskId, ep: EndpointId, msg: u64 },
    IpcSendBlocked { task: TaskId, ep: EndpointId },
    IpcDelivered { from: TaskId, to: TaskId, ep: EndpointId, msg: u64 },
    IpcReplyCalled { task: TaskId, ep: EndpointId, to: TaskId },
    IpcReplyDelivered { from: TaskId, to: TaskId, ep: EndpointId },
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
    IpcDemo,
}

#[derive(Clone, Copy)]
enum KernelAction {
    None,
    UpdateTimer,
    AllocateFrame,
    MemDemo,
    IpcDemo,
}

//
// ──────────────────────────────────────────────
// KernelState
// ──────────────────────────────────────────────
//

pub struct KernelState {
    phys_mem: PhysicalMemoryManager,

    tick_count: u64,
    time_ticks: u64,
    should_halt: bool,
    activity: KernelActivity,

    address_spaces: [AddressSpace; MAX_TASKS],

    tasks: [Task; MAX_TASKS],
    num_tasks: usize,
    current_task: usize,

    ready_queue: [usize; MAX_TASKS],
    rq_len: usize,

    wait_queue: [usize; MAX_TASKS],
    wq_len: usize,

    event_log: [Option<LogEvent>; EVENT_LOG_CAP],
    event_log_len: usize,

    quantum: u64,

    mem_demo_mapped: [bool; MAX_TASKS],
    mem_demo_frame: [Option<PhysFrame>; MAX_TASKS],

    endpoints: [Endpoint; MAX_ENDPOINTS],

    // IPC demo(A) state
    ipc_a_msgs_delivered: u8,   // 0..=2
    ipc_a_replies_sent: u8,     // 0..=2
    ipc_a_sent_by_task2: bool,
    ipc_a_sent_by_task1: bool,
}

impl KernelState {
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let mut phys_mem = PhysicalMemoryManager::new(boot_info);

        let root_frame_for_task0: PhysFrame = {
            let (level_4_frame, _) = Cr3::read();
            let phys_u64 = level_4_frame.start_address().as_u64();
            let frame_index = phys_u64 / PAGE_SIZE;
            PhysFrame::from_index(frame_index)
        };

        let tasks = [
            Task {
                id: TASK0_ID,
                state: TaskState::Running,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 1,
                address_space_id: AddressSpaceId(KERNEL_ASID_INDEX),
                blocked_reason: None,
                last_msg: None,
                pending_send_msg: None,
            },
            Task {
                id: TASK1_ID,
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 3,
                address_space_id: AddressSpaceId(FIRST_USER_ASID_INDEX),
                blocked_reason: None,
                last_msg: None,
                pending_send_msg: None,
            },
            Task {
                id: TASK2_ID,
                state: TaskState::Ready,
                runtime_ticks: 0,
                time_slice_used: 0,
                priority: 2,
                address_space_id: AddressSpaceId(FIRST_USER_ASID_INDEX + 1),
                blocked_reason: None,
                last_msg: None,
                pending_send_msg: None,
            },
        ];

        let mut address_spaces = [
            AddressSpace::new_kernel(),
            AddressSpace::new_user(),
            AddressSpace::new_user(),
        ];

        address_spaces[KERNEL_ASID_INDEX].root_page_frame = Some(root_frame_for_task0);

        for as_idx in FIRST_USER_ASID_INDEX..MAX_TASKS {
            let raw = match phys_mem.allocate_frame() {
                Some(f) => f,
                None => {
                    logging::error("no more frames for user pml4");
                    continue;
                }
            };

            let phys_u64 = raw.start_address().as_u64();
            let frame_index = phys_u64 / PAGE_SIZE;
            let user_root = PhysFrame::from_index(frame_index);

            address_spaces[as_idx].root_page_frame = Some(user_root);
            arch::paging::init_user_pml4_from_current(user_root);
        }

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

            endpoints: [
                Endpoint::new(EndpointId(0)),
                Endpoint::new(EndpointId(1)),
            ],

            ipc_a_msgs_delivered: 0,
            ipc_a_replies_sent: 0,
            ipc_a_sent_by_task2: false,
            ipc_a_sent_by_task1: false,
        }
    }

    fn push_event(&mut self, ev: LogEvent) {
        if self.event_log_len < EVENT_LOG_CAP {
            self.event_log[self.event_log_len] = Some(ev);
            self.event_log_len += 1;
        }
    }

    //
    // Invariants
    //
    fn debug_check_invariants(&self) {
        // AddressSpace invariants
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
            }
            if aspace.root_page_frame.is_none() {
                logging::error("INVARIANT VIOLATION: user address space has no root_page_frame");
            }
        }

        for (idx, task) in self.tasks.iter().enumerate().take(self.num_tasks) {
            let as_id = task.address_space_id.0;
            if as_id >= MAX_TASKS {
                logging::error("INVARIANT VIOLATION: address_space_id out of range");
                logging::info_u64(" task_index", idx as u64);
                logging::info_u64(" as_id", as_id as u64);
            }
            if as_id != idx {
                logging::error("INVARIANT VIOLATION: task.address_space_id != task index (prototype spec)");
                logging::info_u64(" task_index", idx as u64);
                logging::info_u64(" as_id", as_id as u64);
            }
        }

        // Scheduler/queues invariants
        for (idx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            match t.state {
                TaskState::Blocked => {
                    if t.blocked_reason.is_none() {
                        logging::error("INVARIANT VIOLATION: BLOCKED task has no blocked_reason");
                        logging::info_u64(" task_index", idx as u64);
                    }
                }
                _ => {
                    if t.blocked_reason.is_some() {
                        logging::error("INVARIANT VIOLATION: non-BLOCKED task has blocked_reason");
                        logging::info_u64(" task_index", idx as u64);
                    }
                }
            }
        }

        if self.current_task >= self.num_tasks {
            logging::error("INVARIANT VIOLATION: current_task index out of range");
        } else if self.tasks[self.current_task].state != TaskState::Running {
            logging::error("INVARIANT VIOLATION: current_task is not RUNNING");
            logging::info_u64(" current_task_index", self.current_task as u64);
        }

        // User mapping invariant (no high-half)
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
                }
            });
        }

        // Endpoint invariants
        for e in self.endpoints.iter() {
            if let Some(idx) = e.recv_waiter {
                if idx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.recv_waiter out of range");
                } else {
                    let t = &self.tasks[idx];
                    if t.state != TaskState::Blocked {
                        logging::error("INVARIANT VIOLATION: recv_waiter is not BLOCKED");
                    }
                    match t.blocked_reason {
                        Some(BlockedReason::IpcRecv { ep }) if ep == e.id => {}
                        _ => logging::error("INVARIANT VIOLATION: recv_waiter blocked_reason mismatch"),
                    }
                }
            }

            for pos in 0..e.sq_len {
                let idx = e.send_queue[pos];
                if idx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.send_queue idx out of range");
                    continue;
                }
                let t = &self.tasks[idx];
                if t.state != TaskState::Blocked {
                    logging::error("INVARIANT VIOLATION: sender in send_queue is not BLOCKED");
                }
                match t.blocked_reason {
                    Some(BlockedReason::IpcSend { ep }) if ep == e.id => {}
                    _ => logging::error("INVARIANT VIOLATION: sender blocked_reason mismatch"),
                }
            }

            for pos in 0..e.rq_len {
                let idx = e.reply_queue[pos];
                if idx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.reply_queue idx out of range");
                    continue;
                }
                let t = &self.tasks[idx];
                if t.state != TaskState::Blocked {
                    logging::error("INVARIANT VIOLATION: reply waiter is not BLOCKED");
                }
                match t.blocked_reason {
                    Some(BlockedReason::IpcReply { ep, .. }) if ep == e.id => {}
                    _ => logging::error("INVARIANT VIOLATION: reply waiter blocked_reason mismatch"),
                }
            }
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
    // Queue helpers
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

    fn remove_from_wait_queue(&mut self, idx: usize) -> bool {
        if idx >= self.num_tasks {
            return false;
        }
        for pos in 0..self.wq_len {
            if self.wait_queue[pos] == idx {
                let last = self.wq_len - 1;
                self.wait_queue[pos] = self.wait_queue[last];
                self.wq_len -= 1;

                self.push_event(LogEvent::WaitDequeued(self.tasks[idx].id));
                return true;
            }
        }
        false
    }

    //
    // ReadyQueue
    //
    fn enqueue_ready(&mut self, idx: usize) {
        if self.rq_len >= MAX_TASKS || idx >= self.num_tasks {
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
        if self.wq_len >= MAX_TASKS || idx >= self.num_tasks {
            return;
        }
        if self.is_in_wait_queue(idx) {
            return;
        }
        if self.tasks[idx].state != TaskState::Blocked {
            return;
        }
        if self.tasks[idx].blocked_reason.is_none() {
            return;
        }

        self.wait_queue[self.wq_len] = idx;
        self.wq_len += 1;

        self.push_event(LogEvent::WaitQueued(self.tasks[idx].id));
    }

    //
    // Scheduler
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
            self.tasks[next_idx].blocked_reason = None;
            self.current_task = next_idx;

            logging::info(" switched to task");
            logging::info_u64(" task_id", next_id.0);

            let root = self.address_spaces[as_idx].root_page_frame;
            arch::paging::switch_address_space(root);

            self.push_event(LogEvent::TaskSwitched(next_id));
            self.push_event(LogEvent::TaskStateChanged(next_id, TaskState::Running));
        } else {
            logging::info(" no ready tasks; scheduler idle");
        }
    }

    //
    // runtime
    //
    fn update_runtime_for(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_runtime_for: ran_idx out of range");
            return;
        }
        let id = self.tasks[ran_idx].id;
        self.tasks[ran_idx].runtime_ticks += 1;
        logging::info_u64(" runtime_ticks", self.tasks[ran_idx].runtime_ticks);
        self.push_event(LogEvent::RuntimeUpdated(id, self.tasks[ran_idx].runtime_ticks));
    }

    //
    // block current
    //
    fn block_current(&mut self, reason: BlockedReason) {
        let idx = self.current_task;
        let id = self.tasks[idx].id;

        self.tasks[idx].state = TaskState::Blocked;
        self.tasks[idx].blocked_reason = Some(reason);
        self.tasks[idx].time_slice_used = 0;

        self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));
        self.enqueue_wait(idx);
    }

    fn block_current_and_schedule(&mut self, reason: BlockedReason) {
        self.block_current(reason);
        self.schedule_next_task();
    }

    fn wake_task_to_ready(&mut self, idx: usize) {
        if idx >= self.num_tasks {
            return;
        }
        if self.tasks[idx].state != TaskState::Blocked {
            logging::error("wake_task_to_ready: target is not BLOCKED");
            return;
        }

        let _ = self.remove_from_wait_queue(idx);
        let id = self.tasks[idx].id;

        self.tasks[idx].state = TaskState::Ready;
        self.tasks[idx].blocked_reason = None;
        self.tasks[idx].time_slice_used = 0;

        self.push_event(LogEvent::TaskStateChanged(id, TaskState::Ready));
        self.enqueue_ready(idx);
    }

    //
    // fake block (Sleep)
    //
    fn maybe_block_task(&mut self, ran_idx: usize) -> bool {
        if ran_idx >= self.num_tasks {
            logging::error("maybe_block_task: ran_idx out of range");
            return false;
        }
        if ran_idx != self.current_task {
            return false;
        }

        if self.tick_count != 0
            && self.tick_count % 7 == 0
            && self.tasks[ran_idx].id.0 == 2
        {
            logging::info(" blocking current task (fake I/O wait)");
            self.block_current_and_schedule(BlockedReason::Sleep);
            return true;
        }

        false
    }

    //
    // time slice
    //
    fn update_time_slice_for_and_maybe_schedule(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_time_slice_for_and_maybe_schedule: ran_idx out of range");
            return;
        }
        if ran_idx != self.current_task {
            logging::info(" time_slice update skipped (task switched in this tick)");
            return;
        }
        if self.tasks[ran_idx].state != TaskState::Running {
            logging::info(" time_slice update skipped (task not RUNNING)");
            return;
        }

        let id = self.tasks[ran_idx].id;
        self.tasks[ran_idx].time_slice_used += 1;
        logging::info_u64(" time_slice_used", self.tasks[ran_idx].time_slice_used);

        if self.tasks[ran_idx].time_slice_used >= self.quantum {
            logging::info(" quantum expired; scheduling next task");
            self.push_event(LogEvent::QuantumExpired(id, self.tasks[ran_idx].time_slice_used));
            self.schedule_next_task();
        }
    }

    //
    // Wake Sleep only
    //
    fn maybe_wake_one_sleep_task(&mut self) {
        for pos in 0..self.wq_len {
            let idx = self.wait_queue[pos];
            if idx >= self.num_tasks {
                continue;
            }
            if self.tasks[idx].blocked_reason == Some(BlockedReason::Sleep) {
                logging::info(" waking 1 blocked task (Sleep only)");
                self.wake_task_to_ready(idx);
                return;
            }
        }
    }

    //
    // MemDemo
    //
    fn apply_mem_action(&mut self, action: MemAction) {
        unsafe {
            arch::paging::apply_mem_action(action, &mut self.phys_mem);
        }
    }

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

    fn demo_page_for_task(&self, task_idx: usize) -> VirtPage {
        let idx = match task_idx {
            TASK0_INDEX => DEMO_VIRT_PAGE_INDEX_TASK0,
            TASK1_INDEX => DEMO_VIRT_PAGE_INDEX_USER,
            TASK2_INDEX => DEMO_VIRT_PAGE_INDEX_USER,
            _ => DEMO_VIRT_PAGE_INDEX_TASK0,
        };
        VirtPage::from_index(idx)
    }

    fn do_mem_demo(&mut self) {
        let task_idx = self.current_task;
        let task = self.tasks[task_idx];
        let task_id = task.id;

        let page = self.demo_page_for_task(task_idx);

        let flags = if task_idx == TASK0_INDEX {
            PageFlags::PRESENT | PageFlags::WRITABLE
        } else {
            PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER
        };

        let mem_action = if !self.mem_demo_mapped[task_idx] {
            logging::info(" mem_demo: issuing Map (for current task)");

            let frame = match self.get_or_alloc_demo_frame(task_idx) {
                Some(f) => f,
                None => {
                    logging::error(" mem_demo: no more usable frames");
                    self.should_halt = true;
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

                if task_idx == TASK0_INDEX {
                    logging::info(" mem_demo: applying arch paging (Task0 / current CR3)");
                    self.apply_mem_action(mem_action);
                } else {
                    let root = match aspace.root_page_frame {
                        Some(r) => r,
                        None => {
                            logging::error(" mem_demo: user root_page_frame is None (unexpected)");
                            return;
                        }
                    };

                    logging::info(" mem_demo: applying arch paging (User root / no CR3 switch)");
                    unsafe {
                        arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem);
                    }

                    let virt_addr_u64 = page.start_address().0;
                    arch::paging::debug_translate_in_root(root, virt_addr_u64);
                }

                self.push_event(LogEvent::MemActionApplied {
                    task: task_id,
                    address_space: task.address_space_id,
                    action: mem_action,
                });
            }
            Err(AddressSpaceError::AlreadyMapped) => logging::info(" address_space.apply: AlreadyMapped"),
            Err(AddressSpaceError::NotMapped) => logging::info(" address_space.apply: NotMapped"),
            Err(AddressSpaceError::CapacityExceeded) => logging::info(" address_space.apply: CapacityExceeded"),
            Err(AddressSpaceError::UserMappingInKernelSpace) => logging::error(" address_space.apply: UserMappingInKernelSpace"),
            Err(AddressSpaceError::UserMappingMissingUserFlag) => logging::error(" address_space.apply: UserMappingMissingUserFlag"),
            Err(AddressSpaceError::KernelMappingHasUserFlag) => logging::error(" address_space.apply: KernelMappingHasUserFlag"),
        }
    }

    //
    // ──────────────────────────────────────────────
    // IPC core
    // ──────────────────────────────────────────────
    //

    fn take_reply_waiter_for_partner(&mut self, ep: EndpointId, partner: TaskId) -> Option<usize> {
        if ep.0 >= MAX_ENDPOINTS {
            return None;
        }

        let e = &mut self.endpoints[ep.0];

        // 後ろから探す（順序は抽象化、swap-remove を維持）
        for pos in (0..e.rq_len).rev() {
            let idx = e.reply_queue[pos];
            if idx >= self.num_tasks {
                continue;
            }

            match self.tasks[idx].blocked_reason {
                Some(BlockedReason::IpcReply { partner: p, ep: pep }) if p == partner && pep == ep => {
                    return e.remove_reply_waiter_at(pos);
                }
                _ => {}
            }
        }

        None
    }

    fn ipc_recv(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        let recv_idx = self.current_task;
        let recv_id = self.tasks[recv_idx].id;
        self.push_event(LogEvent::IpcRecvCalled { task: recv_id, ep });

        // sender が待っていれば deliver（send_queue から）
        let send_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.dequeue_sender()
        };

        if let Some(send_idx) = send_idx_opt {
            let send_id = self.tasks[send_idx].id;

            let msg = match self.tasks[send_idx].pending_send_msg.take() {
                Some(m) => m,
                None => {
                    logging::error("ipc_recv: sender had no pending_send_msg");
                    0
                }
            };

            // sender は reply待ち（Blocked のまま reason を差し替え）
            self.tasks[send_idx].state = TaskState::Blocked;
            self.tasks[send_idx].blocked_reason = Some(BlockedReason::IpcReply {
                partner: recv_id,
                ep,
            });
            self.tasks[send_idx].time_slice_used = 0;
            self.enqueue_wait(send_idx);

            {
                let e = &mut self.endpoints[ep.0];
                e.enqueue_reply_waiter(send_idx);
            }

            self.tasks[recv_idx].last_msg = Some(msg);

            if ep == IPC_DEMO_EP0 && recv_idx == TASK2_INDEX && self.ipc_a_msgs_delivered < 2 {
                self.ipc_a_msgs_delivered += 1;
            }

            self.push_event(LogEvent::IpcDelivered {
                from: send_id,
                to: recv_id,
                ep,
                msg,
            });
            return;
        }

        // sender がいない → recv_waiter に登録して Block
        if self.endpoints[ep.0].recv_waiter.is_some() {
            // このプロトタイプでは recv_waiter は単一前提。
            // 2人目を Block すると回収不能になり得るので「拒否」する。
            logging::error("ipc_recv: recv_waiter already exists; recv rejected (prototype)");
            return;
        }

        self.block_current(BlockedReason::IpcRecv { ep });
        self.endpoints[ep.0].recv_waiter = Some(recv_idx);

        self.push_event(LogEvent::IpcRecvBlocked { task: recv_id, ep });
        self.schedule_next_task();
    }

    fn ipc_send(&mut self, ep: EndpointId, msg: u64) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        let send_idx = self.current_task;
        let send_id = self.tasks[send_idx].id;

        self.push_event(LogEvent::IpcSendCalled { task: send_id, ep, msg });

        // recv_waiter がいれば deliver
        let recv_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.recv_waiter.take()
        };

        if let Some(recv_idx) = recv_idx_opt {
            let recv_id = self.tasks[recv_idx].id;

            self.wake_task_to_ready(recv_idx);
            self.tasks[recv_idx].last_msg = Some(msg);

            // sender を reply待ちで Block → reply_queue へ
            self.block_current(BlockedReason::IpcReply {
                partner: recv_id,
                ep,
            });

            {
                let e = &mut self.endpoints[ep.0];
                e.enqueue_reply_waiter(send_idx);
            }

            if ep == IPC_DEMO_EP0 && recv_idx == TASK2_INDEX && self.ipc_a_msgs_delivered < 2 {
                self.ipc_a_msgs_delivered += 1;
            }

            self.push_event(LogEvent::IpcDelivered {
                from: send_id,
                to: recv_id,
                ep,
                msg,
            });

            self.schedule_next_task();
            return;
        }

        // receiver がいない → sender を send_queue に積んで Block
        self.tasks[send_idx].pending_send_msg = Some(msg);

        self.block_current(BlockedReason::IpcSend { ep });
        {
            let e = &mut self.endpoints[ep.0];
            e.enqueue_sender(send_idx);
        }

        self.push_event(LogEvent::IpcSendBlocked { task: send_id, ep });
        self.schedule_next_task();
    }

    fn ipc_reply(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        let recv_idx = self.current_task;
        let recv_id = self.tasks[recv_idx].id;

        let send_idx = match self.take_reply_waiter_for_partner(ep, recv_id) {
            Some(i) => i,
            None => return,
        };

        let send_id = self.tasks[send_idx].id;

        self.push_event(LogEvent::IpcReplyCalled {
            task: recv_id,
            ep,
            to: send_id,
        });

        self.wake_task_to_ready(send_idx);

        if ep == IPC_DEMO_EP0 && recv_idx == TASK2_INDEX && self.ipc_a_replies_sent < 2 {
            self.ipc_a_replies_sent += 1;
        }

        self.push_event(LogEvent::IpcReplyDelivered {
            from: recv_id,
            to: send_id,
            ep,
        });
    }

    //
    // ──────────────────────────────────────────────
    // IPC demo(A): 2 send → reply_queue=2 → 2 reply
    // ──────────────────────────────────────────────
    //
    fn ipc_demo(&mut self) {
        let ep = IPC_DEMO_EP0;

        // 見える化: IpcDemo を実行するたび、ep0 の状態を表示
        {
            let e = &self.endpoints[ep.0];
            logging::info("ipc_demo: ep0 state");
            logging::info_u64(" recv_waiter_is_some", if e.recv_waiter.is_some() { 1 } else { 0 });

            if let Some(w) = e.recv_waiter {
                logging::info_u64(" recv_waiter_task_index", w as u64);
                if w < self.num_tasks {
                    logging::info_u64(" recv_waiter_task_id", self.tasks[w].id.0);
                }
            }

            logging::info_u64(" send_queue_len", e.sq_len as u64);
            logging::info_u64(" reply_queue_len", e.rq_len as u64);
            logging::info_u64(" msgs_delivered", self.ipc_a_msgs_delivered as u64);
            logging::info_u64(" replies_sent", self.ipc_a_replies_sent as u64);
        }

        let cur = self.current_task;

        // Receiver (Task3)
        if cur == TASK2_INDEX {
            if self.ipc_a_msgs_delivered < 2 {
                self.ipc_recv(ep);
                return;
            }

            if self.ipc_a_replies_sent < 2 {
                self.ipc_reply(ep);
                return;
            }

            // 周回終了 → 状態リセット
            self.ipc_a_msgs_delivered = 0;
            self.ipc_a_replies_sent = 0;
            self.ipc_a_sent_by_task2 = false;
            self.ipc_a_sent_by_task1 = false;
            self.tasks[TASK2_INDEX].last_msg = None;

            logging::info("ipc_demo: cycle reset");
            return;
        }

        // Sender A (Task2): “recv_waiter が立っている(1回目)” のときだけ送る
        if cur == TASK1_INDEX {
            if !self.ipc_a_sent_by_task2 {
                let e = &self.endpoints[ep.0];
                if e.recv_waiter == Some(TASK2_INDEX) && self.ipc_a_msgs_delivered == 0 {
                    self.ipc_a_sent_by_task2 = true;
                    self.ipc_send(ep, 0x1111_0000_0000_0000u64);
                    return;
                }
            }
            return;
        }

        // Sender B (Task1): “recv_waiter が立っている(2回目)” のときだけ送る
        if cur == TASK0_INDEX {
            if !self.ipc_a_sent_by_task1 {
                let e = &self.endpoints[ep.0];
                if e.recv_waiter == Some(TASK2_INDEX) && self.ipc_a_msgs_delivered == 1 {
                    self.ipc_a_sent_by_task1 = true;
                    self.ipc_send(ep, 0x2222_0000_0000_0000u64);
                    return;
                }
            }
            return;
        }
    }

    //
    // tick
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

                self.maybe_wake_one_sleep_task();
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
                self.do_mem_demo();
            }
            KernelAction::IpcDemo => {
                logging::info(" action = IpcDemo");
                self.ipc_demo();
            }
        }

        // ran_idx の runtime は「この tick の開始時点で走っていたタスク」に加算
        self.update_runtime_for(ran_idx);

        // Sleep block は current_task のみ判定
        let still_running = ran_idx == self.current_task && self.tasks[ran_idx].state == TaskState::Running;

        let blocked_by_sleep = if still_running {
            self.maybe_block_task(ran_idx)
        } else {
            false
        };

        if still_running && !blocked_by_sleep {
            self.update_time_slice_for_and_maybe_schedule(ran_idx);
        } else if blocked_by_sleep {
            logging::info(" skip time_slice update due to block in this tick");
        } else {
            logging::info(" skip time_slice update due to task switch in this tick");
        }

        self.activity = next_activity;

        self.debug_check_invariants();
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    //
    // dump_events
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

            if let Some(m) = task.last_msg {
                logging::info("  IPC:");
                logging::info_u64("    last_msg", m);
            }
        }
        logging::info("=== End of AddressSpace Dump ===");

        logging::info("=== Endpoint Dump ===");
        for ep in self.endpoints.iter() {
            logging::info(" ENDPOINT:");
            logging::info_u64("  ep_id", ep.id.0 as u64);

            match ep.recv_waiter {
                Some(tidx) => {
                    logging::info_u64("  recv_waiter_task_index", tidx as u64);
                    if tidx < self.num_tasks {
                        logging::info_u64("  recv_waiter_task_id", self.tasks[tidx].id.0);
                    }
                }
                None => logging::info("  recv_waiter_task_index = None"),
            }

            logging::info_u64("  send_queue_len", ep.sq_len as u64);
            for pos in 0..ep.sq_len {
                let tidx = ep.send_queue[pos];
                logging::info_u64("   send_queue_task_index", tidx as u64);
                if tidx < self.num_tasks {
                    logging::info_u64("   send_queue_task_id", self.tasks[tidx].id.0);
                }
            }

            logging::info_u64("  reply_queue_len", ep.rq_len as u64);
            for pos in 0..ep.rq_len {
                let tidx = ep.reply_queue[pos];
                logging::info_u64("   reply_queue_task_index", tidx as u64);
                if tidx < self.num_tasks {
                    logging::info_u64("   reply_queue_task_id", self.tasks[tidx].id.0);
                }
            }
        }
        logging::info("=== End of Endpoint Dump ===");
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
        LogEvent::FrameAllocated => logging::info("EVENT: FrameAllocated"),
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

        LogEvent::IpcRecvCalled { task, ep } => {
            logging::info("EVENT: IpcRecvCalled");
            logging::info_u64(" task", task.0);
            logging::info_u64(" ep", ep.0 as u64);
        }
        LogEvent::IpcRecvBlocked { task, ep } => {
            logging::info("EVENT: IpcRecvBlocked");
            logging::info_u64(" task", task.0);
            logging::info_u64(" ep", ep.0 as u64);
        }
        LogEvent::IpcSendCalled { task, ep, msg } => {
            logging::info("EVENT: IpcSendCalled");
            logging::info_u64(" task", task.0);
            logging::info_u64(" ep", ep.0 as u64);
            logging::info_u64(" msg", msg);
        }
        LogEvent::IpcSendBlocked { task, ep } => {
            logging::info("EVENT: IpcSendBlocked");
            logging::info_u64(" task", task.0);
            logging::info_u64(" ep", ep.0 as u64);
        }
        LogEvent::IpcDelivered { from, to, ep, msg } => {
            logging::info("EVENT: IpcDelivered");
            logging::info_u64(" from", from.0);
            logging::info_u64(" to", to.0);
            logging::info_u64(" ep", ep.0 as u64);
            logging::info_u64(" msg", msg);
        }
        LogEvent::IpcReplyCalled { task, ep, to } => {
            logging::info("EVENT: IpcReplyCalled");
            logging::info_u64(" task", task.0);
            logging::info_u64(" ep", ep.0 as u64);
            logging::info_u64(" to", to.0);
        }
        LogEvent::IpcReplyDelivered { from, to, ep } => {
            logging::info("EVENT: IpcReplyDelivered");
            logging::info_u64(" from", from.0);
            logging::info_u64(" to", to.0);
            logging::info_u64(" ep", ep.0 as u64);
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
        KernelActivity::MappingDemoPage => (KernelActivity::IpcDemo, KernelAction::MemDemo),
        KernelActivity::IpcDemo => (KernelActivity::Idle, KernelAction::IpcDemo),
    }
}

// ─────────────────────────────────────────────
// 起動エントリ
// ─────────────────────────────────────────────

pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    let code_addr = start as usize as u64;
    let stack_probe: u64 = 0;
    let stack_addr = &stack_probe as *const u64 as u64;
    arch::paging::configure_cr3_switch_safety(code_addr, stack_addr);

    let mut kstate = KernelState::new(boot_info);
    kstate.bootstrap();

    let max_ticks = 80;
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
