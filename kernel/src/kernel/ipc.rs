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
//
// ★安全性の追加（今回）:
// - キュー満杯時は “block させない/救済する” を徹底（永久待ち防止）
// - 壊れた待ち要素（Dead / blocked_reason mismatch / pending_send_msg None 等）は掃除して救済
// - recv_waiter が既にいる prototype 制限は明示エラーで返す（無限スピン抑制）

use super::{
    trace, AddressSpaceKind, BlockedReason, EndpointId, KernelState, LogEvent, TaskId, TaskState, IPC_DEMO_EP0,
    MAX_ENDPOINTS, MAX_TASKS,
};

/// reply エラーコード（Dead partner を待っていた等）
pub const IPC_ERR_DEAD_PARTNER: u64 = 0xDEAD_DEAD_DEAD_DEAD;

/// endpoint close エラーコード（owner dead 等）
pub const IPC_ERR_ENDPOINT_CLOSED: u64 = 0xC105_ED00_C105_ED00;

/// キュー満杯などの capacity エラー
pub const IPC_ERR_CAPACITY: u64 = 0xC0DE_C0DE_C0DE_C0DE;

/// prototype 制限: recv_waiter が既に存在
pub const IPC_ERR_RECV_ALREADY_WAITING: u64 = 0xBADC_0FFE_BADC_0FFE;

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

    /// ★追加: enqueue が可能か（満杯なら false）
    fn try_enqueue_sender(&mut self, idx: usize) -> bool {
        if self.sq_len >= MAX_TASKS {
            return false;
        }
        if self.send_queue_contains(idx) {
            return true;
        }
        self.send_queue[self.sq_len] = idx;
        self.sq_len += 1;
        true
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

    /// ★追加: enqueue が可能か（満杯なら false）
    fn try_enqueue_reply_waiter(&mut self, idx: usize) -> bool {
        if self.rq_len >= MAX_TASKS {
            return false;
        }
        if self.reply_queue_contains(idx) {
            return true;
        }
        self.reply_queue[self.rq_len] = idx;
        self.rq_len += 1;
        true
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

    /// ★追加: send_queue から特定 idx を 1つ除去（swap-remove）
    fn remove_sender_idx(&mut self, idx: usize) -> bool {
        let mut pos = 0;
        while pos < self.sq_len {
            if self.send_queue[pos] == idx {
                let last = self.sq_len - 1;
                self.send_queue[pos] = self.send_queue[last];
                self.sq_len -= 1;
                return true;
            }
            pos += 1;
        }
        false
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

    /// ★追加: 現在タスクを “エラーで救済” して READY へ戻す（永久待ち防止）
    fn rescue_current_with_error(&mut self, err: u64) {
        let idx = self.current_task;
        if idx >= self.num_tasks {
            return;
        }
        if self.tasks[idx].state == TaskState::Dead {
            return;
        }

        self.tasks[idx].pending_send_msg = None;
        self.tasks[idx].blocked_reason = None;
        self.tasks[idx].last_reply = Some(err);

        // Blocked のまま終えない
        if self.tasks[idx].state == TaskState::Blocked {
            self.wake_task_to_ready(idx);
        }
    }

    /// ★追加: 指定タスクを “エラーで救済” して READY へ戻す（永久待ち防止）
    fn rescue_task_with_error(&mut self, idx: usize, err: u64) {
        if idx >= self.num_tasks {
            return;
        }
        if self.tasks[idx].state == TaskState::Dead {
            return;
        }

        self.tasks[idx].pending_send_msg = None;
        self.tasks[idx].blocked_reason = None;
        self.tasks[idx].last_reply = Some(err);
        self.wake_task_to_ready(idx);
    }

    /// Step2: endpoint を close し、待ちタスクを rescue する
    pub(super) fn close_endpoint_and_rescue_waiters(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            return;
        }

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

        // 2) send_queue rescue
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

        // 3) reply_queue rescue
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
    /// ★追加: 探索中に壊れた要素は掃除して詰まりを防ぐ
    fn take_reply_waiter_for_partner(&mut self, ep: EndpointId, partner: TaskId) -> Option<usize> {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc: take_reply_waiter_for_partner: ep out of range");
            return None;
        }

        // ここで “救済すべき waiter” を一時退避して、endpoint への &mut 借用を先に解放する
        let mut to_rescue: Option<usize> = None;

        {
            let e = &mut self.endpoints[ep.0];

            // 後ろから見る（swap-remove との相性が良い）
            let mut pos = e.rq_len;
            while pos > 0 {
                pos -= 1;

                let idx = e.reply_queue[pos];

                if idx >= self.num_tasks {
                    crate::logging::error("ipc: reply_queue contains out-of-range task idx; drop");
                    let _ = e.remove_reply_waiter_at(pos);
                    continue;
                }
                if self.tasks[idx].state == TaskState::Dead {
                    crate::logging::error("ipc: reply_queue contains DEAD task; drop");
                    crate::logging::info_u64("task_id", self.tasks[idx].id.0);
                    let _ = e.remove_reply_waiter_at(pos);
                    continue;
                }

                match self.tasks[idx].blocked_reason {
                    Some(BlockedReason::IpcReply { partner: p, ep: pep }) if p == partner && pep == ep => {
                        // 期待ケース：この waiter を取り出して返す
                        return e.remove_reply_waiter_at(pos);
                    }

                    _ => {
                        // mismatch は “壊れている可能性が高い” ので掃除する（永久待ちの種になる）
                        crate::logging::error("ipc: reply_queue blocked_reason mismatch; drop (will rescue)");
                        crate::logging::info_u64("task_id", self.tasks[idx].id.0);

                        let removed = e.remove_reply_waiter_at(pos);
                        if removed.is_some() && to_rescue.is_none() {
                            to_rescue = removed;
                        }
                        continue;
                    }
                }
            }
        } // ← ここで e の &mut borrow が確実に解放される

        // endpoint から外した waiter を rescue（&mut self が使える）
        if let Some(widx) = to_rescue {
            self.rescue_task_with_error(widx, IPC_ERR_DEAD_PARTNER);
        }

        None
    }

    // -------------------------------------------------------------------------
    // recv (fastpath/slowpath)
    // -------------------------------------------------------------------------

    fn ipc_recv_fastpath(&mut self, ep: EndpointId, recv_idx: usize) -> bool {
        // sender を取り出す。壊れた要素（state/blocked_reason 不整合）は捨てて次を試す。
        let send_idx = loop {
            let send_idx_opt = {
                let e = &mut self.endpoints[ep.0];
                e.dequeue_sender()
            };

            let idx = match send_idx_opt {
                Some(i) => i,
                None => return false,
            };

            if idx >= self.num_tasks {
                crate::logging::error("ipc_recv_fastpath: dequeued sender idx out of range; drop");
                continue;
            }
            if self.tasks[idx].state == TaskState::Dead {
                crate::logging::error("ipc_recv_fastpath: dequeued sender is DEAD; drop");
                continue;
            }

            // send_queue に居る sender は Blocked(IpcSend) のはず
            match self.tasks[idx].blocked_reason {
                Some(BlockedReason::IpcSend { ep: sep }) if sep == ep => {
                    if self.tasks[idx].state != TaskState::Blocked {
                        crate::logging::error("ipc_recv_fastpath: sender state is not BLOCKED; drop");
                        crate::logging::info_u64("task_id", self.tasks[idx].id.0);
                        continue;
                    }
                    break idx;
                }
                _ => {
                    crate::logging::error("ipc_recv_fastpath: sender blocked_reason mismatch; drop");
                    crate::logging::info_u64("task_id", self.tasks[idx].id.0);
                    continue;
                }
            }
        };

        // ★重要: pending_send_msg が無い sender は救済して次へ（永久待ち防止）
        let msg = match self.tasks[send_idx].pending_send_msg.take() {
            Some(m) => m,
            None => {
                crate::logging::error("ipc_recv_fastpath: sender had no pending_send_msg; rescue+continue");
                let sid = self.tasks[send_idx].id;
                crate::logging::info_u64("sender_task_id", sid.0);

                // sender は send_queue から既に外れているので、ここで rescue しないと詰む
                self.rescue_task_with_error(send_idx, IPC_ERR_DEAD_PARTNER);
                return false;
            }
        };

        let send_id = self.tasks[send_idx].id;
        let recv_id = self.tasks[recv_idx].id;

        // sender -> reply wait
        // ★reply_queue 満杯なら block させない（永久待ち防止）
        let ok = {
            let e = &mut self.endpoints[ep.0];
            e.try_enqueue_reply_waiter(send_idx)
        };
        if !ok {
            crate::logging::error("ipc_recv_fastpath: reply_queue full; rescue sender");
            crate::logging::info_u64("sender_task_id", send_id.0);
            self.rescue_task_with_error(send_idx, IPC_ERR_CAPACITY);

            // receiver 側は msg を受け取らない（deliver しない）
            return false;
        }

        self.block_task(send_idx, BlockedReason::IpcReply { partner: recv_id, ep });

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
            // ★明示エラー（無限スピン抑制）
            self.tasks[recv_idx].last_reply = Some(IPC_ERR_RECV_ALREADY_WAITING);
            return;
        }

        self.counters.ipc_recv_slow += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::RecvSlow);

        self.block_task(recv_idx, BlockedReason::IpcRecv { ep });
        self.endpoints[ep.0].recv_waiter = Some(recv_idx);

        self.push_event(LogEvent::IpcRecvBlocked { task: recv_id, ep });

        // ★FIX: ring3_mailbox だけ抑制。ring3_mailbox_loop は schedule 必須。
        #[cfg(not(feature = "ring3_mailbox"))]
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
        if send_idx != self.current_task {
            crate::logging::error("ipc_send_fastpath: send_idx != current_task; reject");
            crate::logging::info_u64("send_idx", send_idx as u64);
            crate::logging::info_u64("current_task", self.current_task as u64);
            return false;
        }

        let recv_idx = match self.endpoints[ep.0].recv_waiter {
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

        // OKなら消費
        let _ = self.endpoints[ep.0].recv_waiter.take();

        let send_id = self.tasks[send_idx].id;
        let recv_id = self.tasks[recv_idx].id;

        // receiver を READY へ
        self.wake_task_to_ready(recv_idx);
        self.tasks[recv_idx].last_msg = Some(msg);

        // sender は reply wait
        // ★reply_queue 満杯なら block させない（永久待ち防止）
        let ok = {
            let e = &mut self.endpoints[ep.0];
            e.try_enqueue_reply_waiter(send_idx)
        };
        if !ok {
            crate::logging::error("ipc_send_fastpath: reply_queue full; rescue sender");
            crate::logging::info_u64("task_id", send_id.0);
            self.tasks[send_idx].last_reply = Some(IPC_ERR_CAPACITY);
            return true; // deliver は成立させた（recv は起こして msg を渡した）
        }

        self.block_task(send_idx, BlockedReason::IpcReply { partner: recv_id, ep });

        if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_msgs_delivered < 2 {
            self.demo_msgs_delivered += 1;
        }

        self.counters.ipc_send_fast += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::SendFast);

        self.push_event(LogEvent::IpcDelivered { from: send_id, to: recv_id, ep, msg });

        // ★重要: ring3_mailbox_loop では schedule 必須（current_task が Blocked のまま tick を終えない）
        #[cfg(feature = "ring3_mailbox_loop")]
        self.schedule_next_task();

        // ring3_mailbox（単発）は schedule しない（CR3切替を避ける目的）
        #[cfg(all(feature = "ring3_mailbox", not(feature = "ring3_mailbox_loop")))]
        trace::trace_ipc_path(trace::IpcPathEvent::SendFast);

        // それ以外は通常通り schedule
        #[cfg(not(any(feature = "ring3_mailbox", feature = "ring3_mailbox_loop")))]
        self.schedule_next_task();

        true
    }

    fn ipc_send_slowpath(&mut self, ep: EndpointId, send_idx: usize, msg: u64) {
        if send_idx != self.current_task {
            crate::logging::error("ipc_send_slowpath: send_idx != current_task; reject");
            crate::logging::info_u64("send_idx", send_idx as u64);
            crate::logging::info_u64("current_task", self.current_task as u64);
            return;
        }

        let send_id = self.tasks[send_idx].id;

        self.counters.ipc_send_slow += 1;
        trace::trace_ipc_path(trace::IpcPathEvent::SendSlow);

        // ★キュー満杯なら block しない（永久待ち防止）
        let ok = {
            let e = &mut self.endpoints[ep.0];
            e.try_enqueue_sender(send_idx)
        };
        if !ok {
            crate::logging::error("ipc_send_slowpath: send_queue full; reject");
            crate::logging::info_u64("task_id", send_id.0);
            self.tasks[send_idx].last_reply = Some(IPC_ERR_CAPACITY);
            return;
        }

        // enqueue が成功した後に状態を作る（順序重要）
        self.tasks[send_idx].pending_send_msg = Some(msg);
        self.block_task(send_idx, BlockedReason::IpcSend { ep });

        self.push_event(LogEvent::IpcSendBlocked { task: send_id, ep });

        // ★重要: ring3_mailbox_loop では schedule 必須
        #[cfg(feature = "ring3_mailbox_loop")]
        self.schedule_next_task();

        // ring3_mailbox（単発）は schedule しない
        #[cfg(all(feature = "ring3_mailbox", not(feature = "ring3_mailbox_loop")))]
        trace::trace_ipc_path(trace::IpcPathEvent::SendSlow);

        // それ以外は通常通り schedule
        #[cfg(not(any(feature = "ring3_mailbox", feature = "ring3_mailbox_loop")))]
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
                crate::logging::error("ipc_reply: reply_waiter blocked_reason mismatch; abort+rescue");
                self.rescue_task_with_error(send_idx, IPC_ERR_DEAD_PARTNER);
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
