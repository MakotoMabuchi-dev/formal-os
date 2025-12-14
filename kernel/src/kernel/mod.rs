// kernel/src/kernel/mod.rs
//
// formal-os: 優先度付きプリエンプティブ＋ReadyQueue＋Blocked状態付きミニカーネル + IPC(Endpoint) + minimal syscall boundary
//
// 目的:
// - タスク状態遷移（Ready/Running/Blocked）とキュー整合性を、ログと invariant で追える形にする。
// - AddressSpace の分離（root(PML4) 違いで同一VAが別PAに解決される）を、translateログで示す。
// - BlockedReason を導入し、IPC（send/recv/reply）の待ちを自然に表現する。
// - Endpoint を追加し、同期 IPC（send/recv/reply）のプロトタイプを動かす。
// - syscall 境界（タスク→カーネルの正式入口）を最小で導入する。
// - low entry / high-alias entry の段取りは entry.rs に分離する。
//
// 設計方針:
// - unsafe は arch 側に局所化し、kernel 側は状態遷移＋抽象イベント中心。
// - WaitQueue は「Blocked 全体」を保持する。
//   * Sleep の wake は “Sleep のみ” を対象にする（IPC の待ちをタイマで勝手に起こさない）。
// - tick 中に schedule が走って current_task が変わるのは自然に起こりうる。
//   * time_slice 更新は「その tick の最後まで同じ task が RUNNING の場合のみ」行う。
// - event_log はリングバッファ化し、直近のログを保持する（観測性改善）。

mod entry;
mod ipc;
mod pagetable_init;
mod syscall;
mod user_program;

pub use entry::start;
pub use syscall::Syscall;

use bootloader::BootInfo;
use x86_64::registers::control::Cr3;

use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};
use crate::mem::paging::{MemAction, PageFlags};
use crate::mem::address_space::{AddressSpace, AddressSpaceError, AddressSpaceKind};
use crate::mem::layout::KERNEL_SPACE_START;

use core::ptr::{read_volatile, write_volatile};

use ipc::Endpoint;

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

// MemDemo: Task別の “offset” 仮想ページ（user は paging 側で USER_SPACE_BASE を足す）
const DEMO_VIRT_PAGE_INDEX_TASK0: u64 = 0x100; // 0x0010_0000
const DEMO_VIRT_PAGE_INDEX_USER:  u64 = 0x110; // 0x0011_0000 (offset)

