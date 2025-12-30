// kernel/src/kernel/ipc.rs
//
// IPC（同期: send/recv/reply）
// - Endpoint に send_queue / recv_waiter / reply_queue を持たせる。
// - KernelState の ipc_* は、syscall からのみ呼ばれる想定。
// - キュー順序は swap-remove で抽象化（公平性は後回し）。
//
// 設計メモ（フォーマル化を意識）:
// - 「前提崩れ」は panic せず、ログ＋return（fail-safe）で状態破壊を避ける。
// - reply は “待っている sender(=reply_waiter)” に対して deliver する。
// - Dead partner を待つ reply_waiter は永遠待ちになるため、kill 側で救済する（mod.rs 側で実施）。
//
// ★fastpath/slowpath 分離 + counters:
// - send/recv の fastpath/slowpath でカウンタを増やす（ログ量は増やさない）
//
// ★Step1:
// - Kernel task の IPC 参加を入口で禁止（endpoint に触らない）
//
// ★Step2:
// - Endpoint の “close” を導入する（owner が死んだら close）。
// - close 時に waiters を READY に戻し、last_reply にエラーを入れる（永遠待ち防止）。
// - open/closed は endpoint の仕様として扱い、invariant でも検知する。

use super::{
    trace, BlockedReason, EndpointId, KernelState, LogEvent, TaskId, TaskState, IPC_DEMO_EP0, MAX_ENDPOINTS, MAX_TASKS,
};
use super::AddressSpaceKind;

/// reply エラーコード（Dead partner を待っていた等）
pub const IPC_ERR_DEAD_PARTNER: u64 = 0xDEAD_DEAD_DEAD_DEAD;

/// endpoint close エラーコード（owner dead 等）
pub const IPC_ERR_ENDPOINT_CLOSED: u64 = 0xC105_ED00_C105_ED00;

/// Endpoint（reply_queue 版）
#[derive(Clone, Copy)]
pub struct Endpoint {
    pub id: EndpointId,

    /// Step2: owner（このタスクが死んだら endpoint は close される）
    pub owner: Option<TaskId>,

    /// Step2: close フラグ（closed の endpoint では send/recv/reply しない）
    pub is_closed: bool,

    /// “受信待ち” は単独 waiter（prototype）
    pub recv_waiter: Option<usize>,

    /// “送信待ち” キュー
    pub send_queue: [usize; MAX_TASKS],
    pub sq_len: usize,

    /// “返信待ち” キュー（blocked_reason で partner を識別）
    pub reply_queue: [usize; MAX_TASKS],
    pub rq_len: usize,
}

