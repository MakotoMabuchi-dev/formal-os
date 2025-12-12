// src/kernel/mod.rs
//
// formal-os: 優先度付きプリエンプティブ＋ReadyQueue＋Blocked状態付きミニカーネル + IPC(Endpoint)
//
// 目的:
// - タスク状態遷移（Ready/Running/Blocked）とキュー整合性を、ログと invariant で追える形にする。
// - AddressSpace の分離（root(PML4) 違いで同一VAが別PAに解決される）を、translateログで示す。
// - BlockedReason を導入し、将来の IPC（send/recv/reply）待ちを自然に表現できる拡張点を作る。
// - 次の段階として、Endpoint を追加し、IPC の最小プロトタイプ（同期send/recv/reply）を動かす。
//
// 設計方針:
// - unsafe は arch 側に局所化し、kernel 側は状態遷移＋抽象イベント中心。
// - WaitQueue は「Blocked 全体」を保持する。
//   * Sleep の wake は “Sleep のみ” を対象にする（IPC の待ちをタイマで勝手に起こさない）。
// - tick 中に schedule が走って current_task が変わるのは自然に起こりうるため、
//   ran_idx と current_task のズレは許容し、ブロック判定は current_task に対してのみ行う。

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

// プロトタイプ用の固定 ID（設計仕様をコードで表現するための定数）
const KERNEL_ASID_INDEX: usize = 0;          // AddressSpaces[0] は Kernel
const FIRST_USER_ASID_INDEX: usize = 1;      // AddressSpaces[1..] は User

const TASK0_INDEX: usize = 0;
const TASK1_INDEX: usize = 1;
const TASK2_INDEX: usize = 2;

const TASK0_ID: TaskId = TaskId(1);
const TASK1_ID: TaskId = TaskId(2);
const TASK2_ID: TaskId = TaskId(3);

// デモ用：Task別の仮想ページ
// - Task0: 0x0010_0000 (0x100)
// - User タスクは「同一virt」を使う（分離の証拠を示す）
const DEMO_VIRT_PAGE_INDEX_TASK0: u64 = 0x100; // 0x0010_0000
const DEMO_VIRT_PAGE_INDEX_USER:  u64 = 0x110; // 0x0011_0000  ← Task1/Task2 共通

const IPC_DEMO_EP0: EndpointId = EndpointId(0);

//
// ──────────────────────────────────────────────
// TaskId / AddressSpaceId / TaskState / BlockedReason / Task
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

/// IPC 用エンドポイントID（最小デモは固定個数）
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct EndpointId(pub usize);

/// Blocked 状態の理由（将来 IPC 待ちなどを追加するための拡張点）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BlockedReason {
    /// 現段階では「擬似 I/O wait」相当（将来は sleep に発展させる/タイムアウトを持たせる）
    Sleep,

    /// IPC: recv 待ち
    IpcRecv { ep: EndpointId },

    /// IPC: send 待ち（受信者不在）
    IpcSend { ep: EndpointId },

    /// IPC: reply 待ち（同期的 IPC を表現）
    IpcReply { partner: TaskId, ep: EndpointId },
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

    /// Blocked の理由（Blocked のときのみ Some、Blocked 以外では None）
    pub blocked_reason: Option<BlockedReason>,

    /// IPC デモ用：受信した値（本来は inbox などに積む）
    pub last_msg: Option<u64>,

    /// IPC デモ用：送信待ちで保持する payload
    pub pending_send_msg: Option<u64>,
}

//
// ──────────────────────────────────────────────
// Endpoint（最小IPC）
// ──────────────────────────────────────────────
//

#[derive(Clone, Copy)]
pub struct Endpoint {
    pub id: EndpointId,

    /// recv で待っているタスク index（最大1）
    pub recv_waiter: Option<usize>,

    /// send 待ちタスク index のキュー（順序は抽象化してOK）
    pub send_queue: [usize; MAX_TASKS],
    pub sq_len: usize,

    /// reply 待ちの送信者（最小デモ：1件）
    pub reply_waiter: Option<usize>,
}

impl Endpoint {
    pub const fn new(id: EndpointId) -> Self {
        Endpoint {
            id,
            recv_waiter: None,
            send_queue: [0; MAX_TASKS],
            sq_len: 0,
            reply_waiter: None,
        }
    }