const IPC_DEMO_EP0: EndpointId = EndpointId(0);

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

    // syscall boundary
    pub pending_syscall: Option<Syscall>,
}

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

    SyscallIssued { task: TaskId },
    SyscallHandled { task: TaskId },

    IpcRecvCalled { task: TaskId, ep: EndpointId },
    IpcRecvBlocked { task: TaskId, ep: EndpointId },
    IpcSendCalled { task: TaskId, ep: EndpointId, msg: u64 },
    IpcSendBlocked { task: TaskId, ep: EndpointId },
    IpcDelivered { from: TaskId, to: TaskId, ep: EndpointId, msg: u64 },
    IpcReplyCalled { task: TaskId, ep: EndpointId, to: TaskId },
    IpcReplyDelivered { from: TaskId, to: TaskId, ep: EndpointId },
}

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

    // event log（リングバッファ）
    event_log: [Option<LogEvent>; EVENT_LOG_CAP],
    event_log_head: usize,
    event_log_len: usize,

    quantum: u64,

    mem_demo_mapped: [bool; MAX_TASKS],
    mem_demo_frame: [Option<PhysFrame>; MAX_TASKS],

    endpoints: [Endpoint; MAX_ENDPOINTS],

    demo_msgs_delivered: u8,
    demo_replies_sent: u8,
    demo_sent_by_task2: bool,
    demo_sent_by_task1: bool,
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
                pending_syscall: None,
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
                pending_syscall: None,
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
                pending_syscall: None,
            },
        ];

        let mut address_spaces = [
            AddressSpace::new_kernel(),
            AddressSpace::new_user(),
            AddressSpace::new_user(),
        ];

        address_spaces[KERNEL_ASID_INDEX].root_page_frame = Some(root_frame_for_task0);

        // User PML4 を 2つ作る
        for as_idx in FIRST_USER_ASID_INDEX..MAX_TASKS {
            let user_root = match pagetable_init::allocate_new_l4_table(&mut phys_mem) {
                Some(f) => f,
                None => {
                    logging::error("no more frames for user pml4");
                    continue;
                }
            };

            address_spaces[as_idx].root_page_frame = Some(user_root);

            logging::info("init_user_pml4_from_current: start");
            logging::info_u64("as_idx", as_idx as u64);
            logging::info_u64("root_page_frame_index", user_root.number);

            arch::paging::init_user_pml4_from_current(user_root);

            logging::info("init_user_pml4_from_current: done");
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
            event_log_head: 0,
            event_log_len: 0,

            quantum: 5,

            mem_demo_mapped: [false; MAX_TASKS],
            mem_demo_frame: [None; MAX_TASKS],

            endpoints: [
                Endpoint::new(EndpointId(0)),
                Endpoint::new(EndpointId(1)),
            ],

            demo_msgs_delivered: 0,
            demo_replies_sent: 0,
            demo_sent_by_task2: false,
            demo_sent_by_task1: false,
        }
    }

    fn push_event(&mut self, ev: LogEvent) {
        if EVENT_LOG_CAP == 0 {
            return;
        }

        let pos = (self.event_log_head + self.event_log_len) % EVENT_LOG_CAP;
        self.event_log[pos] = Some(ev);

        if self.event_log_len < EVENT_LOG_CAP {
            self.event_log_len += 1;
        } else {
            self.event_log_head = (self.event_log_head + 1) % EVENT_LOG_CAP;
        }
    }

    fn debug_check_invariants(&self) {
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

        for (idx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            match t.state {
                TaskState::Blocked => {
                    if t.blocked_reason.is_none() {
                        logging::error("INVARIANT VIOLATION: BLOCKED task has no blocked_reason");
                        logging::info_u64("task_index", idx as u64);
                    }
                }
                _ => {
                    if t.blocked_reason.is_some() {
                        logging::error("INVARIANT VIOLATION: non-BLOCKED task has blocked_reason");
                        logging::info_u64("task_index", idx as u64);
                    }
                }
            }
        }

        if self.current_task >= self.num_tasks {
            logging::error("INVARIANT VIOLATION: current_task out of range");
        } else if self.tasks[self.current_task].state != TaskState::Running {
            logging::error("INVARIANT VIOLATION: current_task is not RUNNING");
        }

        for as_idx in FIRST_USER_ASID_INDEX..self.num_tasks {
            let aspace = &self.address_spaces[as_idx];
            if aspace.kind != AddressSpaceKind::User {
                continue;
            }
            aspace.for_each_mapping(|m| {
                let offset = m.page.number * PAGE_SIZE;

                if offset >= arch::paging::USER_SPACE_SIZE {
                    logging::error("INVARIANT VIOLATION: user mapping offset out of user slot range");
                    logging::info_u64("as_idx", as_idx as u64);
                    logging::info_u64("offset", offset);
                }

                let _ = KERNEL_SPACE_START;
            });
        }

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

    fn schedule_next_task(&mut self) {
        let prev_idx = self.current_task;
        let prev_id = self.tasks[prev_idx].id;

        // いまの CR3 が user の可能性があるので、まず「現行タスクの種別」で VGA を安全側に合わせる
        // これにより、この関数内でのログが VGA を触って落ちる事故を防ぐ。
        {
            let cur_as_idx = self.tasks[self.current_task].address_space_id.0;
            match self.address_spaces[cur_as_idx].kind {
                AddressSpaceKind::Kernel => logging::set_vga_enabled(true),
                AddressSpaceKind::User => logging::set_vga_enabled(false),
            }
        }

        if self.tasks[prev_idx].state == TaskState::Running {
            self.tasks[prev_idx].state = TaskState::Ready;
            self.tasks[prev_idx].time_slice_used = 0;
            self.push_event(LogEvent::TaskStateChanged(prev_id, TaskState::Ready));
            self.enqueue_ready(prev_idx);
        }

        let next_idx = match self.dequeue_ready_highest_priority() {
            Some(i) => i,
            None => {
                // ここも「現行の VGA 設定」に従う（user中なら serial のみ）
                logging::info("no ready tasks; scheduler idle");
                return;
            }
        };

        let next_id = self.tasks[next_idx].id;
        let as_idx = self.tasks[next_idx].address_space_id.0;

        self.tasks[next_idx].state = TaskState::Running;
        self.tasks[next_idx].time_slice_used = 0;
        self.tasks[next_idx].blocked_reason = None;
        self.current_task = next_idx;

        let next_kind = self.address_spaces[as_idx].kind;
        let root = self.address_spaces[as_idx].root_page_frame;

        // ★重要：CR3 切替の前後で VGA を正しい状態にする
        //
        // - 次が User: 先に VGA OFF → CR3=user → 以降のログは serial のみ
        // - 次が Kernel: まず CR3=kernel に戻す → その後 VGA ON → ログOK
        match next_kind {
            AddressSpaceKind::User => {
                logging::set_vga_enabled(false);
                arch::paging::switch_address_space(root);

                // user CR3 のままでも安全（serial のみ）
                logging::info("switched to task");
                logging::info_u64("task_id", next_id.0);
            }
            AddressSpaceKind::Kernel => {
                // いま user CR3 の可能性があるので、先に CR3 を kernel に戻す
                arch::paging::switch_address_space(root);

                // kernel に戻ってから VGA ON
                logging::set_vga_enabled(true);

                logging::info("switched to task");
                logging::info_u64("task_id", next_id.0);
            }
        }

        self.push_event(LogEvent::TaskSwitched(next_id));
        self.push_event(LogEvent::TaskStateChanged(next_id, TaskState::Running));
    }

    fn update_runtime_for(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_runtime_for: ran_idx out of range");
            return;
        }
        let id = self.tasks[ran_idx].id;
        self.tasks[ran_idx].runtime_ticks += 1;
        logging::info_u64("runtime_ticks", self.tasks[ran_idx].runtime_ticks);
        self.push_event(LogEvent::RuntimeUpdated(id, self.tasks[ran_idx].runtime_ticks));
    }

    fn block_current(&mut self, reason: BlockedReason) {
        let idx = self.current_task;
        let id = self.tasks[idx].id;

        self.tasks[idx].state = TaskState::Blocked;
        self.tasks[idx].blocked_reason = Some(reason);
        self.tasks[idx].time_slice_used = 0;

        self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));
        self.enqueue_wait(idx);
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
            logging::info("blocking current task (fake I/O wait)");
            self.block_current(BlockedReason::Sleep);
            self.schedule_next_task();
            return true;
        }

        false
    }

    fn update_time_slice_for_and_maybe_schedule(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_time_slice_for_and_maybe_schedule: ran_idx out of range");
            return;
        }
        if ran_idx != self.current_task {
            logging::info("skip time_slice update due to task switch in this tick");
            return;
        }
        if self.tasks[ran_idx].state != TaskState::Running {
            logging::info("skip time_slice update (task not RUNNING)");
            return;
        }

        let id = self.tasks[ran_idx].id;
        self.tasks[ran_idx].time_slice_used += 1;
        logging::info_u64("time_slice_used", self.tasks[ran_idx].time_slice_used);

        if self.tasks[ran_idx].time_slice_used >= self.quantum {
            logging::info("quantum expired; scheduling next task");
            self.push_event(LogEvent::QuantumExpired(id, self.tasks[ran_idx].time_slice_used));
            self.schedule_next_task();
        }
    }

    fn maybe_wake_one_sleep_task(&mut self) {
        for pos in 0..self.wq_len {
            let idx = self.wait_queue[pos];
            if idx >= self.num_tasks {
                continue;
            }
            if self.tasks[idx].blocked_reason == Some(BlockedReason::Sleep) {
                logging::info("waking 1 blocked task (Sleep only)");
                self.wake_task_to_ready(idx);
                return;
            }
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
        #[cfg(feature = "evil_double_map")]
        {
            self.do_mem_demo_evil_double_map();
            return;
        }

        #[cfg(feature = "evil_unmap_not_mapped")]
        {
            self.do_mem_demo_evil_unmap_not_mapped();
            return;
        }

        self.do_mem_demo_normal();
    }

    fn do_mem_demo_normal(&mut self) {
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
            logging::info("mem_demo: issuing Map (for current task)");

            let frame = match self.get_or_alloc_demo_frame(task_idx) {
                Some(f) => f,
                None => {
                    logging::error("mem_demo: no more usable frames");
                    self.should_halt = true;
                    return;
                }
            };

            MemAction::Map { page, frame, flags }
        } else {
            logging::info("mem_demo: issuing Unmap (for current task)");
            MemAction::Unmap { page }
        };

        let as_idx = task.address_space_id.0;
        let aspace = &mut self.address_spaces[as_idx];

        // 1) 論理側（失敗は fail-stop）
        match aspace.apply(mem_action) {
            Ok(()) => {
                logging::info("address_space.apply: OK");
            }
            Err(e) => {
                logging::error("address_space.apply: ERROR");
                match e {
                    AddressSpaceError::AlreadyMapped => logging::info("reason = AlreadyMapped"),
                    AddressSpaceError::NotMapped => logging::info("reason = NotMapped"),
                    AddressSpaceError::CapacityExceeded => logging::info("reason = CapacityExceeded"),
                }
                panic!("address_space.apply failed; abort (fail-stop)");
            }
        }

        // 2) arch 側（unsafe は arch に閉じ込める）
        if task_idx == TASK0_INDEX {
            logging::info("mem_demo: applying arch paging (Task0 / current CR3)");
            match unsafe { arch::paging::apply_mem_action(mem_action, &mut self.phys_mem) } {
                Ok(()) => {}
                Err(_e) => {
                    logging::error("arch::paging::apply_mem_action failed; abort (fail-stop)");
                    panic!("arch apply_mem_action failed");
                }
            }
        } else {
            let root = match aspace.root_page_frame {
                Some(r) => r,
                None => {
                    logging::error("mem_demo: user root_page_frame is None (unexpected)");
                    panic!("user root_page_frame is None");
                }
            };

            logging::info("mem_demo: applying arch paging (User root / no CR3 switch)");
            match unsafe { arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem) } {
                Ok(()) => {}
                Err(_e) => {
                    logging::error("arch::paging::apply_mem_action_in_root failed; abort (fail-stop)");
                    panic!("arch apply_mem_action_in_root failed");
                }
            }

            // translate で観測（既存）
            let virt_addr_u64 = arch::paging::USER_SPACE_BASE + page.start_address().0;
            arch::paging::debug_translate_in_root(root, virt_addr_u64);

            // ★★★ ここが今回の追加：user root に CR3 を入れて、USER VA へ実 read/write ★★★
            // 条件: Map のときだけ実アクセス（Unmap 後に触ると #PF で落ちる）
            if let MemAction::Map { .. } = mem_action {
                logging::info("mem_demo: USER RW TEST (CR3=user root, access user VA)");

                // 念のため「今この瞬間」も user root を入れる（既に入ってる場合でもOK）
                arch::paging::switch_address_space(Some(root));

                // user slot の VA を作る（paging.rs 側のポリシーと一致）
                let user_virt = virt_addr_u64 as *mut u64;

                // タスクごとに変化するテスト値（ログで追いやすくする）
                let test_value: u64 = 0xC0DE_0000_0000_0000u64
                    ^ ((task_id.0 & 0xFFFF) << 16)
                    ^ (self.tick_count & 0xFFFF);

                unsafe {
                    logging::info("user_mem_test: writing test_value");
                    write_volatile(user_virt, test_value);

                    let read_back = read_volatile(user_virt);

                    logging::info("user_mem_test: read_back");
                    logging::info_u64("", read_back);

                    if read_back == test_value {
                        logging::info("user_mem_test: OK (value matched)");
                    } else {
                        logging::error("user_mem_test: MISMATCH!");
                        logging::info_u64("expected", test_value);
                        logging::info_u64("got", read_back);
                        panic!("user_mem_test mismatch (fail-stop)");
                    }
                }

                // NOTE:
                // - ここで kernel root に戻さない（このタスクが RUNNING の間は user root が自然）
                // - 次のスケジュールで switch_address_space が必ず走るので整合は保たれる
            }
        }

        // 3) 成功扱いになった後でのみ状態を進める
        self.mem_demo_mapped[task_idx] = !self.mem_demo_mapped[task_idx];

        self.push_event(LogEvent::MemActionApplied {
            task: task_id,
            address_space: task.address_space_id,
            action: mem_action,
        });
    }

    #[cfg(feature = "evil_double_map")]
    fn do_mem_demo_evil_double_map(&mut self) {
        let task_idx = self.current_task;
        let task = self.tasks[task_idx];

        let page = self.demo_page_for_task(task_idx);

        let flags = if task_idx == TASK0_INDEX {
            PageFlags::PRESENT | PageFlags::WRITABLE
        } else {
            PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER
        };

        logging::info("mem_demo: issuing Map (evil double-map test)");

        let frame = match self.get_or_alloc_demo_frame(task_idx) {
            Some(f) => f,
            None => {
                logging::error("mem_demo: no more usable frames");
                self.should_halt = true;
                return;
            }
        };

        let mem_action = MemAction::Map { page, frame, flags };

        let as_idx = task.address_space_id.0;
        let aspace = &mut self.address_spaces[as_idx];

        match aspace.apply(mem_action) {
            Ok(()) => {
                logging::info("address_space.apply: OK");
            }
            Err(e) => {
                logging::error("address_space.apply: ERROR");
                match e {
                    AddressSpaceError::AlreadyMapped => logging::info("reason = AlreadyMapped"),
                    AddressSpaceError::NotMapped => logging::info("reason = NotMapped"),
                    AddressSpaceError::CapacityExceeded => logging::info("reason = CapacityExceeded"),
                }
                panic!("AddressSpace.apply failed in evil double-map test");
            }
        }

        // 1回目だけここまで来る（2回目は上で panic する）
        if task_idx == TASK0_INDEX {
            logging::info("mem_demo: applying arch paging (Task0 / current CR3)");
            match unsafe { arch::paging::apply_mem_action(mem_action, &mut self.phys_mem) } {
                Ok(()) => {}
                Err(_e) => {
                    logging::error("arch::paging::apply_mem_action failed; abort (fail-stop)");
                    panic!("arch apply_mem_action failed");
                }
            }
        } else {
            let root = aspace.root_page_frame.expect("user root_page_frame must exist");
            logging::info("mem_demo: applying arch paging (User root / no CR3 switch)");
            match unsafe { arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem) } {
                Ok(()) => {}
                Err(_e) => {
                    logging::error("arch::paging::apply_mem_action_in_root failed; abort (fail-stop)");
                    panic!("arch apply_mem_action_in_root failed");
                }
            }
        }

        let _ = task;
    }

    #[cfg(feature = "evil_unmap_not_mapped")]
    fn do_mem_demo_evil_unmap_not_mapped(&mut self) {
        let task_idx = self.current_task;
        let task = self.tasks[task_idx];
        let _task_id = task.id;

        let page = self.demo_page_for_task(task_idx);
        let as_idx = task.address_space_id.0;
        let aspace = &mut self.address_spaces[as_idx];

        logging::info("mem_demo: issuing Unmap (evil unmap-not-mapped test)");

        let mem_action = MemAction::Unmap { page };

        // 1) 論理層で必ず NotMapped になるはず
        match aspace.apply(mem_action) {
            Ok(()) => {
                logging::error("UNEXPECTED: Unmap succeeded on non-mapped page");
                panic!("evil_unmap_not_mapped violated invariant");
            }
            Err(AddressSpaceError::NotMapped) => {
                logging::info("address_space.apply: NotMapped (expected)");
                panic!("evil_unmap_not_mapped: correct fail-stop");
            }
            Err(e) => {
                logging::error("address_space.apply: unexpected error");
                match e {
                    AddressSpaceError::AlreadyMapped => logging::info("reason = AlreadyMapped"),
                    AddressSpaceError::CapacityExceeded => logging::info("reason = CapacityExceeded"),
                    _ => {}
                }
                panic!("evil_unmap_not_mapped: unexpected error");
            }
        }
    }

    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        logging::info("KernelState::tick()");
        logging::info_u64("tick_count", self.tick_count);

        self.push_event(LogEvent::TickStarted(self.tick_count));

        let running = self.tasks[self.current_task].id;
        logging::info_u64("running_task", running.0);

        let ran_idx = self.current_task;

        let (next_activity, action) = next_activity_and_action(self.activity);

        match action {
            KernelAction::None => {
                logging::info("action = None");
            }
            KernelAction::UpdateTimer => {
                logging::info("action = UpdateTimer");
                self.time_ticks += 1;
                logging::info_u64("time_ticks", self.time_ticks);
                self.push_event(LogEvent::TimerUpdated(self.time_ticks));
                self.maybe_wake_one_sleep_task();
            }
            KernelAction::AllocateFrame => {
                logging::info("action = AllocateFrame");
                if let Some(_) = self.phys_mem.allocate_frame() {
                    logging::info("allocated usable frame (tick)");
                    self.push_event(LogEvent::FrameAllocated);
                } else {
                    logging::error("no more usable frames; halting later");
                    self.should_halt = true;
                }
            }
            KernelAction::MemDemo => {
                logging::info("action = MemDemo");
                self.do_mem_demo();
            }
        }

        self.user_step_issue_syscall(ran_idx);

        if ran_idx == self.current_task {
            self.handle_pending_syscall_if_any();
        }

        self.update_runtime_for(ran_idx);

        let still_running = ran_idx == self.current_task && self.tasks[ran_idx].state == TaskState::Running;

        let blocked_by_sleep = if still_running {
            self.maybe_block_task(ran_idx)
        } else {
            false
        };

        if still_running && !blocked_by_sleep {
            self.update_time_slice_for_and_maybe_schedule(ran_idx);
        } else if blocked_by_sleep {
            logging::info("skip time_slice update due to block in this tick");
        } else {
            logging::info("skip time_slice update due to task switch in this tick");
        }

        self.activity = next_activity;

        self.debug_check_invariants();
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }

    pub fn dump_events(&self) {
        logging::info("=== KernelState Event Log Dump ===");
        for i in 0..self.event_log_len {
            let idx = (self.event_log_head + i) % EVENT_LOG_CAP;
            if let Some(ev) = self.event_log[idx] {
                log_event_to_vga(ev);
            }
        }
        logging::info("=== End of Event Log ===");

        logging::info("=== AddressSpace Dump (per task) ===");
        for i in 0..self.num_tasks {
            let task = self.tasks[i];

            logging::info("Task AddressSpace:");
            logging::info_u64("task_index", i as u64);
            logging::info_u64("task_id", task.id.0);

            let as_idx = task.address_space_id.0;
            let aspace = &self.address_spaces[as_idx];

            match aspace.kind {
                AddressSpaceKind::Kernel => logging::info("kind = Kernel"),
                AddressSpaceKind::User => logging::info("kind = User"),
            }

            match aspace.root_page_frame {
                Some(root) => logging::info_u64("root_page_frame_index", root.number),
                None => logging::info("root_page_frame_index = None"),
            }

            logging::info_u64("address_space_id", as_idx as u64);

            let count = aspace.mapping_count();
            logging::info_u64("mapping_count", count as u64);

            aspace.for_each_mapping(|m| {
                logging::info("MAPPING:");
                logging::info_u64("virt_page_index", m.page.number);
                logging::info_u64("phys_frame_index", m.frame.number);
                logging::info_u64("flags_bits", m.flags.bits());
            });

            if let Some(m) = task.last_msg {
                logging::info("IPC:");
                logging::info_u64("last_msg", m);
            }
        }
        logging::info("=== End of AddressSpace Dump ===");

        logging::info("=== Endpoint Dump ===");
        for ep in self.endpoints.iter() {
            logging::info("ENDPOINT:");
            logging::info_u64("ep_id", ep.id.0 as u64);

            match ep.recv_waiter {
                Some(tidx) => {
                    logging::info_u64("recv_waiter_task_index", tidx as u64);
                    if tidx < self.num_tasks {
                        logging::info_u64("recv_waiter_task_id", self.tasks[tidx].id.0);
                    }
                }
                None => logging::info("recv_waiter_task_index = None"),
            }

            logging::info_u64("send_queue_len", ep.sq_len as u64);
            for pos in 0..ep.sq_len {
                let tidx = ep.send_queue[pos];
                logging::info_u64("send_queue_task_index", tidx as u64);
                if tidx < self.num_tasks {
                    logging::info_u64("send_queue_task_id", self.tasks[tidx].id.0);
                }
            }

            logging::info_u64("reply_queue_len", ep.rq_len as u64);
            for pos in 0..ep.rq_len {
                let tidx = ep.reply_queue[pos];
                logging::info_u64("reply_queue_task_index", tidx as u64);
                if tidx < self.num_tasks {
                    logging::info_u64("reply_queue_task_id", self.tasks[tidx].id.0);
                }
            }
        }
        logging::info("=== End of Endpoint Dump ===");
    }

    // --- ここから下は、あなたの元コードにある他メソッド（syscall/IPC/user_program等） ---
    // NOTE: ここはあなたの過去チャットの全体コードが必要ですが、今回の修正は paging Result 反映だけなので、
    //       既にあなたの手元にある同名実装をそのまま残してください。
    //
    // ただし「全コード表示」要求に合わせるため、本来はこの下も全て貼る必要があります。
    // いま貼ってもらっていない範囲（user_step_issue_syscall / handle_pending_syscall_if_any 等）は
    // こちらで“推測して再構成”すると危険なので、ここでは改変せず、あなたの手元の既存実装を維持してください。
    //
    // --- ここまで ---
}