impl Endpoint {
    pub const fn new(id: EndpointId) -> Self {
        Endpoint {
            id,
            owner: None,
            is_closed: false,
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

impl KernelState {
    /// 指定タスクが Kernel address space かどうか（IPC の方針判断用）
    fn is_kernel_task_index(&self, idx: usize) -> bool {
        if idx >= self.num_tasks {
            return false;
        }
        let as_idx = self.tasks[idx].address_space_id.0;
        if as_idx >= self.num_tasks {
            return false;
        }
        self.address_spaces[as_idx].kind == AddressSpaceKind::Kernel
    }

    /// Step1: Kernel task の IPC を入口で禁止（endpoint に触らない）
    fn reject_ipc_if_kernel_current(&mut self, api_name: &'static str, ep: EndpointId) -> bool {
        let idx = self.current_task;
        if idx >= self.num_tasks {
            return true;
        }
        if self.tasks[idx].state == TaskState::Dead {
            return true;
        }

        if self.is_kernel_task_index(idx) {
            let tid = self.tasks[idx].id;
            crate::logging::error("ipc: kernel task is forbidden to call IPC (rejected at entry)");
            crate::logging::info(api_name);
            crate::logging::info_u64("task_id", tid.0);
            crate::logging::info_u64("ep_id", ep.0 as u64);

            // 最小のエラー返し
            self.tasks[idx].last_reply = Some(IPC_ERR_DEAD_PARTNER);
            return true;
        }

        false
    }

    /// Step2: endpoint が closed なら、入口で拒否（状態は壊さない）
    fn reject_ipc_if_endpoint_closed(&mut self, api_name: &'static str, ep: EndpointId) -> bool {
        if ep.0 >= MAX_ENDPOINTS {
            return true;
        }
        if self.endpoints[ep.0].is_closed {
            let idx = self.current_task;
            if idx < self.num_tasks && self.tasks[idx].state != TaskState::Dead {
                let tid = self.tasks[idx].id;
                crate::logging::error("ipc: endpoint is CLOSED (rejected at entry)");
                crate::logging::info(api_name);
                crate::logging::info_u64("task_id", tid.0);
                crate::logging::info_u64("ep_id", ep.0 as u64);
                self.tasks[idx].last_reply = Some(IPC_ERR_ENDPOINT_CLOSED);
            }
            return true;
        }
        false
    }

    /// Step2: endpoint を close し、待ちタスクを rescue する
    pub(super) fn close_endpoint_and_rescue_waiters(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

        // close マーク（再入を避ける）
        if self.endpoints[ep.0].is_closed {
            return;
        }
        self.endpoints[ep.0].is_closed = true;

        crate::logging::error("ipc: endpoint CLOSED; rescuing waiters");
        crate::logging::info_u64("ep_id", ep.0 as u64);

        // 1) recv_waiter rescue
        if let Some(recv_idx) = self.endpoints[ep.0].recv_waiter.take() {
            if recv_idx < self.num_tasks && self.tasks[recv_idx].state != TaskState::Dead {
                self.tasks[recv_idx].blocked_reason = None;
                self.tasks[recv_idx].last_reply = Some(IPC_ERR_ENDPOINT_CLOSED);
                self.wake_task_to_ready(recv_idx);
            }
        }

        // 2) send_queue rescue（全員）
        while self.endpoints[ep.0].sq_len > 0 {
            let last = self.endpoints[ep.0].sq_len - 1;
            let send_idx = self.endpoints[ep.0].send_queue[last];
            self.endpoints[ep.0].sq_len -= 1;

            if send_idx < self.num_tasks && self.tasks[send_idx].state != TaskState::Dead {
                self.tasks[send_idx].pending_send_msg = None;
                self.tasks[send_idx].blocked_reason = None;
                self.tasks[send_idx].last_reply = Some(IPC_ERR_ENDPOINT_CLOSED);
                self.wake_task_to_ready(send_idx);
            }
        }

        // 3) reply_queue rescue（全員）
        while self.endpoints[ep.0].rq_len > 0 {
            let last = self.endpoints[ep.0].rq_len - 1;
            let widx = self.endpoints[ep.0].reply_queue[last];
            self.endpoints[ep.0].rq_len -= 1;

            if widx < self.num_tasks && self.tasks[widx].state != TaskState::Dead {
                self.tasks[widx].blocked_reason = None;
                self.tasks[widx].last_reply = Some(IPC_ERR_ENDPOINT_CLOSED);
                self.wake_task_to_ready(widx);
            }
        }
    }

    /// reply_queue から「partner を待っている waiter」を 1つ取り出す
    fn take_reply_waiter_for_partner(&mut self, ep: EndpointId, partner: TaskId) -> Option<usize> {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc: take_reply_waiter_for_partner: ep out of range");
            return None;
        }

        let e = &mut self.endpoints[ep.0];

        for pos in (0..e.rq_len).rev() {
            let idx = e.reply_queue[pos];
            if idx >= self.num_tasks {
                crate::logging::error("ipc: reply_queue contains out-of-range task idx");
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

    // -------------------------------------------------------------------------
    // recv (fastpath/slowpath)
    // -------------------------------------------------------------------------

    fn ipc_recv_fastpath(&mut self, ep: EndpointId, recv_idx: usize) -> bool {
        let send_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.dequeue_sender()
        };

        let send_idx = match send_idx_opt {
            Some(i) => i,
            None => return false,
        };

        if send_idx >= self.num_tasks {
            crate::logging::error("ipc_recv_fastpath: dequeued sender idx out of range");
            return false;
        }
        if self.tasks[send_idx].state == TaskState::Dead {
            crate::logging::error("ipc_recv_fastpath: dequeued sender is DEAD; abort deliver");
            return false;
        }

        let msg = match self.tasks[send_idx].pending_send_msg.take() {
            Some(m) => m,
            None => {
                crate::logging::error("ipc_recv_fastpath: sender had no pending_send_msg; abort deliver");
                return false;
            }
        };

        let recv_id = self.tasks[recv_idx].id;
        let send_id = self.tasks[send_idx].id;

        // sender -> reply wait
        self.tasks[send_idx].state = TaskState::Blocked;
        self.tasks[send_idx].blocked_reason = Some(BlockedReason::IpcReply { partner: recv_id, ep });
        self.tasks[send_idx].time_slice_used = 0;

        {
            let e = &mut self.endpoints[ep.0];
            e.enqueue_reply_waiter(send_idx);
        }

        self.tasks[recv_idx].last_msg = Some(msg);

        if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_msgs_delivered < 2 {
            self.demo_msgs_delivered += 1;
        }

        self.counters.ipc_recv_fast += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::RecvFast);

        self.push_event(LogEvent::IpcDelivered { from: send_id, to: recv_id, ep, msg });
        true
    }

    fn ipc_recv_slowpath(&mut self, ep: EndpointId, recv_idx: usize) {
        let recv_id = self.tasks[recv_idx].id;

        if self.endpoints[ep.0].recv_waiter.is_some() {
            crate::logging::error("ipc_recv_slowpath: recv_waiter already exists; recv rejected (prototype)");
            return;
        }

        self.counters.ipc_recv_slow += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::RecvSlow);

        self.block_current(BlockedReason::IpcRecv { ep });
        self.endpoints[ep.0].recv_waiter = Some(recv_idx);

        self.push_event(LogEvent::IpcRecvBlocked { task: recv_id, ep });
        self.schedule_next_task();
    }

    pub(super) fn ipc_recv(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc_recv: ep out of range");
            return;
        }
        if self.reject_ipc_if_kernel_current("api=ipc_recv", ep) {
            return;
        }
        if self.reject_ipc_if_endpoint_closed("api=ipc_recv", ep) {
            return;
        }

        let recv_idx = self.current_task;
        if recv_idx >= self.num_tasks {
            crate::logging::error("ipc_recv: current_task out of range");
            return;
        }
        if self.tasks[recv_idx].state == TaskState::Dead {
            return;
        }

        let recv_id = self.tasks[recv_idx].id;
        self.push_event(LogEvent::IpcRecvCalled { task: recv_id, ep });

        if self.ipc_recv_fastpath(ep, recv_idx) {
            return;
        }

        self.ipc_recv_slowpath(ep, recv_idx);
    }

    // -------------------------------------------------------------------------
    // send (fastpath/slowpath)
    // -------------------------------------------------------------------------

    fn ipc_send_fastpath(&mut self, ep: EndpointId, send_idx: usize, msg: u64) -> bool {
        let recv_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.recv_waiter.take()
        };

        let recv_idx = match recv_idx_opt {
            Some(i) => i,
            None => return false,
        };

        if recv_idx >= self.num_tasks {
            crate::logging::error("ipc_send_fastpath: recv_waiter idx out of range");
            return false;
        }
        if self.tasks[recv_idx].state == TaskState::Dead {
            crate::logging::error("ipc_send_fastpath: recv_waiter is DEAD; abort deliver");
            return false;
        }

        match self.tasks[recv_idx].blocked_reason {
            Some(BlockedReason::IpcRecv { ep: rep }) if rep == ep => {}
            _ => {
                crate::logging::error("ipc_send_fastpath: recv_waiter blocked_reason mismatch; abort deliver");
                return false;
            }
        }

        let send_id = self.tasks[send_idx].id;
        let recv_id = self.tasks[recv_idx].id;

        self.wake_task_to_ready(recv_idx);
        self.tasks[recv_idx].last_msg = Some(msg);

        self.block_current(BlockedReason::IpcReply { partner: recv_id, ep });
        {
            let e = &mut self.endpoints[ep.0];
            e.enqueue_reply_waiter(send_idx);
        }

        if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_msgs_delivered < 2 {
            self.demo_msgs_delivered += 1;
        }

        self.counters.ipc_send_fast += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::SendFast);

