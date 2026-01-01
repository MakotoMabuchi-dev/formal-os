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
//
// ★追加（計測）:
// - IPC/スケジューラの fast/slow を数える counters を KernelState に持たせる。
// - ログの量を増やさず、dump にまとめる（観測性と比較容易性を両立）。
//
// ★追加（デモ安定化）:
// - send_queue 経由を確実に踏ませるための専用フラグを追加する。
//   （「既存フラグ流用」は長期的に事故るので禁止）

mod entry;
mod ipc;
mod pagetable_init;
mod syscall;
mod user_program;
mod trace;
mod state_ref;

pub use entry::start;
pub use syscall::Syscall;
pub use state_ref::with_kernel_state;
pub use syscall::mailbox_dispatch;

use bootloader::BootInfo;
use x86_64::registers::control::Cr3;

use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};
use crate::mem::paging::{MemAction, PageFlags};
use crate::mem::address_space::{AddressSpace, AddressSpaceError, AddressSpaceKind};
use crate::mem::layout::KERNEL_SPACE_START;
use crate::kernel::ipc::IPC_ERR_DEAD_PARTNER;

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
    // ★Top3: user fault を kill できるように Dead を追加
    Dead,
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

    pub priority: u8,

    pub runtime_ticks: u64,
    pub time_slice_used: u64,

    pub address_space_id: AddressSpaceId,
    pub blocked_reason: Option<BlockedReason>,

    pub last_msg: Option<u64>,
    pub last_reply: Option<u64>,

    // syscall（mem 系など）の戻り値
    pub last_syscall_ret: Option<u64>,

    // syscall 戻り値の「未読」フラグ（ログ出力を1回にする）
    pub last_syscall_ret_unread: bool,

    pub pending_send_msg: Option<u64>,
    pub pending_syscall: Option<Syscall>,
}


// ★Top3: kill reason（最小）
#[derive(Clone, Copy)]
pub enum TaskKillReason {
    UserPageFault { addr: u64, err: u64, rip: u64 },
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

    // ★Top3: kill の観測点
    TaskKilled { task: TaskId, reason: TaskKillReason },
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

// -----------------------------------------------------------------------------
// counters (計測)
// -----------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct KernelCounters {
    // scheduler
    pub sched_switches: u64,

    // IPC
    pub ipc_send_fast: u64,
    pub ipc_send_slow: u64,
    pub ipc_recv_fast: u64,
    pub ipc_recv_slow: u64,
    pub ipc_reply_delivered: u64,

    // faults / kill
    pub task_killed_user_pf: u64,
}

impl KernelCounters {
    pub const fn new() -> Self {
        KernelCounters {
            sched_switches: 0,
            ipc_send_fast: 0,
            ipc_send_slow: 0,
            ipc_recv_fast: 0,
            ipc_recv_slow: 0,
            ipc_reply_delivered: 0,
            task_killed_user_pf: 0,
        }
    }
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
    mem_demo_stage: [u8; MAX_TASKS],
    mem_demo_frame: [Option<PhysFrame>; MAX_TASKS],

    endpoints: [Endpoint; MAX_ENDPOINTS],

    demo_msgs_delivered: u8,
    demo_replies_sent: u8,
    demo_sent_by_task2: bool,
    demo_sent_by_task1: bool,

    // ★追加（専用フラグ）:
    // send_queue 経由（recv_fastpath）を確実に踏ませるための early send を 1 回だけ発行する
    demo_early_sent_by_task0: bool,

    // ★追加（kill_cleanup_test 専用）:
    // Task1 の stage0 Map 完了直後に 1 回だけ kill する（cleanup 検証）
    #[cfg(feature = "kill_cleanup_test")]
    kill_cleanup_test_fired: bool,

    #[cfg(feature = "pf_demo")]
    pf_demo_done: bool,

