// kernel/src/kernel/user_program.rs
//
// 役割:
// - IPC(send/recv/reply) が「最低1回は成立する」ことを最短で確認するデモ。
// - 循環条件で send が一度も起きない（recv_waiter待ち）を避けるため、最初の send を強制する。
//
// やること:
// - Task1 と Task0 が、それぞれ 1 回だけ IpcSend を発行して “kick” する。
// - それ以外（Task3想定）は IpcRecv を待ち、受信したら IpcReply を返す。
// - 返信先は Syscall::IpcReply に to が無い設計なので、kernel 側の「reply_waiter/partner」解決に任せる。
//
// やらないこと:
// - 送信元追跡の厳密化や多段プロトコルはしない（まず成立優先）。
//

use crate::kernel::{
    EndpointId, KernelState, Syscall, TaskState, IPC_DEMO_EP0, TASK0_INDEX, TASK1_INDEX, TASK2_INDEX,
};

impl KernelState {
    /// 各 tick の終盤で「ユーザ側が発行したい syscall」をセットする。
    pub fn user_step_issue_syscall(&mut self, task_idx: usize) {
        if task_idx >= self.num_tasks {
            return;
        }

        // DEAD には何もしない
        if self.tasks[task_idx].state == TaskState::Dead {
            return;
        }

        // 既に syscall が積まれているなら触らない
        if self.tasks[task_idx].pending_syscall.is_some() {
            return;
        }

        // Blocked 中は kernel 側の wake を待つ（勝手に追加 syscall しない）
        if self.tasks[task_idx].state == TaskState::Blocked {
            return;
        }

        let ep: EndpointId = IPC_DEMO_EP0;

        // ------------------------------------------------------------
        // Task1: 最初の kick send（1回だけ）
        // ------------------------------------------------------------
        if task_idx == TASK1_INDEX {
            if !self.demo_sent_by_task1 {
                self.demo_sent_by_task1 = true;
                let msg: u64 = 0x1111_0000_0000_0000 ^ (self.tick_count & 0xFFFF);
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                return;
            }

            // reply が来てたら観測してクリア
            if let Some(v) = self.tasks[task_idx].last_reply {
                crate::logging::info("ipc_reply_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("reply", v);
                self.tasks[task_idx].last_reply = None;
            }
            return;
        }

        // ------------------------------------------------------------
        // Task0: 2回目の kick send（1回だけ）
        // ------------------------------------------------------------
        if task_idx == TASK0_INDEX {
            if !self.demo_sent_by_task2 {
                self.demo_sent_by_task2 = true;
                let msg: u64 = 0x2222_0000_0000_0000 ^ (self.tick_count & 0xFFFF);
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                return;
            }

            if let Some(v) = self.tasks[task_idx].last_reply {
                crate::logging::info("ipc_reply_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("reply", v);
                self.tasks[task_idx].last_reply = None;
            }
            return;
        }

        // ------------------------------------------------------------
        // Task2 (task_index=2 / task_id=3): IPC server (recv -> reply)
        // ------------------------------------------------------------
        if task_idx == TASK2_INDEX {
            let ep: EndpointId = IPC_DEMO_EP0;

            if let Some(msg) = self.tasks[task_idx].last_msg {
                crate::logging::info("ipc_msg_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("msg", msg);

                let reply: u64 = 0xABCD_0000_0000_0000 ^ (msg & 0xFFFF);

                self.tasks[task_idx].last_msg = None;
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcReply { ep, msg: reply });
                return;
            }

            // まだ受信していないなら recv
            self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
            return;
        }


        // ------------------------------------------------------------
        // それ以外（Task3想定）: recv → reply
        // ------------------------------------------------------------
        if let Some(msg) = self.tasks[task_idx].last_msg {
            crate::logging::info("ipc_msg_received");
            crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
            crate::logging::info_u64("msg", msg);

            // デモ返信（送信元の特定は kernel 側の reply_waiter 解決に任せる）
            let reply: u64 = 0xABCD_0000_0000_0000 ^ (msg & 0xFFFF);

            self.tasks[task_idx].last_msg = None;
            self.tasks[task_idx].pending_syscall = Some(Syscall::IpcReply { ep, msg: reply });
            return;
        }

        // まだ受信していないなら recv
        self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
    }
}