        self.push_event(LogEvent::IpcDelivered { from: send_id, to: recv_id, ep, msg });

        self.schedule_next_task();
        true
    }

    fn ipc_send_slowpath(&mut self, ep: EndpointId, send_idx: usize, msg: u64) {
        let send_id = self.tasks[send_idx].id;

        self.counters.ipc_send_slow += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::SendSlow);

        self.tasks[send_idx].pending_send_msg = Some(msg);

        self.block_current(BlockedReason::IpcSend { ep });
        {
            let e = &mut self.endpoints[ep.0];
            e.enqueue_sender(send_idx);
        }

        self.push_event(LogEvent::IpcSendBlocked { task: send_id, ep });
        self.schedule_next_task();
    }

    pub(super) fn ipc_send(&mut self, ep: EndpointId, msg: u64) {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc_send: ep out of range");
            return;
        }
        if self.reject_ipc_if_kernel_current("api=ipc_send", ep) {
            return;
        }
        if self.reject_ipc_if_endpoint_closed("api=ipc_send", ep) {
            return;
        }

        let send_idx = self.current_task;
        if send_idx >= self.num_tasks {
            crate::logging::error("ipc_send: current_task out of range");
            return;
        }
        if self.tasks[send_idx].state == TaskState::Dead {
            return;
        }

        let send_id = self.tasks[send_idx].id;
        self.push_event(LogEvent::IpcSendCalled { task: send_id, ep, msg });

        if self.ipc_send_fastpath(ep, send_idx, msg) {
            return;
        }

        self.ipc_send_slowpath(ep, send_idx, msg);
    }

    // -------------------------------------------------------------------------
    // reply
    // -------------------------------------------------------------------------

    pub(super) fn ipc_reply(&mut self, ep: EndpointId, msg: u64) {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc_reply: ep out of range");
            return;
        }
        if self.reject_ipc_if_kernel_current("api=ipc_reply", ep) {
            return;
        }
        if self.reject_ipc_if_endpoint_closed("api=ipc_reply", ep) {
            return;
        }

        let recv_idx = self.current_task;
        if recv_idx >= self.num_tasks {
            crate::logging::error("ipc_reply: current_task out of range");
            return;
        }
        if self.tasks[recv_idx].state == TaskState::Dead {
            return;
        }

        let recv_id = self.tasks[recv_idx].id;

        let send_idx = match self.take_reply_waiter_for_partner(ep, recv_id) {
            Some(i) => i,
            None => {
                trace::trace_ipc_path(trace::IpcPathEvent::ReplyNoWaiter);
                return;
            }
        };

        if send_idx >= self.num_tasks {
            crate::logging::error("ipc_reply: reply_waiter idx out of range");
            return;
        }
        if self.tasks[send_idx].state == TaskState::Dead {
            crate::logging::error("ipc_reply: reply_waiter is DEAD; abort");
            return;
        }

        match self.tasks[send_idx].blocked_reason {
            Some(BlockedReason::IpcReply { partner, ep: pep }) if partner == recv_id && pep == ep => {}
            _ => {
                crate::logging::error("ipc_reply: reply_waiter blocked_reason mismatch; abort");
                return;
            }
        }

        let send_id = self.tasks[send_idx].id;

        self.push_event(LogEvent::IpcReplyCalled { task: recv_id, ep, to: send_id });

        self.tasks[send_idx].last_reply = Some(msg);
        self.wake_task_to_ready(send_idx);

        if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_replies_sent < 2 {
            self.demo_replies_sent += 1;
        }

        self.counters.ipc_reply_delivered += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::ReplyDelivered);

        self.push_event(LogEvent::IpcReplyDelivered { from: recv_id, to: send_id, ep });
    }
}