    fn enqueue_sender(&mut self, idx: usize) {
        if self.sq_len >= MAX_TASKS {
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

    // ── IPC ───────────────────────
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

    // IPC
    endpoints: [Endpoint; MAX_ENDPOINTS],

    // IPC デモ進行
    ipc_demo_recv_done: bool,
    ipc_demo_send_done: bool,
    ipc_demo_reply_done: bool,
}

impl KernelState {
    //
    // new()
    //
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let mut phys_mem = PhysicalMemoryManager::new(boot_info);

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

        // User AddressSpace 用の root_page_frame を割り当て、Kernel の上位半分をコピーして初期化する
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

            // User PML4 を “Kernel 上位半分コピー” で初期化（CR3切替で落ちないように）
            arch::paging::init_user_pml4_from_current(user_root);
        }

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

            endpoints: [
                Endpoint::new(EndpointId(0)),
                Endpoint::new(EndpointId(1)),
            ],

            ipc_demo_recv_done: false,
            ipc_demo_send_done: false,
            ipc_demo_reply_done: false,
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

            // この段階では User も root_page_frame を持つ（CR3切替の準備）
            if aspace.root_page_frame.is_none() {
                logging::error("INVARIANT VIOLATION: user address space has no root_page_frame");
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
        // 2. スケジューラ / キュー構造の invariant
        // ─────────────────────────────

        // BlockedReason の整合
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

        // current_task は必ず Running
        if self.current_task >= self.num_tasks {
            logging::error("INVARIANT VIOLATION: current_task index out of range");
        } else if self.tasks[self.current_task].state != TaskState::Running {
            logging::error("INVARIANT VIOLATION: current_task is not RUNNING");
            logging::info_u64(" current_task_index", self.current_task as u64);
        }

        // Running は current_task のみ
        for (idx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            if idx == self.current_task {
                continue;
            }
            if t.state == TaskState::Running {
                logging::error("INVARIANT VIOLATION: multiple RUNNING tasks");
                logging::info_u64(" extra_running_task_index", idx as u64);
            }
        }

        let mut in_ready = [false; MAX_TASKS];
        let mut in_wait = [false; MAX_TASKS];

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

        for idx in 0..self.num_tasks {
            if in_ready[idx] && in_wait[idx] {
                logging::error("INVARIANT VIOLATION: task is in both ready_queue and wait_queue");
                logging::info_u64(" task_index", idx as u64);
            }
        }

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

        // ─────────────────────────────
        // 4. IPC Endpoint invariant（最小）
        // ─────────────────────────────
        for e in self.endpoints.iter() {
            // recv_waiter 整合
            if let Some(idx) = e.recv_waiter {
                if idx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.recv_waiter out of range");
                    logging::info_u64(" ep", e.id.0 as u64);
                    logging::info_u64(" idx", idx as u64);
                } else {
                    let t = &self.tasks[idx];
                    if t.state != TaskState::Blocked {
                        logging::error("INVARIANT VIOLATION: recv_waiter is not BLOCKED");
                        logging::info_u64(" ep", e.id.0 as u64);
                        logging::info_u64(" idx", idx as u64);
                    }
                    match t.blocked_reason {
                        Some(BlockedReason::IpcRecv { ep }) if ep == e.id => {}
                        _ => {
                            logging::error("INVARIANT VIOLATION: recv_waiter blocked_reason mismatch");
                            logging::info_u64(" ep", e.id.0 as u64);
                            logging::info_u64(" idx", idx as u64);
                        }
                    }
                }
            }

            // send_queue 整合
            for pos in 0..e.sq_len {
                let idx = e.send_queue[pos];
                if idx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.send_queue idx out of range");
                    logging::info_u64(" ep", e.id.0 as u64);
                    logging::info_u64(" idx", idx as u64);
                    continue;
                }
                let t = &self.tasks[idx];
                if t.state != TaskState::Blocked {
                    logging::error("INVARIANT VIOLATION: sender in send_queue is not BLOCKED");
                    logging::info_u64(" ep", e.id.0 as u64);
                    logging::info_u64(" idx", idx as u64);
                }
                match t.blocked_reason {
                    Some(BlockedReason::IpcSend { ep }) if ep == e.id => {}
                    _ => {
                        logging::error("INVARIANT VIOLATION: sender blocked_reason mismatch");
                        logging::info_u64(" ep", e.id.0 as u64);
                        logging::info_u64(" idx", idx as u64);
                    }
                }
            }

            // reply_waiter 整合（最小デモ：1件だけ追う）
            if let Some(idx) = e.reply_waiter {
                if idx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.reply_waiter out of range");
                    logging::info_u64(" ep", e.id.0 as u64);
                    logging::info_u64(" idx", idx as u64);
                } else {
                    let t = &self.tasks[idx];
                    if t.state != TaskState::Blocked {
                        logging::error("INVARIANT VIOLATION: reply_waiter is not BLOCKED");
                        logging::info_u64(" ep", e.id.0 as u64);
                        logging::info_u64(" idx", idx as u64);
                    }
                    match t.blocked_reason {
                        Some(BlockedReason::IpcReply { ep, .. }) if ep == e.id => {}
                        _ => {
                            logging::error("INVARIANT VIOLATION: reply_waiter blocked_reason mismatch");
                            logging::info_u64(" ep", e.id.0 as u64);
                            logging::info_u64(" idx", idx as u64);
                        }
                    }
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

    fn remove_from_wait_queue(&mut self, idx: usize) -> bool {
        for pos in 0..self.wq_len {
            if self.wait_queue[pos] == idx {
                let last = self.wq_len - 1;
                self.wait_queue[pos] = self.wait_queue[last];
                self.wq_len -= 1;
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
        if self.tasks[idx].blocked_reason.is_none() {
            // BlockedReason を必須にする
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

        // tick 中に schedule が走ると ran_idx と current_task はズレうる。
        // ここは “current_task に対してのみ” 判定する。
        if ran_idx != self.current_task {
            return false;
        }

        if self.tick_count != 0
            && self.tick_count % 7 == 0
            && self.tasks[ran_idx].id.0 == 2
        {
            let id = self.tasks[ran_idx].id;

            logging::info(" blocking current task (fake I/O wait)");
            self.tasks[ran_idx].state = TaskState::Blocked;
            self.tasks[ran_idx].blocked_reason = Some(BlockedReason::Sleep);
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
    // 疑似 Wake（Sleep だけ起こす）
    // - IPC の Blocked をタイマで勝手に起こすと、Endpoint 側の待ち状態と矛盾する。
    //
    fn maybe_wake_one_sleep_task(&mut self) {
        for pos in 0..self.wq_len {
            let idx = self.wait_queue[pos];
            if idx >= self.num_tasks {
                continue;
            }

            if self.tasks[idx].blocked_reason == Some(BlockedReason::Sleep) {
                // swap-remove
                let last = self.wq_len - 1;
                self.wait_queue[pos] = self.wait_queue[last];
                self.wq_len -= 1;

                let id = self.tasks[idx].id;

                logging::info(" waking 1 blocked task (Sleep only)");
                self.tasks[idx].state = TaskState::Ready;
                self.tasks[idx].blocked_reason = None;

                self.push_event(LogEvent::TaskStateChanged(id, TaskState::Ready));
                self.enqueue_ready(idx);
                return;
            }
        }
    }

    //
    // 実ページテーブル操作（現CR3向け）
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
    // タスクごとのデモ用 VirtPage を返す
    // - Kernel(Task0): 0x0010_0000
    // - User(Task1/Task2): 同じ 0x0011_0000 を使う（分離の証拠）
    //
    fn demo_page_for_task(&self, task_idx: usize) -> VirtPage {
        let idx = match task_idx {
            TASK0_INDEX => DEMO_VIRT_PAGE_INDEX_TASK0,
            TASK1_INDEX => DEMO_VIRT_PAGE_INDEX_USER,
            TASK2_INDEX => DEMO_VIRT_PAGE_INDEX_USER,
            _ => DEMO_VIRT_PAGE_INDEX_TASK0,
        };
        VirtPage::from_index(idx)
    }

    //
    // ──────────────────────────────────────────────
    // IPC helpers（最小プロトタイプ）
    // ──────────────────────────────────────────────
    //

    fn block_current_and_schedule(&mut self, reason: BlockedReason) {
        let idx = self.current_task;
        let id = self.tasks[idx].id;

        self.tasks[idx].state = TaskState::Blocked;
        self.tasks[idx].blocked_reason = Some(reason);
        self.tasks[idx].time_slice_used = 0;

        self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));

        self.enqueue_wait(idx);
        self.schedule_next_task();
    }

    fn wake_task_to_ready(&mut self, idx: usize) {
        if idx >= self.num_tasks {
            return;
        }

        // wait_queue から除去（入っているはず）
        let _ = self.remove_from_wait_queue(idx);

        let id = self.tasks[idx].id;

        self.tasks[idx].state = TaskState::Ready;
        self.tasks[idx].blocked_reason = None;

        self.push_event(LogEvent::TaskStateChanged(id, TaskState::Ready));
        self.enqueue_ready(idx);
    }

    /// IPC: recv（受信者が ep で待つ。送信者が居れば即 deliver）
    fn ipc_recv(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        let recv_idx = self.current_task;
        let recv_id = self.tasks[recv_idx].id;

        self.push_event(LogEvent::IpcRecvCalled { task: recv_id, ep });

        // sender が居れば 1件取り出す（Endpoint の借用を短くする）
        let send_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.dequeue_sender()
        };

        if let Some(send_idx) = send_idx_opt {
            let send_id = self.tasks[send_idx].id;
            let msg = self.tasks[send_idx].pending_send_msg.take().unwrap_or(0);

            // sender は reply 待ちで Block
            self.tasks[send_idx].state = TaskState::Blocked;
            self.tasks[send_idx].blocked_reason = Some(BlockedReason::IpcReply {
                partner: recv_id,
                ep,
            });
            self.tasks[send_idx].time_slice_used = 0;
            self.enqueue_wait(send_idx);

            // Endpoint に reply_waiter をセット
            {
                let e = &mut self.endpoints[ep.0];
                e.reply_waiter = Some(send_idx);
            }

            // receiver に値を渡す
            self.tasks[recv_idx].last_msg = Some(msg);

            self.push_event(LogEvent::IpcDelivered {
                from: send_id,
                to: recv_id,
                ep,
                msg,
            });
            return;
        }

        // sender がいない → recv_waiter をセットして block
        let already_waiting = {
            let e = &mut self.endpoints[ep.0];
            e.recv_waiter.is_some()
        };

        if !already_waiting {
            let e = &mut self.endpoints[ep.0];
            e.recv_waiter = Some(recv_idx);
        }

        self.push_event(LogEvent::IpcRecvBlocked { task: recv_id, ep });
        self.block_current_and_schedule(BlockedReason::IpcRecv { ep });
    }

    /// IPC: send（受信者が待っていれば即 deliver、いなければ send_queue に入って Block）
    fn ipc_send(&mut self, ep: EndpointId, msg: u64) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        let send_idx = self.current_task;
        let send_id = self.tasks[send_idx].id;

        self.push_event(LogEvent::IpcSendCalled { task: send_id, ep, msg });

        // recv_waiter を取り出す（Endpoint の借用を短くする）
        let recv_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.recv_waiter.take()
        };

        if let Some(recv_idx) = recv_idx_opt {
            let recv_id = self.tasks[recv_idx].id;

            // receiver を起こす（WaitQueue から除去して Ready に）
            self.wake_task_to_ready(recv_idx);

            // 値を渡す
            self.tasks[recv_idx].last_msg = Some(msg);

            // Endpoint に reply_waiter をセット
            {
                let e = &mut self.endpoints[ep.0];
                e.reply_waiter = Some(send_idx);
            }

            self.push_event(LogEvent::IpcDelivered {
                from: send_id,
                to: recv_id,
                ep,
                msg,
            });

            // sender は reply待ちで Block
            self.block_current_and_schedule(BlockedReason::IpcReply {
                partner: recv_id,
                ep,
            });
            return;
        }

        // receiver がいない → sender を send_queue に積んで Block
        self.tasks[send_idx].pending_send_msg = Some(msg);

        {
            let e = &mut self.endpoints[ep.0];
            e.enqueue_sender(send_idx);
        }

        self.push_event(LogEvent::IpcSendBlocked { task: send_id, ep });
        self.block_current_and_schedule(BlockedReason::IpcSend { ep });
    }

    /// IPC: reply（最小デモ：ep の reply_waiter を 1件だけ起こす）
    fn ipc_reply(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        let recv_idx = self.current_task;
        let recv_id = self.tasks[recv_idx].id;

        let send_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.reply_waiter.take()
        };

        let send_idx = match send_idx_opt {
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

        self.push_event(LogEvent::IpcReplyDelivered {
            from: recv_id,
            to: send_id,
            ep,
        });
    }

    //
    // IPC デモ：最小のシナリオを回す
    //
    // - Receiver: Task3（TaskId=3）
    // - Sender  : Task2（TaskId=2）
    //
    fn ipc_demo(&mut self) {
        let idx = self.current_task;
        let tid = self.tasks[idx].id;

        // 1) Receiver が recv（初回はブロックする想定）
        if !self.ipc_demo_recv_done && tid == TASK2_ID {
            self.ipc_recv(IPC_DEMO_EP0);
            self.ipc_demo_recv_done = true;
            return;
        }

        // 2) Sender が send（recv_waiter が居れば deliver して reply待ちでブロック）
        if self.ipc_demo_recv_done && !self.ipc_demo_send_done && tid == TASK1_ID {
            self.ipc_send(IPC_DEMO_EP0, 0x1234_5678_9ABC_DEF0u64);
            self.ipc_demo_send_done = true;
            return;
        }

        // 3) Receiver が reply（受信済みなら reply）
        if self.ipc_demo_send_done && !self.ipc_demo_reply_done && tid == TASK2_ID {
            if self.tasks[idx].last_msg.is_some() {
                self.ipc_reply(IPC_DEMO_EP0);
                self.ipc_demo_reply_done = true;
            }
            return;
        }

        // 4) 一周したらリセット（次の周回へ）
        if self.ipc_demo_recv_done && self.ipc_demo_send_done && self.ipc_demo_reply_done {
            self.ipc_demo_recv_done = false;
            self.ipc_demo_send_done = false;
            self.ipc_demo_reply_done = false;

            // 周回の前に inbox を消す（見やすさ）
            self.tasks[TASK2_INDEX].last_msg = None;
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

        // この tick 冒頭で「実行していた」タスク index
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

                // Sleep だけ起こす（IPC をタイマで起こさない）
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

                let task_idx = self.current_task;
                let task = self.tasks[task_idx];
                let task_id = task.id;

                // ★ User 同一VAテスト：Task1/Task2 は同じ virt ページを使う
                let page = self.demo_page_for_task(task_idx);

                // Task0 は kernel 相当、Task1/2 は user 相当としてフラグを変える
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

                        if task_idx == TASK0_INDEX {
                            logging::info(" mem_demo: applying arch paging (Task0 / current CR3)");
                            self.apply_mem_action(mem_action);
                        } else {
                            let root = match aspace.root_page_frame {
                                Some(r) => r,
                                None => {
                                    logging::error(" mem_demo: user root_page_frame is None (unexpected)");
                                    self.activity = next_activity;
                                    self.debug_check_invariants();
                                    return;
                                }
                            };

                            logging::info(" mem_demo: applying arch paging (User root / no CR3 switch)");
                            unsafe {
                                arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem);
                            }

                            // その root で virt がどう解決されるかをログで検証
                            let virt_addr_u64 = page.start_address().0;
                            arch::paging::debug_translate_in_root(root, virt_addr_u64);
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
            KernelAction::IpcDemo => {
                logging::info(" action = IpcDemo");
                self.ipc_demo();
            }
        }

        // tick途中で schedule が走ることがあるため、ran_idx の runtime を積む（デモ用途）
        self.update_runtime_for(ran_idx);

        // ブロック判定は current_task に対してのみ（ran_idx が変わっていたらスキップ）
        let blocked = if ran_idx == self.current_task {
            self.maybe_block_task(ran_idx)
        } else {
            false
        };

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

            if let Some(m) = task.last_msg {
                logging::info("  IPC:");
                logging::info_u64("    last_msg", m);
            }
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

        // ── IPC ───────────────────────
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
// カーネル起動エントリ
// ─────────────────────────────────────────────

pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    // CR3 real switch を有効化してよいか（カーネルが high-half にいるか）を判定
    let code_addr = start as usize as u64;
    let stack_probe: u64 = 0;
    let stack_addr = &stack_probe as *const u64 as u64;
    arch::paging::configure_cr3_switch_safety(code_addr, stack_addr);

    let mut kstate = KernelState::new(boot_info);

    kstate.bootstrap();

    let max_ticks = 60;
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