fn log_event_to_vga(ev: LogEvent) {
    match ev {
        LogEvent::TickStarted(n) => {
            logging::info("EVENT: TickStarted");
            logging::info_u64("tick", n);
        }
        LogEvent::TimerUpdated(n) => {
            logging::info("EVENT: TimerUpdated");
            logging::info_u64("time", n);
        }
        LogEvent::FrameAllocated => logging::info("EVENT: FrameAllocated"),
        LogEvent::TaskSwitched(tid) => {
            logging::info("EVENT: TaskSwitched");
            logging::info_u64("task", tid.0);
        }
        LogEvent::TaskStateChanged(tid, state) => {
            logging::info("EVENT: TaskStateChanged");
            logging::info_u64("task", tid.0);
            match state {
                TaskState::Ready => logging::info("to READY"),
                TaskState::Running => logging::info("to RUNNING"),
                TaskState::Blocked => logging::info("to BLOCKED"),
            }
        }
        LogEvent::ReadyQueued(tid) => {
            logging::info("EVENT: ReadyQueued");
            logging::info_u64("task", tid.0);
        }
        LogEvent::ReadyDequeued(tid) => {
            logging::info("EVENT: ReadyDequeued");
            logging::info_u64("task", tid.0);
        }
        LogEvent::WaitQueued(tid) => {
            logging::info("EVENT: WaitQueued");
            logging::info_u64("task", tid.0);
        }
        LogEvent::WaitDequeued(tid) => {
            logging::info("EVENT: WaitDequeued");
            logging::info_u64("task", tid.0);
        }
        LogEvent::RuntimeUpdated(tid, rt) => {
            logging::info("EVENT: RuntimeUpdated");
            logging::info_u64("task", tid.0);
            logging::info_u64("runtime", rt);
        }
        LogEvent::QuantumExpired(tid, used) => {
            logging::info("EVENT: QuantumExpired");
            logging::info_u64("task", tid.0);
            logging::info_u64("used_ticks", used);
        }
        LogEvent::MemActionApplied { task, address_space, action } => {
            logging::info("EVENT: MemActionApplied");
            logging::info_u64("task", task.0);
            logging::info_u64("address_space_id", address_space.0 as u64);

            match action {
                MemAction::Map { page, frame, flags } => {
                    logging::info("mem_action = Map");
                    logging::info_u64("virt_page_index", page.number);
                    logging::info_u64("phys_frame_index", frame.number);
                    logging::info_u64("flags_bits", flags.bits());
                }
                MemAction::Unmap { page } => {
                    logging::info("mem_action = Unmap");
                    logging::info_u64("virt_page_index", page.number);
                }
            }
        }
        LogEvent::SyscallIssued { task } => {
            logging::info("EVENT: SyscallIssued");
            logging::info_u64("task", task.0);
        }
        LogEvent::SyscallHandled { task } => {
            logging::info("EVENT: SyscallHandled");
            logging::info_u64("task", task.0);
        }
        LogEvent::IpcRecvCalled { task, ep } => {
            logging::info("EVENT: IpcRecvCalled");
            logging::info_u64("task", task.0);
            logging::info_u64("ep", ep.0 as u64);
        }
        LogEvent::IpcRecvBlocked { task, ep } => {
            logging::info("EVENT: IpcRecvBlocked");
            logging::info_u64("task", task.0);
            logging::info_u64("ep", ep.0 as u64);
        }
        LogEvent::IpcSendCalled { task, ep, msg } => {
            logging::info("EVENT: IpcSendCalled");
            logging::info_u64("task", task.0);
            logging::info_u64("ep", ep.0 as u64);
            logging::info_u64("msg", msg);
        }
        LogEvent::IpcSendBlocked { task, ep } => {
            logging::info("EVENT: IpcSendBlocked");
            logging::info_u64("task", task.0);
            logging::info_u64("ep", ep.0 as u64);
        }
        LogEvent::IpcDelivered { from, to, ep, msg } => {
            logging::info("EVENT: IpcDelivered");
            logging::info_u64("from", from.0);
            logging::info_u64("to", to.0);
            logging::info_u64("ep", ep.0 as u64);
            logging::info_u64("msg", msg);
        }
        LogEvent::IpcReplyCalled { task, ep, to } => {
            logging::info("EVENT: IpcReplyCalled");
            logging::info_u64("task", task.0);
            logging::info_u64("ep", ep.0 as u64);
            logging::info_u64("to", to.0);
        }
        LogEvent::IpcReplyDelivered { from, to, ep } => {
            logging::info("EVENT: IpcReplyDelivered");
            logging::info_u64("from", from.0);
            logging::info_u64("to", to.0);
            logging::info_u64("ep", ep.0 as u64);
        }
    }
}

fn next_activity_and_action(current: KernelActivity) -> (KernelActivity, KernelAction) {
    match current {
        KernelActivity::Idle => (KernelActivity::UpdatingTimer, KernelAction::None),
        KernelActivity::UpdatingTimer => (KernelActivity::AllocatingFrame, KernelAction::UpdateTimer),
        KernelActivity::AllocatingFrame => (KernelActivity::MappingDemoPage, KernelAction::AllocateFrame),
        KernelActivity::MappingDemoPage => (KernelActivity::Idle, KernelAction::MemDemo),
    }
}
