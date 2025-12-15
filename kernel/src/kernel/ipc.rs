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
// このファイルの役割:
// - Endpoint データ構造と、ipc_recv / ipc_send / ipc_reply の基本遷移を提供する。
// - Dead partner 救済（reply_waiter rescue）は kill_task() 側（kernel/mod.rs）に置く。

use super::{
    BlockedReason, EndpointId, KernelState, LogEvent, TaskId, TaskState, IPC_DEMO_EP0, MAX_ENDPOINTS, MAX_TASKS,
};

/// reply エラーコード（Dead partner を待っていた等）
///
/// NOTE:
/// - user_program は last_reply を見て “reply を受け取った” 扱いにできる。
/// - 値はプロトタイプ用の固定値（フォーマル化時に仕様として固定する）。
pub const IPC_ERR_DEAD_PARTNER: u64 = 0xDEAD_DEAD_DEAD_DEAD;

/// Endpoint（reply_queue 版）
#[derive(Clone, Copy)]
pub struct Endpoint {
    pub id: EndpointId,

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
    /// reply_queue から「partner を待っている waiter」を 1つ取り出す
    fn take_reply_waiter_for_partner(&mut self, ep: EndpointId, partner: TaskId) -> Option<usize> {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc: take_reply_waiter_for_partner: ep out of range");
            return None;
        }

        let e = &mut self.endpoints[ep.0];

        // swap-remove 前提なので逆順でも OK
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

    /// recv:
    /// - sender が待っていれば即 deliver（sender は reply 待ちへ）
    /// - sender がいなければ recv_waiter に登録して Block
    pub(super) fn ipc_recv(&mut self, ep: EndpointId) {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc_recv: ep out of range");
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

        // sender が待っていれば deliver（send_queue から）
        let send_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.dequeue_sender()
        };

        if let Some(send_idx) = send_idx_opt {
            if send_idx >= self.num_tasks {
                crate::logging::error("ipc_recv: dequeued sender idx out of range");
                return;
            }
            if self.tasks[send_idx].state == TaskState::Dead {
                crate::logging::error("ipc_recv: dequeued sender is DEAD; abort deliver");
                return;
            }

            // sender が持つ msg を取り出す（無いなら fail-safe）
            let msg = match self.tasks[send_idx].pending_send_msg.take() {
                Some(m) => m,
                None => {
                    crate::logging::error("ipc_recv: sender had no pending_send_msg; abort deliver");
                    return;
                }
            };

            let send_id = self.tasks[send_idx].id;

            // sender は reply 待ちに遷移（Blocked）
            self.tasks[send_idx].state = TaskState::Blocked;
            self.tasks[send_idx].blocked_reason = Some(BlockedReason::IpcReply { partner: recv_id, ep });
            self.tasks[send_idx].time_slice_used = 0;

            // ★重要(Step2): IPC 待ちは wait_queue に載せない
            // - reply_queue のみに登録する
            {
                let e = &mut self.endpoints[ep.0];
                e.enqueue_reply_waiter(send_idx);
            }

            // receiver へ msg を渡す（receiver は RUNNING のまま）
            self.tasks[recv_idx].last_msg = Some(msg);

            // デモ観測
            if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_msgs_delivered < 2 {
                self.demo_msgs_delivered += 1;
            }

            self.push_event(LogEvent::IpcDelivered { from: send_id, to: recv_id, ep, msg });
            return;
        }

        // sender がいない → recv_waiter に登録して Block
        if self.endpoints[ep.0].recv_waiter.is_some() {
            crate::logging::error("ipc_recv: recv_waiter already exists; recv rejected (prototype)");
            return;
        }

        self.block_current(BlockedReason::IpcRecv { ep });
        self.endpoints[ep.0].recv_waiter = Some(recv_idx);

        self.push_event(LogEvent::IpcRecvBlocked { task: recv_id, ep });
        self.schedule_next_task();
    }

    /// send:
    /// - recv_waiter がいれば即 deliver（sender は reply 待ちへ）
    /// - recv_waiter がいなければ send_queue に積んで Block
    pub(super) fn ipc_send(&mut self, ep: EndpointId, msg: u64) {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc_send: ep out of range");
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

        // recv_waiter がいれば deliver
        let recv_idx_opt = {
            let e = &mut self.endpoints[ep.0];
            e.recv_waiter.take()
        };

        if let Some(recv_idx) = recv_idx_opt {
            if recv_idx >= self.num_tasks {
                crate::logging::error("ipc_send: recv_waiter idx out of range");
                return;
            }
            if self.tasks[recv_idx].state == TaskState::Dead {
                crate::logging::error("ipc_send: recv_waiter is DEAD; abort deliver");
                return;
            }

            // recv_waiter は「BLOCKED + IpcRecv」のはず。崩れていたら fail-safe
            match self.tasks[recv_idx].blocked_reason {
                Some(BlockedReason::IpcRecv { ep: rep }) if rep == ep => {}
                _ => {
                    crate::logging::error("ipc_send: recv_waiter blocked_reason mismatch; abort deliver");
                    return;
                }
            }

            let recv_id = self.tasks[recv_idx].id;

            // receiver を READY に戻す（endpoint 側参照も掃除される想定）
            self.wake_task_to_ready(recv_idx);
            self.tasks[recv_idx].last_msg = Some(msg);

            // sender は reply 待ちに遷移（wait_queue には載せない）
            self.block_current(BlockedReason::IpcReply { partner: recv_id, ep });
            {
                let e = &mut self.endpoints[ep.0];
                e.enqueue_reply_waiter(send_idx);
            }

            // デモ観測
            if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_msgs_delivered < 2 {
                self.demo_msgs_delivered += 1;
            }

            self.push_event(LogEvent::IpcDelivered { from: send_id, to: recv_id, ep, msg });
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

    /// reply:
    /// - current task（receiver）が、reply_waiter（sender）へ返信する。
    /// - msg は “結果/ack” として sender 側の last_reply に格納される。
    pub(super) fn ipc_reply(&mut self, ep: EndpointId, msg: u64) {
        if ep.0 >= MAX_ENDPOINTS {
            crate::logging::error("ipc_reply: ep out of range");
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
                // 返信対象がいないのは “正常系でもあり得る”(まだ届いてない等)
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

        // sender は本来 BLOCKED(IpcReply{partner=recv_id}) のはず。崩れてたら fail-safe
        match self.tasks[send_idx].blocked_reason {
            Some(BlockedReason::IpcReply { partner, ep: pep }) if partner == recv_id && pep == ep => {}
            _ => {
                crate::logging::error("ipc_reply: reply_waiter blocked_reason mismatch; abort");
                return;
            }
        }

        let send_id = self.tasks[send_idx].id;

        // ★LogEvent の定義に合わせる
        // - もし LogEvent::IpcReplyCalled が { task, ep, to } を持つならこのまま
        // - もし { task, ep } だけなら、下の行を to 無し版に変えてください
        self.push_event(LogEvent::IpcReplyCalled { task: recv_id, ep, to: send_id });

        // sender を READY に戻し、返信を渡す
        self.tasks[send_idx].last_reply = Some(msg);
        self.wake_task_to_ready(send_idx);

        // デモ観測
        if ep == IPC_DEMO_EP0 && recv_idx == super::TASK2_INDEX && self.demo_replies_sent < 2 {
            self.demo_replies_sent += 1;
        }

        self.push_event(LogEvent::IpcReplyDelivered { from: recv_id, to: send_id, ep });
    }
}