    // ★追加: counters
    pub counters: KernelCounters,
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
                priority: 1,
                runtime_ticks: 0,
                time_slice_used: 0,
                address_space_id: AddressSpaceId(KERNEL_ASID_INDEX),
                blocked_reason: None,
                last_msg: None,
                last_reply: None,
                last_syscall_ret: None,
                last_syscall_ret_unread: false,
                pending_send_msg: None,
                pending_syscall: None,
            },
            Task {
                id: TASK1_ID,
                state: TaskState::Ready,
                priority: 3,
                runtime_ticks: 0,
                time_slice_used: 0,
                address_space_id: AddressSpaceId(FIRST_USER_ASID_INDEX),
                blocked_reason: None,
                last_msg: None,
                last_reply: None,
                last_syscall_ret: None,
                last_syscall_ret_unread: false,
                pending_send_msg: None,
                pending_syscall: None,
            },
            Task {
                id: TASK2_ID,
                state: TaskState::Ready,
                priority: 2,
                runtime_ticks: 0,
                time_slice_used: 0,
                address_space_id: AddressSpaceId(FIRST_USER_ASID_INDEX + 1),
                blocked_reason: None,
                last_msg: None,
                last_reply: None,
                last_syscall_ret: None,
                last_syscall_ret_unread: false,
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

        let mut ks = KernelState {
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
            mem_demo_stage: [0; MAX_TASKS],
            mem_demo_frame: [None; MAX_TASKS],

            endpoints: [
                Endpoint::new(EndpointId(0)),
                Endpoint::new(EndpointId(1)),
            ],

            demo_msgs_delivered: 0,
            demo_replies_sent: 0,
            demo_sent_by_task2: false,
            demo_sent_by_task1: false,

            demo_early_sent_by_task0: false,

            #[cfg(feature = "kill_cleanup_test")]
            kill_cleanup_test_fired: false,

            #[cfg(feature = "pf_demo")]
            pf_demo_done: false,

            counters: KernelCounters::new(),
        };

        // ---------------------------------------------------------------------
        // Step2: endpoint owner の設定（テスト時だけ）
        // - endpoint_close_test のときだけ ep0 の owner を Task3（TASK2_ID）にする
        // - 通常ビルドでは owner=None のまま（close の発火源を排除）
        // ---------------------------------------------------------------------
        #[cfg(feature = "endpoint_close_test")]
        {
            ks.endpoints[IPC_DEMO_EP0.0].owner = Some(TASK2_ID);
        }

        #[cfg(not(feature = "endpoint_close_test"))]
        {
            ks.endpoints[IPC_DEMO_EP0.0].owner = None;
        }

        ks
    }

    /// entry.rs の ring3_* demo が同じフレームアロケータを共有するための出口。
    /// （entry.rs 側で新しい PhysicalMemoryManager を作ると二重確保が起きる）
    pub(crate) fn phys_mem_mut(&mut self) -> &mut PhysicalMemoryManager {
        &mut self.phys_mem
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

    // -------------------------------------------------------------------------
    // syscall return value (観測安定化)
    // 方針:
    // - syscall 境界だけが set する
    // - user_program 側だけが unread を消費して clear する
    // -------------------------------------------------------------------------

    /// syscall 境界のみが呼ぶ: 「現在タスク」に last_syscall_ret をセットして unread にする
    pub(super) fn set_last_syscall_ret_for_current(&mut self, ret: u64) {
        let idx = self.current_task;
        if idx >= self.num_tasks {
            return;
        }
        if self.tasks[idx].state == TaskState::Dead {
            return;
        }

        self.tasks[idx].last_syscall_ret = Some(ret);
        self.tasks[idx].last_syscall_ret_unread = true;
    }

    /// user_program 側のみが呼ぶ: unread のときだけ取り出して clear する
    pub(super) fn take_unread_last_syscall_ret(&mut self, idx: usize) -> Option<u64> {
        if idx >= self.num_tasks {
            return None;
        }
        if self.tasks[idx].state == TaskState::Dead {
            return None;
        }
        if !self.tasks[idx].last_syscall_ret_unread {
            return None;
        }

        self.tasks[idx].last_syscall_ret_unread = false;
        self.tasks[idx].last_syscall_ret.take()
    }

    fn debug_check_invariants(&self) {
        // -------------------------------------------------------------------------
        // AddressSpace の基本整合
        // -------------------------------------------------------------------------
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
                logging::info_u64("as_idx", as_idx as u64);
            }
            if aspace.root_page_frame.is_none() {
                logging::error("INVARIANT VIOLATION: user address space has no root_page_frame");
                logging::info_u64("as_idx", as_idx as u64);
            }
        }

        // -------------------------------------------------------------------------
        // TaskState と BlockedReason の整合
        // -------------------------------------------------------------------------
        for (idx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            match t.state {
                TaskState::Blocked => {
                    if t.blocked_reason.is_none() {
                        logging::error("INVARIANT VIOLATION: BLOCKED task has no blocked_reason");
                        logging::info_u64("task_index", idx as u64);
                        logging::info_u64("task_id", t.id.0);
                    }
                }
                TaskState::Dead => {
                    if t.blocked_reason.is_some() {
                        logging::error("INVARIANT VIOLATION: DEAD task has blocked_reason");
                        logging::info_u64("task_index", idx as u64);
                        logging::info_u64("task_id", t.id.0);
                    }

                    if t.last_msg.is_some()
                        || t.last_reply.is_some()
                        || t.pending_send_msg.is_some()
                        || t.pending_syscall.is_some()
                    {
                        logging::error("INVARIANT VIOLATION: DEAD task has leftover task-local state");
                        logging::info_u64("task_index", idx as u64);
                        logging::info_u64("task_id", t.id.0);
                    }
                }
                _ => {
                    if t.blocked_reason.is_some() {
                        logging::error("INVARIANT VIOLATION: non-BLOCKED task has blocked_reason");
                        logging::info_u64("task_index", idx as u64);
                        logging::info_u64("task_id", t.id.0);
                    }
                }
            }
        }

        // -------------------------------------------------------------------------
        // current_task の整合（Dead が current になるのは禁止）
        // -------------------------------------------------------------------------
        if self.current_task >= self.num_tasks {
            logging::error("INVARIANT VIOLATION: current_task out of range");
        } else {
            let st = self.tasks[self.current_task].state;
            if st == TaskState::Dead {
                logging::error("INVARIANT VIOLATION: current_task is DEAD");
            } else if st != TaskState::Running {
                logging::error("INVARIANT VIOLATION: current_task is not RUNNING");
            }
        }

        // -------------------------------------------------------------------------
        // User AddressSpace の mapping 整合
        // -------------------------------------------------------------------------
        for as_idx in FIRST_USER_ASID_INDEX..self.num_tasks {
            let aspace = &self.address_spaces[as_idx];
            if aspace.kind != AddressSpaceKind::User {
                continue;
            }

            aspace.for_each_mapping(|m| {
                if !m.flags.contains(PageFlags::USER) {
                    return;
                }

                let offset = m.page.number * PAGE_SIZE;

                if offset >= arch::paging::USER_SPACE_SIZE {
                    logging::error("INVARIANT VIOLATION: user mapping offset out of user slot range");
                    logging::info_u64("as_idx", as_idx as u64);
                    logging::info_u64("virt_page_index", m.page.number);
                    logging::info_u64("offset", offset);
                }

                let _ = KERNEL_SPACE_START;
            });
        }

        // -------------------------------------------------------------------------
        // Step1: Kernel task は endpoint 構造に絶対に現れない（混入検知）
        // -------------------------------------------------------------------------
        let is_kernel_task_index = |tidx: usize| -> bool {
            if tidx >= self.num_tasks {
                return false;
            }
            let as_idx = self.tasks[tidx].address_space_id.0;
            if as_idx >= self.num_tasks {
                return false;
            }
            self.address_spaces[as_idx].kind == AddressSpaceKind::Kernel
        };

        // -------------------------------------------------------------------------
        // Endpoint の整合（構造チェック）
        // -------------------------------------------------------------------------
        for e in self.endpoints.iter() {
            // -----------------------------------------------------------------
            // Step2: closed endpoint は待ち構造を持たない（close で rescue 済みのはず）
            // -----------------------------------------------------------------
            if e.is_closed {
                if e.recv_waiter.is_some() || e.sq_len != 0 || e.rq_len != 0 {
                    logging::error("INVARIANT VIOLATION: CLOSED endpoint has waiters/queues");
                    logging::info_u64("ep_id", e.id.0 as u64);
                    logging::info_u64("sq_len", e.sq_len as u64);
                    logging::info_u64("rq_len", e.rq_len as u64);
                    if let Some(w) = e.recv_waiter {
                        logging::info_u64("recv_waiter_task_index", w as u64);
                    }
                }
            }

            if let Some(tidx) = e.recv_waiter {
                if tidx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.recv_waiter out of range");
                } else {
                    let t = &self.tasks[tidx];

                    // ★Step1: kernel task 混入検知
                    if is_kernel_task_index(tidx) {
                        logging::error("INVARIANT VIOLATION: kernel task appears as endpoint.recv_waiter");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep_id", e.id.0 as u64);
                    }

                    if t.state == TaskState::Dead {
                        logging::error("INVARIANT VIOLATION: endpoint.recv_waiter points DEAD task");
                        logging::info_u64("task_id", t.id.0);
                    }
                    if t.state != TaskState::Blocked {
                        logging::error("INVARIANT VIOLATION: recv_waiter is not BLOCKED");
                        logging::info_u64("task_id", t.id.0);
                    }

                    match t.blocked_reason {
                        Some(BlockedReason::IpcRecv { ep }) if ep == e.id => {}
                        _ => {
                            logging::error("INVARIANT VIOLATION: recv_waiter blocked_reason mismatch");
                            logging::info_u64("task_id", t.id.0);
                        }
                    }
                }
            }

            for pos in 0..e.sq_len {
                let tidx = e.send_queue[pos];
                if tidx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.send_queue idx out of range");
                    continue;
                }

                let t = &self.tasks[tidx];

                // ★Step1: kernel task 混入検知
                if is_kernel_task_index(tidx) {
                    logging::error("INVARIANT VIOLATION: kernel task appears in endpoint.send_queue");
                    logging::info_u64("task_id", t.id.0);
                    logging::info_u64("ep_id", e.id.0 as u64);
                }

                if t.state == TaskState::Dead {
                    logging::error("INVARIANT VIOLATION: send_queue contains DEAD task");
                    logging::info_u64("task_id", t.id.0);
                }
                if t.state != TaskState::Blocked {
                    logging::error("INVARIANT VIOLATION: sender in send_queue is not BLOCKED");
                    logging::info_u64("task_id", t.id.0);
                }

                match t.blocked_reason {
                    Some(BlockedReason::IpcSend { ep }) if ep == e.id => {}
                    _ => {
                        logging::error("INVARIANT VIOLATION: sender blocked_reason mismatch");
                        logging::info_u64("task_id", t.id.0);
                    }
                }
            }

            for pos in 0..e.rq_len {
                let tidx = e.reply_queue[pos];
                if tidx >= self.num_tasks {
                    logging::error("INVARIANT VIOLATION: endpoint.reply_queue idx out of range");
                    continue;
                }

                let t = &self.tasks[tidx];

                // ★Step1: kernel task 混入検知
                if is_kernel_task_index(tidx) {
                    logging::error("INVARIANT VIOLATION: kernel task appears in endpoint.reply_queue");
                    logging::info_u64("task_id", t.id.0);
                    logging::info_u64("ep_id", e.id.0 as u64);
                }

                if t.state == TaskState::Dead {
                    logging::error("INVARIANT VIOLATION: reply_queue contains DEAD task");
                    logging::info_u64("task_id", t.id.0);
                }
                if t.state != TaskState::Blocked {
                    logging::error("INVARIANT VIOLATION: reply waiter is not BLOCKED");
                    logging::info_u64("task_id", t.id.0);
                }

                match t.blocked_reason {
                    Some(BlockedReason::IpcReply { ep, partner }) if ep == e.id => {
                        if let Some(pidx) = self.tasks.iter().position(|x| x.id == partner) {
                            if self.tasks[pidx].state == TaskState::Dead {
                                logging::error("INVARIANT VIOLATION: IpcReply waiter has DEAD partner");
                                logging::info_u64("waiter_task_id", t.id.0);
                                logging::info_u64("partner_task_id", partner.0);
                            }
                        }
                    }
                    _ => {
                        logging::error("INVARIANT VIOLATION: reply waiter blocked_reason mismatch");
                        logging::info_u64("task_id", t.id.0);
                    }
                }
            }
        }

        // -------------------------------------------------------------------------
        // Step1（Top3）: Dead task 後始末の invariant
        // -------------------------------------------------------------------------
        for (tidx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            if t.state != TaskState::Dead {
                continue;
            }

            if self.is_in_ready_queue(tidx) {
                logging::error("INVARIANT VIOLATION: DEAD task is in ready_queue");
                logging::info_u64("task_index", tidx as u64);
                logging::info_u64("task_id", t.id.0);
            }

            if self.is_in_wait_queue(tidx) {
                logging::error("INVARIANT VIOLATION: DEAD task is in wait_queue");
                logging::info_u64("task_index", tidx as u64);
                logging::info_u64("task_id", t.id.0);
            }

            let as_idx = t.address_space_id.0;
            if as_idx < self.num_tasks && self.address_spaces[as_idx].kind == AddressSpaceKind::User {
                let mut found = false;
                self.address_spaces[as_idx].for_each_mapping(|m| {
                    if m.flags.contains(PageFlags::USER) {
                        found = true;
                    }
                });

                if found {
                    logging::error("INVARIANT VIOLATION: DEAD task address space still has USER mappings");
                    logging::info_u64("task_index", tidx as u64);
                    logging::info_u64("task_id", t.id.0);
                    logging::info_u64("as_idx", as_idx as u64);
                }
            }
        }

        // -------------------------------------------------------------------------
        // Step2: wait_queue は Sleep 専用
        // -------------------------------------------------------------------------
        for pos in 0..self.wq_len {
            let idx = self.wait_queue[pos];
            if idx >= self.num_tasks {
                logging::error("INVARIANT VIOLATION: wait_queue contains out-of-range idx");
                continue;
            }

            let t = &self.tasks[idx];

            if t.state == TaskState::Dead {
                logging::error("INVARIANT VIOLATION: wait_queue contains DEAD task");
                logging::info_u64("task_id", t.id.0);
                continue;
            }

            if t.state != TaskState::Blocked {
                logging::error("INVARIANT VIOLATION: wait_queue contains non-BLOCKED task");
                logging::info_u64("task_id", t.id.0);
            }

            if t.blocked_reason != Some(BlockedReason::Sleep) {
                logging::error("INVARIANT VIOLATION: wait_queue contains non-Sleep blocked_reason");
                logging::info_u64("task_id", t.id.0);
            }
        }

        for (idx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            if t.state == TaskState::Dead {
                continue;
            }
            if t.state == TaskState::Blocked && t.blocked_reason == Some(BlockedReason::Sleep) {
                if !self.is_in_wait_queue(idx) {
                    logging::error("INVARIANT VIOLATION: Sleep BLOCKED task is not in wait_queue");
                    logging::info_u64("task_id", t.id.0);
                }
            }
        }

        // -------------------------------------------------------------------------
        // Step3: 逆向き invariant（Task -> 待ち構造）
        // -------------------------------------------------------------------------
        for (tidx, t) in self.tasks.iter().enumerate().take(self.num_tasks) {
            if t.state == TaskState::Dead {
                continue;
            }
            if t.state != TaskState::Blocked {
                continue;
            }

            let reason = match t.blocked_reason {
                Some(r) => r,
                None => {
                    logging::error("INVARIANT VIOLATION: BLOCKED task has no blocked_reason (reverse check)");
                    logging::info_u64("task_id", t.id.0);
                    continue;
                }
            };

            match reason {
                BlockedReason::Sleep => {
                    if !self.is_in_wait_queue(tidx) {
                        logging::error("INVARIANT VIOLATION: Sleep BLOCKED task not in wait_queue (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                    }
                }

                BlockedReason::IpcRecv { ep } => {
                    if ep.0 >= MAX_ENDPOINTS {
                        logging::error("INVARIANT VIOLATION: IpcRecv has out-of-range ep (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep", ep.0 as u64);
                        continue;
                    }

                    let e = &self.endpoints[ep.0];
                    if e.recv_waiter != Some(tidx) {
                        logging::error("INVARIANT VIOLATION: IpcRecv task not registered as recv_waiter (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep", ep.0 as u64);
                    }

                    if self.is_in_wait_queue(tidx) {
                        logging::error("INVARIANT VIOLATION: IpcRecv task is in wait_queue (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                    }
                }

                BlockedReason::IpcSend { ep } => {
                    if ep.0 >= MAX_ENDPOINTS {
                        logging::error("INVARIANT VIOLATION: IpcSend has out-of-range ep (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep", ep.0 as u64);
                        continue;
                    }

                    let e = &self.endpoints[ep.0];
                    let mut found = false;
                    for pos in 0..e.sq_len {
                        if e.send_queue[pos] == tidx {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        logging::error("INVARIANT VIOLATION: IpcSend task not found in endpoint.send_queue (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep", ep.0 as u64);
                        logging::info_u64("sq_len", e.sq_len as u64);
                    }

                    if self.is_in_wait_queue(tidx) {
                        logging::error("INVARIANT VIOLATION: IpcSend task is in wait_queue (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                    }
                }

                BlockedReason::IpcReply { partner, ep } => {
                    if ep.0 >= MAX_ENDPOINTS {
                        logging::error("INVARIANT VIOLATION: IpcReply has out-of-range ep (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep", ep.0 as u64);
                        continue;
                    }

                    let e = &self.endpoints[ep.0];
                    let mut found = false;
                    for pos in 0..e.rq_len {
                        if e.reply_queue[pos] == tidx {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        logging::error("INVARIANT VIOLATION: IpcReply task not found in endpoint.reply_queue (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                        logging::info_u64("ep", ep.0 as u64);
                        logging::info_u64("rq_len", e.rq_len as u64);
                    }

                    if let Some(pidx) = self.tasks.iter().position(|x| x.id == partner) {
                        if self.tasks[pidx].state == TaskState::Dead {
                            logging::error("INVARIANT VIOLATION: IpcReply waiter has DEAD partner (reverse check)");
                            logging::info_u64("waiter_task_id", t.id.0);
                            logging::info_u64("partner_task_id", partner.0);
                        }
                    }

                    if self.is_in_wait_queue(tidx) {
                        logging::error("INVARIANT VIOLATION: IpcReply task is in wait_queue (reverse check)");
                        logging::info_u64("task_id", t.id.0);
                    }
                }
            }
        }
    }

    /// ring3_mailbox_loop 用:
    /// - ring3 の int80 を「Task1(User) が呼んだ」として扱う運用に合わせて、
    ///   KernelState 側の current_task/state を最小限で整合させる。
    ///
    /// 目的:
    /// - debug_check_invariants() の `current_task is not RUNNING` を止める
    /// - tick の runtime/quantum 更新条件が自然に動くようにする
    pub fn prepare_ring3_loop_current_task(&mut self) {
        // この module 内の定数をそのまま参照する（super:: は不要/不可）
        let t0 = TASK0_INDEX;
        let t1 = TASK1_INDEX;

        if t1 >= self.num_tasks {
            logging::error("prepare_ring3_loop_current_task: TASK1_INDEX out of range");
            self.should_halt = true;
            return;
        }

        if self.tasks[t1].state == TaskState::Dead {
            logging::error("prepare_ring3_loop_current_task: Task1 is DEAD; cannot enter ring3 loop");
            self.should_halt = true;
            return;
        }

        // いま current が RUNNING なら、それを Ready に戻す（Task0 想定）
        if t0 < self.num_tasks && self.tasks[t0].state == TaskState::Running && t0 != t1 {
            self.tasks[t0].state = TaskState::Ready;
            self.tasks[t0].time_slice_used = 0;
            self.tasks[t0].blocked_reason = None;
        }

        // ring3 を「Task1 が走っている」として扱う
        self.current_task = t1;
        self.tasks[t1].state = TaskState::Running;
        self.tasks[t1].time_slice_used = 0;
        self.tasks[t1].blocked_reason = None;

        // ready_queue に Task1 が残っていたら消す（あっても動くが invariant 的に気持ち悪い）
        let _ = self.remove_from_ready_queue(t1);
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

    fn remove_from_ready_queue(&mut self, idx: usize) -> bool {
        if self.rq_len == 0 {
            return false;
        }
        let mut write_pos = 0usize;
        let mut removed = false;

        for read_pos in 0..self.rq_len {
            let v = self.ready_queue[read_pos];
            if v == idx {
                removed = true;
                continue;
            }
            self.ready_queue[write_pos] = v;
            write_pos += 1;
        }

        self.rq_len = write_pos;
        removed
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

    fn remove_task_from_endpoints(&mut self, idx: usize) {
        for ep in self.endpoints.iter_mut() {
            if ep.recv_waiter == Some(idx) {
                ep.recv_waiter = None;
            }

            let mut pos = 0;
            while pos < ep.sq_len {
                if ep.send_queue[pos] == idx {
                    ep.send_queue[pos] = ep.send_queue[ep.sq_len - 1];
                    ep.sq_len -= 1;
                } else {
                    pos += 1;
                }
            }

            let mut pos = 0;
            while pos < ep.rq_len {
                if ep.reply_queue[pos] == idx {
                    ep.reply_queue[pos] = ep.reply_queue[ep.rq_len - 1];
                    ep.rq_len -= 1;
                } else {
                    pos += 1;
                }
            }
        }
    }

    fn resolve_ipc_reply_waiters_for_dead_partner(&mut self, dead_partner: TaskId) {
        let mut wake_list: [Option<usize>; MAX_TASKS] = [None; MAX_TASKS];
        let mut wake_len: usize = 0;

        for ep in self.endpoints.iter_mut() {
            let mut pos: usize = 0;
            while pos < ep.rq_len {
                let waiter_idx = ep.reply_queue[pos];

                let should_rescue = waiter_idx < self.num_tasks
                    && self.tasks[waiter_idx].state == TaskState::Blocked
                    && matches!(
                        self.tasks[waiter_idx].blocked_reason,
                        Some(BlockedReason::IpcReply { partner, ep: wep })
                            if partner == dead_partner && wep == ep.id
                    );

                if should_rescue {
                    let last = ep.rq_len - 1;
                    ep.reply_queue[pos] = ep.reply_queue[last];
                    ep.rq_len -= 1;

                    self.tasks[waiter_idx].blocked_reason = None;
                    self.tasks[waiter_idx].last_reply = Some(IPC_ERR_DEAD_PARTNER);

                    if wake_len < MAX_TASKS {
                        wake_list[wake_len] = Some(waiter_idx);
                        wake_len += 1;
                    }

                    crate::logging::error("ipc: reply waiter rescued due to DEAD partner");
                    crate::logging::info_u64("waiter_task_id", self.tasks[waiter_idx].id.0);
                    crate::logging::info_u64("dead_partner_task_id", dead_partner.0);

                    continue;
                }

                pos += 1;
            }
        }

        for i in 0..wake_len {
            if let Some(waiter_idx) = wake_list[i] {
                self.wake_task_to_ready(waiter_idx);
            }
        }
    }

    fn cleanup_user_mappings_of_address_space(&mut self, as_idx: usize) {
        if as_idx >= self.num_tasks {
            return;
        }
        if self.address_spaces[as_idx].kind != AddressSpaceKind::User {
            return;
        }

        let root = match self.address_spaces[as_idx].root_page_frame {
            Some(r) => r,
            None => {
                logging::error("cleanup_user_mappings: user root_page_frame is None");
                panic!("cleanup_user_mappings: user root_page_frame is None");
            }
        };

        // ---- 観測性: 事前情報 ----
        logging::info("cleanup_user_mappings: start");
        logging::info_u64("as_idx", as_idx as u64);
        logging::info_u64("root_page_frame_index", root.number);

        let before_count = self.address_spaces[as_idx].mapping_count();
        logging::info_u64("cleanup_user_mappings: before clear mapping_count", before_count as u64);

        // ---- 重要: USER フラグの mapping を “AddressSpace の全 mapping” から拾う ----
        // for_each_user_mapping_page() が将来壊れても、ここで拾えるようにする
        let mut pages: [Option<VirtPage>; 64] = [None; 64];
        let mut n: usize = 0;

        {
            let aspace = &self.address_spaces[as_idx];
            aspace.for_each_mapping(|m| {
                if !m.flags.contains(PageFlags::USER) {
                    return;
                }
                if n < pages.len() {
                    pages[n] = Some(m.page);
                    n += 1;
                } else {
                    // 64を超えるなら、まずは “検証用途なので” ここで止める（必要なら後で拡張）
                    logging::error("cleanup_user_mappings: too many USER mappings; truncated");
                }
            });
        }

        logging::info_u64("cleanup_user_mappings: collected_user_pages", n as u64);

        // ---- AddressSpace 側の“ユーザマッピング記録”を消す（論理状態）----
        {
            let aspace = &mut self.address_spaces[as_idx];
            aspace.clear_user_mappings();
        }

        let after_clear_count = self.address_spaces[as_idx].mapping_count();
        logging::info_u64("cleanup_user_mappings: after clear mapping_count", after_clear_count as u64);

        // ---- arch の実ページテーブル側を unmap（物理状態）----
        let mut applied: usize = 0;

        for i in 0..n {
            let page = match pages[i] {
                Some(p) => p,
                None => continue,
            };

            let mem_action = MemAction::Unmap { page };

            match unsafe { arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem) } {
                Ok(()) => {
                    applied += 1;

                    // うるさくなりすぎないように先頭数件だけ translate を確認
                    if i < 4 {
                        let virt_addr_u64 = arch::paging::USER_SPACE_BASE + page.start_address().0;
                        logging::info("cleanup_user_mappings: debug_translate_after_unmap");
                        logging::info_u64("virt_addr", virt_addr_u64);
                        arch::paging::debug_translate_in_root(root, virt_addr_u64);
                    }
                }
                Err(_e) => {
                    logging::error("cleanup_user_mappings: arch unmap failed; abort (fail-stop)");
                    logging::info_u64("as_idx", as_idx as u64);
                    logging::info_u64("virt_page_index", page.number);
                    panic!("cleanup_user_mappings: arch unmap failed");
                }
            }
        }

        logging::info_u64("cleanup_user_mappings: arch_unmap_applied", applied as u64);

        let after_unmap_count = self.address_spaces[as_idx].mapping_count();
        logging::info_u64("cleanup_user_mappings: after unmap mapping_count", after_unmap_count as u64);

        logging::info("cleanup_user_mappings: done");
    }

    fn kill_task(&mut self, idx: usize, reason: TaskKillReason) {
        // counters を “reason” ベースで一元管理（経路差でズレないようにする）
        match reason {
            TaskKillReason::UserPageFault { .. } => {
                self.counters.task_killed_user_pf += 1;
            }
        }

        if idx >= self.num_tasks {
            return;
        }

        let dead_id = self.tasks[idx].id;
        let as_idx = self.tasks[idx].address_space_id.0;

        let _ = self.remove_from_ready_queue(idx);
        let _ = self.remove_from_wait_queue(idx);
        self.remove_task_from_endpoints(idx);

        self.tasks[idx].state = TaskState::Dead;
        self.tasks[idx].blocked_reason = None;
        self.tasks[idx].pending_syscall = None;
        self.tasks[idx].pending_send_msg = None;
        self.tasks[idx].last_msg = None;
        self.tasks[idx].last_reply = None;
        self.tasks[idx].last_syscall_ret = None;
        self.tasks[idx].last_syscall_ret_unread = false;
        self.tasks[idx].time_slice_used = 0;

        self.mem_demo_stage[idx] = 0;
        self.mem_demo_mapped[idx] = false;
        self.mem_demo_frame[idx] = None;

        // ★ベストプラクティス: デモ用状態も kill で一貫して掃除しておく（観測の再現性）
        self.demo_early_sent_by_task0 = false;

        self.cleanup_user_mappings_of_address_space(as_idx);

        // ---------------------------------------------------------------------
        // Step2: owner が死んだ endpoint を close し、waiters を rescue する
        // - close を先に実行して “CLOSED を優先” する（DEAD_PARTNER より優先）
        // - Rust の借用規則のため、ep_id を先に集めてから close を呼ぶ
        // ---------------------------------------------------------------------
        let mut to_close: [Option<EndpointId>; MAX_ENDPOINTS] = [None; MAX_ENDPOINTS];
        let mut n: usize = 0;

        for e in self.endpoints.iter() {
            if e.owner == Some(dead_id) {
                if n < MAX_ENDPOINTS {
                    to_close[n] = Some(e.id);
                    n += 1;
                }
            }
        }

        for i in 0..n {
            if let Some(ep_id) = to_close[i] {
                self.close_endpoint_and_rescue_waiters(ep_id);
            }
        }

        // ---------------------------------------------------------------------
        // 既存: dead partner を待つ reply_waiter を rescue
        // - endpoint close を先に実行したので、ここは補助的（残骸拾い）
        // ---------------------------------------------------------------------
        self.resolve_ipc_reply_waiters_for_dead_partner(dead_id);

        self.push_event(LogEvent::TaskKilled { task: dead_id, reason });
        self.push_event(LogEvent::TaskStateChanged(dead_id, TaskState::Dead));

        if idx == self.current_task {
            self.schedule_next_task();
        }
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

        // --- 修正2: ready_queue を Ready のみに掃除する（compaction）---
        let mut write_pos: usize = 0;
        for read_pos in 0..self.rq_len {
            let idx = self.ready_queue[read_pos];

            if idx >= self.num_tasks {
                continue;
            }
            if self.tasks[idx].state != TaskState::Ready {
                continue;
            }

            self.ready_queue[write_pos] = idx;
            write_pos += 1;
        }
        self.rq_len = write_pos;

        if self.rq_len == 0 {
            return None;
        }

        // --- 最高優先度を選ぶ ---
        let mut best_pos: usize = 0;
        let mut best_idx: usize = self.ready_queue[0];

        if best_idx >= self.num_tasks {
            // ここに来るのはほぼないが、念のため
            self.rq_len = 0;
            return None;
        }

        let mut best_prio: u8 = self.tasks[best_idx].priority;

        for pos in 1..self.rq_len {
            let idx = self.ready_queue[pos];
            if idx >= self.num_tasks {
                continue;
            }

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

        // VGA 切替（そのまま）
        {
            let cur_as_idx = self.tasks[self.current_task].address_space_id.0;
            match self.address_spaces[cur_as_idx].kind {
                AddressSpaceKind::Kernel => logging::set_vga_enabled(true),
                AddressSpaceKind::User => logging::set_vga_enabled(false),
            }
        }

        // -------------------------------------------------------------
        // 0) ready_queue を “Ready のみ” に掃除（常に）
        // -------------------------------------------------------------
        self.compact_ready_queue_to_ready_only();

        // -------------------------------------------------------------
        // 1) prev が Running なら Ready に戻す（Idle は戻さない）
        // -------------------------------------------------------------
        if prev_idx < self.num_tasks && self.tasks[prev_idx].state == TaskState::Running && prev_idx != TASK0_INDEX {
            self.tasks[prev_idx].state = TaskState::Ready;
            self.tasks[prev_idx].blocked_reason = None;
            self.tasks[prev_idx].time_slice_used = 0;
            self.push_event(LogEvent::TaskStateChanged(prev_id, TaskState::Ready));
            self.enqueue_ready(prev_idx);
        }

        // -------------------------------------------------------------
        // 2) ready が無い → Sleep を 1つ起こす → それでも無いなら Idle
        // -------------------------------------------------------------
        if self.rq_len == 0 {
            if self.wq_len > 0 {
                logging::info("schedule_next_task: no ready tasks; try wake sleep waiter");
                self.maybe_wake_one_sleep_task();
                self.compact_ready_queue_to_ready_only();
            }

            if self.rq_len == 0 {
                logging::info("schedule_next_task: still no ready tasks; run idle(task0) and continue");
                let idle_idx = TASK0_INDEX;

                if self.tasks[idle_idx].state == TaskState::Dead {
                    logging::error("schedule_next_task: idle task is DEAD; halt-safe");
                    self.should_halt = true;
                    return;
                }

                // ★最重要：current_task が指すタスクは必ず Running
                self.tasks[idle_idx].state = TaskState::Running;
                self.tasks[idle_idx].blocked_reason = None;
                self.tasks[idle_idx].time_slice_used = 0;
                self.current_task = idle_idx;

                let kernel_root = self.address_spaces[KERNEL_ASID_INDEX]
                    .root_page_frame
                    .expect("kernel root_page_frame must exist");
                arch::paging::switch_address_space_quiet(kernel_root);
                logging::set_vga_enabled(true);

                self.push_event(LogEvent::TaskSwitched(self.tasks[idle_idx].id));
                self.push_event(LogEvent::TaskStateChanged(self.tasks[idle_idx].id, TaskState::Running));
                return;
            }
        }

        // -------------------------------------------------------------
        // 3) ready がある前提：選ぶ
        // -------------------------------------------------------------
        logging::info("sched: dump ready_queue before dequeue");
        logging::info_u64("rq_len", self.rq_len as u64);
        for pos in 0..self.rq_len {
            let idx = self.ready_queue[pos];
            logging::info_u64("rq[pos].task_index", idx as u64);
            if idx < self.num_tasks {
                let t = &self.tasks[idx];
                logging::info_u64("rq[pos].task_id", t.id.0);
                match t.state {
                    TaskState::Ready => logging::info("rq[pos].state = Ready"),
                    TaskState::Running => logging::info("rq[pos].state = Running"),
                    TaskState::Blocked => logging::info("rq[pos].state = Blocked"),
                    TaskState::Dead => logging::info("rq[pos].state = Dead"),
                }
                logging::info_u64("rq[pos].prio", t.priority as u64);
            }
        }

        let next_idx = match self.dequeue_ready_highest_priority() {
            Some(i) => i,
            None => {
                logging::error("schedule_next_task: ready_queue broken; halt-safe");
                self.should_halt = true;
                return;
            }
        };

        if next_idx >= self.num_tasks {
            logging::error("schedule_next_task: next_idx out of range; halt-safe");
            self.should_halt = true;
            return;
        }

        let next_id = self.tasks[next_idx].id;
        let as_idx = self.tasks[next_idx].address_space_id.0;

        // ★最重要：current_task を更新したら必ず state=Running
        self.tasks[next_idx].state = TaskState::Running;
        self.tasks[next_idx].time_slice_used = 0;
        self.tasks[next_idx].blocked_reason = None;
        self.current_task = next_idx;

        let next_kind = self.address_spaces[as_idx].kind;
        let root = self.address_spaces[as_idx].root_page_frame;

        match next_kind {
            AddressSpaceKind::User => {
                logging::set_vga_enabled(false);
                arch::paging::switch_address_space(root);
            }
            AddressSpaceKind::Kernel => {
                let kernel_root = self.address_spaces[KERNEL_ASID_INDEX]
                    .root_page_frame
                    .expect("kernel root_page_frame must exist");
                arch::paging::switch_address_space_quiet(kernel_root);
                logging::set_vga_enabled(true);
                logging::info("switched to task");
                logging::info_u64("task_id", next_id.0);
            }
        }

        if next_idx != prev_idx {
            self.counters.sched_switches += 1;
        }

        self.push_event(LogEvent::TaskSwitched(next_id));
        self.push_event(LogEvent::TaskStateChanged(next_id, TaskState::Running));
    }

    fn compact_ready_queue_to_ready_only(&mut self) {
        let mut write_pos: usize = 0;
        for read_pos in 0..self.rq_len {
            let idx = self.ready_queue[read_pos];
            if idx >= self.num_tasks {
                continue;
            }
            if self.tasks[idx].state != TaskState::Ready {
                continue;
            }
            self.ready_queue[write_pos] = idx;
            write_pos += 1;
        }
        self.rq_len = write_pos;
    }

    fn update_runtime_for(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_runtime_for: ran_idx out of range");
            return;
        }
        if self.tasks[ran_idx].state == TaskState::Dead {
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

        if self.tasks[idx].state == TaskState::Dead {
            logging::error("block_current: called for DEAD task; ignore");
            return;
        }

        // Kernel task は IPC で BLOCK させない（既存仕様）
        let as_idx = self.tasks[idx].address_space_id.0;
        if as_idx < self.num_tasks && self.address_spaces[as_idx].kind == AddressSpaceKind::Kernel {
            match reason {
                BlockedReason::IpcRecv { ep }
                | BlockedReason::IpcSend { ep }
                | BlockedReason::IpcReply { ep, .. } => {
                    logging::error("block_current: kernel task would block on IPC; convert to error");
                    logging::info_u64("task_id", id.0);
                    logging::info_u64("ep", ep.0 as u64);

                    self.tasks[idx].last_reply = Some(IPC_ERR_DEAD_PARTNER);
                    self.tasks[idx].pending_send_msg = None;
                    return;
                }
                BlockedReason::Sleep => {}
            }
        }

        // ★ここから下は “正規入口” に寄せる
        self.block_task(idx, reason);
    }

    /// 任意タスクを Blocked に落とす（ready_queue/wait_queue 整合性の唯一の入口）
    fn block_task(&mut self, idx: usize, reason: BlockedReason) {
        if idx >= self.num_tasks {
            logging::error("block_task: idx out of range");
            return;
        }

        let id = self.tasks[idx].id;

        if self.tasks[idx].state == TaskState::Dead {
            logging::error("block_task: called for DEAD task; ignore");
            logging::info_u64("task_id", id.0);
            return;
        }

        // Blocked に落とすなら ready_queue に居てはいけない
        let _ = self.remove_from_ready_queue(idx);

        // ★重要: すでに Blocked でも「理由の更新」を許可する（IpcSend -> IpcReply など）
        if self.tasks[idx].state == TaskState::Blocked {
            let prev_reason = self.tasks[idx].blocked_reason;

            self.tasks[idx].blocked_reason = Some(reason);
            self.tasks[idx].time_slice_used = 0;

            self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));

            // Sleep に遷移した時だけ wait_queue へ（多重 enqueue を避ける）
            match (prev_reason, reason) {
                (Some(BlockedReason::Sleep), BlockedReason::Sleep) => {}
                (_, BlockedReason::Sleep) => self.enqueue_wait(idx),
                _ => {}
            }
            return;
        }

        // ここからは Running/Ready などから Blocked へ落とす通常パス
        self.tasks[idx].state = TaskState::Blocked;
        self.tasks[idx].blocked_reason = Some(reason);
        self.tasks[idx].time_slice_used = 0;

        self.push_event(LogEvent::TaskStateChanged(id, TaskState::Blocked));

        if let BlockedReason::Sleep = reason {
            self.enqueue_wait(idx);
        }
    }

    fn wake_task_to_ready(&mut self, idx: usize) {
        if idx >= self.num_tasks {
            return;
        }
        if self.tasks[idx].state == TaskState::Dead {
            return;
        }

        // 既に Ready/Running なら何もしない（重複投入を防ぐ）
        if self.tasks[idx].state == TaskState::Ready || self.tasks[idx].state == TaskState::Running {
            self.tasks[idx].blocked_reason = None;
            return;
        }

        // Blocked から戻す
        self.tasks[idx].state = TaskState::Ready;
        self.tasks[idx].blocked_reason = None;
        self.tasks[idx].time_slice_used = 0;

        // ready_queue に二重投入しない
        if !self.ready_queue_contains(idx) {
            if self.rq_len < MAX_TASKS {
                self.ready_queue[self.rq_len] = idx;
                self.rq_len += 1;
            }
        }

        self.push_event(LogEvent::TaskStateChanged(self.tasks[idx].id, TaskState::Ready));
    }

    fn ready_queue_contains(&self, idx: usize) -> bool {
        for pos in 0..self.rq_len {
            if self.ready_queue[pos] == idx {
                return true;
            }
        }
        false
    }

    fn maybe_block_task(&mut self, ran_idx: usize) -> bool {
        if ran_idx >= self.num_tasks {
            logging::error("maybe_block_task: ran_idx out of range");
            return false;
        }
        if self.tasks[ran_idx].state == TaskState::Dead {
            return false;
        }
        if ran_idx != self.current_task {
            return false;
        }

        // デモ安定化: fake I/O wait を無効化（IPC/スケジューラ観測に集中）
        false
    }

    fn update_time_slice_for_and_maybe_schedule(&mut self, ran_idx: usize) {
        if ran_idx >= self.num_tasks {
            logging::error("update_time_slice_for_and_maybe_schedule: ran_idx out of range");
            return;
        }
        if self.tasks[ran_idx].state == TaskState::Dead {
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
            logging::info("quantum expired");
            self.push_event(LogEvent::QuantumExpired(id, self.tasks[ran_idx].time_slice_used));

            if self.rq_len == 0 {
                logging::info("quantum expired but no ready tasks; continue running");
                self.tasks[ran_idx].time_slice_used = 0;
                return;
            }

            logging::info("quantum expired; scheduling next task");
            self.schedule_next_task();
        }
    }

    fn maybe_wake_one_sleep_task(&mut self) {
        for pos in 0..self.wq_len {
            let idx = self.wait_queue[pos];
            if idx >= self.num_tasks {
                continue;
            }
            if self.tasks[idx].state == TaskState::Dead {
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

    fn kill_current_task_due_to_user_pf(&mut self, pf: arch::paging::PageFaultInfo) {
        let idx = self.current_task;
        let task_id = self.tasks[idx].id;

        let as_idx = self.tasks[idx].address_space_id.0;
        let kind = self.address_spaces[as_idx].kind;

        match kind {
            AddressSpaceKind::User => {
                logging::error("USER PAGE FAULT => kill current task");
            }
            AddressSpaceKind::Kernel => {
                logging::error("KERNEL CONTEXT PAGE FAULT (during guarded user access) => treat as user fault and kill");
            }
        }

        logging::info_u64("task_id", task_id.0);
        logging::info_u64("addr", pf.addr);
        logging::info_u64("err", pf.err);
        logging::info_u64("rip", pf.rip);

        // ------------------------------------------------------------
        // デモ安定化:
        // - mem_demo の stage3 は「Unmap 後アクセスで #PF が出る」確認用。
        // - ただし通常デモでは sender が死んで IPC が止まるので、kill は feature 時のみ。
        // ------------------------------------------------------------
        #[cfg(feature = "pf_demo")]
        {
            self.kill_task(
                idx,
                TaskKillReason::UserPageFault { addr: pf.addr, err: pf.err, rip: pf.rip },
            );
        }

        #[cfg(not(feature = "pf_demo"))]
        {
            logging::error("USER PAGE FAULT (demo): ignored (pf_demo feature disables this)");
            logging::info_u64("task_id", task_id.0);
            // 再実行ループを避けたい場合は stage を巻き戻す等もあるが、
            // まずは “kill しない” だけで IPC の継続観測を優先する。
        }

    }

    fn do_mem_demo_normal(&mut self) {
        let task_idx = self.current_task;
        let task = self.tasks[task_idx];
        let task_id = task.id;

        if task.state == TaskState::Dead {
            return;
        }

        let page = self.demo_page_for_task(task_idx);

        let flags = if task_idx == TASK0_INDEX {
            PageFlags::PRESENT | PageFlags::WRITABLE
        } else {
            PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER
        };

        let as_idx = task.address_space_id.0;
        let aspace_kind = if as_idx < self.num_tasks {
            self.address_spaces[as_idx].kind
        } else {
            AddressSpaceKind::Kernel
        };

        // -------------------------------------------------------------------------
        // User mem demo: Map -> RW -> Unmap -> RW(after unmap => #PF expected)
        // ★変更点:
        // - stage0/stage2 の map/unmap は「必ず syscall 経由」にする
        // - ここ（デモ直呼び）では arch::paging::apply_mem_action_in_root を呼ばない
        // -------------------------------------------------------------------------
        if task_idx != TASK0_INDEX {
            if aspace_kind != AddressSpaceKind::User {
                logging::error("mem_demo[user]: task is not User; skip");
                return;
            }

            let root = match self.address_spaces[as_idx].root_page_frame {
                Some(r) => r,
                None => {
                    logging::error("mem_demo: user root_page_frame is None (unexpected)");
                    panic!("user root_page_frame is None");
                }
            };

            let virt_addr_u64 = arch::paging::USER_SPACE_BASE + page.start_address().0;

            let stage = self.mem_demo_stage[task_idx];

            match stage {
                // --- stage0: Map（syscall）---
                0 => {
                    logging::info("mem_demo[user]: stage0 Map (via syscall)");

                    // syscall を積むだけ（この tick の後半で handle_pending_syscall が実行する）
                    self.tasks[task_idx].pending_syscall = Some(Syscall::PageMap { page, flags });

                    // 期待: 同 tick 内で map が完了し、次の MemDemo で stage1 へ
                    self.mem_demo_stage[task_idx] = 1;
                    return;
                }

                // --- stage1: RW ---
                1 => {
                    let user_virt = virt_addr_u64 as *mut u64;

                    let test_value: u64 = 0xC0DE_0000_0000_0000u64
                        ^ ((task_id.0 & 0xFFFF) << 16)
                        ^ (self.tick_count & 0xFFFF);

                    // ★arch 側で user_root -> kernel_root まで責務を完結させる
                    let kernel_root = self.address_spaces[KERNEL_ASID_INDEX]
                        .root_page_frame
                        .expect("kernel root_page_frame must exist");

                    let rw_result = arch::paging::guarded_user_rw_u64_in_root(
                        root,
                        kernel_root,
                        user_virt,
                        test_value,
                    );

                    logging::info("mem_demo[user]: stage1 RW (guarded; returned to kernel CR3)");

                    match rw_result {
                        Ok(read_back) => {
                            logging::info("user_mem_test: read_back");
                            logging::info_u64("", read_back);
                            if read_back == test_value {
                                logging::info("user_mem_test: OK (value matched)");
                            } else {
                                logging::error("user_mem_test: MISMATCH!");
                                logging::info_u64("expected", test_value);
                                logging::info_u64("got", read_back);
                            }
                        }
                        Err(pf) => {
                            logging::error("UNEXPECTED: #PF in stage1 RW (Map直後のはず)");
                            self.kill_current_task_due_to_user_pf(pf);
                            self.mem_demo_stage[task_idx] = 0;
                            return;
                        }
                    }

                    // 参考：translate（任意）
                    arch::paging::debug_translate_in_root(root, virt_addr_u64);

                    self.mem_demo_stage[task_idx] = 2;
                    return;
                }

                // --- stage2: Unmap（syscall）---
                2 => {
                    logging::info("mem_demo[user]: stage2 Unmap (via syscall)");

                    self.tasks[task_idx].pending_syscall = Some(Syscall::PageUnmap { page });

                    // 期待: 同 tick 内で unmap が完了し、次の MemDemo で stage3 へ
                    self.mem_demo_stage[task_idx] = 3;
                    return;
                }

                // --- stage3: RW after unmap (=> #PF expected) ---
                _ => {
                    let user_virt = virt_addr_u64 as *mut u64;

                    let test_value: u64 = 0xDEAD_0000_0000_0000u64
                        ^ ((task_id.0 & 0xFFFF) << 16)
                        ^ (self.tick_count & 0xFFFF);

                    // ★arch 側で user_root -> kernel_root まで責務を完結させる
                    let kernel_root = self.address_spaces[KERNEL_ASID_INDEX]
                        .root_page_frame
                        .expect("kernel root_page_frame must exist");

                    let rw_result = arch::paging::guarded_user_rw_u64_in_root(
                        root,
                        kernel_root,
                        user_virt,
                        test_value,
                    );

                    logging::info("mem_demo[user]: stage3 RW-after-unmap (guarded; returned to kernel CR3)");

                    match rw_result {
                        Ok(read_back) => {
                            // ここが成功したら PT/ガードが壊れてる
                            logging::error("UNEXPECTED: RW succeeded after Unmap");
                            logging::info_u64("read_back", read_back);

                            self.mem_demo_stage[task_idx] = 0;
                            return;
                        }
                        Err(pf) => {
                            // 期待通り：ユーザ領域アクセスで #PF → （pf_demo なら kill）
                            self.kill_current_task_due_to_user_pf(pf);
                            self.mem_demo_stage[task_idx] = 0;
                            return;
                        }
                    }
                }
            }
        }

        // -------------------------------------------------------------------------
        // Kernel task mem demo: Map <-> Unmap を繰り返すだけ（従来通り）
        // -------------------------------------------------------------------------
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

        let apply_res = {
            let aspace = &mut self.address_spaces[as_idx];
            aspace.apply(mem_action)
        };

        match apply_res {
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

        logging::info("mem_demo: applying arch paging (Task0 / current CR3)");
        match unsafe { arch::paging::apply_mem_action(mem_action, &mut self.phys_mem) } {
            Ok(()) => {}
            Err(_e) => {
                logging::error("arch::paging::apply_mem_action failed; abort (fail-stop)");
                panic!("arch apply_mem_action failed");
            }
        }

        self.mem_demo_mapped[task_idx] = !self.mem_demo_mapped[task_idx];

        self.push_event(LogEvent::MemActionApplied {
            task: task_id,
            address_space: task.address_space_id,
            action: mem_action,
        });
    }

    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        logging::info("KernelState::tick()");
        logging::info_u64("tick_count", self.tick_count);

        if (self.tick_count % 50) == 0 {
            logging::info("heartbeat");
            logging::info_u64("tick", self.tick_count);
        }

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

                // ring3_mailbox_loop: IPC の loop 検証が主目的なので mem_demo は止める
                #[cfg(feature = "ring3_mailbox_loop")]
                {
                    logging::info("mem_demo skipped (ring3_mailbox_loop)");
                }

                #[cfg(not(feature = "ring3_mailbox_loop"))]
                {
                    self.do_mem_demo();
                }
            }
        }

        if ran_idx < self.num_tasks && self.tasks[ran_idx].state == TaskState::Dead {
            logging::info("tick: running task died in this tick; skip syscall/runtime/quantum updates");
            self.activity = next_activity;

            // ★保険：tick 終了時に current_task が RUNNING でなければスケジュールして整合を回復
            if self.current_task < self.num_tasks && self.tasks[self.current_task].state != TaskState::Running {
                logging::error("tick: current_task not RUNNING at end-of-tick; forcing schedule");
                logging::info_u64("current_task", self.current_task as u64);
                self.schedule_next_task();
            }

            self.debug_check_invariants();
            return;
        }

        // 1 tick あたり syscall 実行は最大 1 回
        // - do_mem_demo() が pending_syscall を積む
        // - user_step_issue_syscall() も積みうる（ただし「すでに積まれてたら return」）
        // 1 tick あたり syscall 実行は最大 1 回
        // - ring3_mailbox_loop では Task1(User) は ring3 側が int80 経由で駆動するため、
        //   カーネル内 user_program が last_reply を消費しないように Task1 をスキップする。
        #[cfg(feature = "ring3_mailbox_loop")]
        {
            if ran_idx != TASK1_INDEX {
                self.user_step_issue_syscall(ran_idx);
            }
        }

        #[cfg(not(feature = "ring3_mailbox_loop"))]
        {
            self.user_step_issue_syscall(ran_idx);
        }

        if ran_idx == self.current_task {
            self.handle_pending_syscall_if_any();
        }

        self.update_runtime_for(ran_idx);

        let still_running = ran_idx == self.current_task
            && self.tasks[ran_idx].state == TaskState::Running;

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

        logging::info("=== Task Dump ===");
        for i in 0..self.num_tasks {
            let task = &self.tasks[i];

            logging::info("TASK:");
            logging::info_u64("task_index", i as u64);
            logging::info_u64("task_id", task.id.0);

            match task.state {
                TaskState::Ready => logging::info("state = Ready"),
                TaskState::Running => logging::info("state = Running"),
                TaskState::Blocked => logging::info("state = Blocked"),
                TaskState::Dead => logging::info("state = Dead"),
            }

            logging::info_u64("address_space_id", task.address_space_id.0 as u64);

            match task.blocked_reason {
                None => logging::info("blocked_reason = None"),
                Some(BlockedReason::Sleep) => logging::info("blocked_reason = Sleep"),
                Some(BlockedReason::IpcRecv { ep }) => {
                    logging::info("blocked_reason = IpcRecv");
                    logging::info_u64("blocked_ep", ep.0 as u64);
                }
                Some(BlockedReason::IpcSend { ep }) => {
                    logging::info("blocked_reason = IpcSend");
                    logging::info_u64("blocked_ep", ep.0 as u64);
                }
                Some(BlockedReason::IpcReply { partner, ep }) => {
                    logging::info("blocked_reason = IpcReply");
                    logging::info_u64("blocked_ep", ep.0 as u64);
                    logging::info_u64("blocked_partner_task_id", partner.0);
                }
            }

            match task.pending_syscall {
                Some(_) => logging::info("pending_syscall = Some"),
                None => logging::info("pending_syscall = None"),
            }

            match task.pending_send_msg {
                Some(v) => {
                    logging::info("pending_send_msg = Some");
                    logging::info_u64("pending_send_msg_value", v);
                }
                None => logging::info("pending_send_msg = None"),
            }

            match task.last_msg {
                Some(v) => {
                    logging::info("last_msg = Some");
                    logging::info_u64("last_msg_value", v);
                }
                None => logging::info("last_msg = None"),
            }

            {
                if let Some(v) = task.last_reply {
                    logging::info("last_reply = Some");
                    logging::info_u64("last_reply_value", v);
                } else {
                    logging::info("last_reply = None");
                }
            }

            // --- 追加: syscall（mem系など）の戻り値 ---
            {
                if let Some(v) = task.last_syscall_ret {
                    logging::info("last_syscall_ret = Some");
                    logging::info_u64("last_syscall_ret_value", v);
                } else {
                    logging::info("last_syscall_ret = None");
                }
            }
        }
        logging::info("=== End of Task Dump ===");

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

        logging::info("=== Counters Dump ===");
        logging::info_u64("sched_switches", self.counters.sched_switches);

        logging::info_u64("ipc_send_fast", self.counters.ipc_send_fast);
        logging::info_u64("ipc_send_slow", self.counters.ipc_send_slow);
        logging::info_u64("ipc_recv_fast", self.counters.ipc_recv_fast);
        logging::info_u64("ipc_recv_slow", self.counters.ipc_recv_slow);
        logging::info_u64("ipc_reply_delivered", self.counters.ipc_reply_delivered);

        logging::info_u64("task_killed_user_pf", self.counters.task_killed_user_pf);
        logging::info("=== End of Counters Dump ===");
    }
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
                TaskState::Dead => logging::info("to DEAD"),
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
        LogEvent::TaskKilled { task, reason } => {
            logging::info("EVENT: TaskKilled");
            logging::info_u64("task", task.0);
            match reason {
                TaskKillReason::UserPageFault { addr, err, rip } => {
                    logging::info("reason = UserPageFault");
                    logging::info_u64("addr", addr);
                    logging::info_u64("err", err);
                    logging::info_u64("rip", rip);
                }
            }
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