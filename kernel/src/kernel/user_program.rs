// kernel/src/kernel/user_program.rs
//
// 役割:
// - IPC(send/recv/reply) が「最低1回は成立する」ことを最短で確認するデモ。
// - 観測性を高めるため、専用 feature で “デモの仕様” を固定できるようにする。
//
// 仕様（通常）:
// - Task1: 最初の kick send（1回だけ）
// - Task0: 周期 kick-send
// - Task2: IPC server (recv -> reply)
//
// 仕様（feature = ipc_demo_single_slow）:
// - 目的: “send_queue 経由の slow send” を 1 回に固定しやすくする
// - Task1: kick send をしない（ノイズ源を除去）
// - Task0:
//   (A) Task0 が最初に RUNNING になった最初の tick に 1 回だけ early send
//   (B) 以後は周期 kick-send だが、recv_waiter が居るときだけ送る（fast のみ）
// - Task2: IPC server (recv -> reply)
//
// 方針:
// - デモは「再現性」が最重要。時刻依存ではなく「Task0 初回実行」等に固定する。
//
// ★Step3（観測性）:
// - syscall 戻り値（mem系）と IPC reply を混線させない。
//   * mem系: last_syscall_ret
//   * IPC  : last_reply

use crate::kernel::{
    EndpointId, KernelState, Syscall, TaskState, IPC_DEMO_EP0, TASK0_INDEX, TASK1_INDEX, TASK2_INDEX,
};

impl KernelState {
    const IPC_KICK_PERIOD_TICKS: u64 = 8;

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
        // Step3: syscall 戻り値（mem系）を観測してクリア（unread のときだけ）
        // - “消費してクリア” はこの1箇所に固定する
        // ------------------------------------------------------------
        if let Some(v) = self.take_unread_last_syscall_ret(task_idx) {
            crate::logging::info("syscall_ret_received");
            crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
            crate::logging::info_u64("ret", v);
        }

        // ------------------------------------------------------------
        // Task0: Kernel task は IPC を発行しない（Step1）
        // ------------------------------------------------------------
        if task_idx == TASK0_INDEX {
            // IPC reply が残っていたら観測して消すだけ（任意）
            if let Some(v) = self.tasks[task_idx].last_reply {
                crate::logging::info("ipc_reply_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("reply", v);
                self.tasks[task_idx].last_reply = None;
            }
            return;
        }

        // ------------------------------------------------------------
        // Task1: kick sender（User）
        // ------------------------------------------------------------
        if task_idx == TASK1_INDEX {
            // IPC reply が来てたら観測してクリア
            if let Some(v) = self.tasks[task_idx].last_reply {
                crate::logging::info("ipc_reply_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("reply", v);
                self.tasks[task_idx].last_reply = None;
            }

            #[cfg(feature = "ipc_demo_single_slow")]
            {
                return;
            }

            // 通常モード：最初に 1 回だけ kick
            if !self.demo_sent_by_task1 {
                self.demo_sent_by_task1 = true;
                let msg: u64 = 0x1111_0000_0000_0000u64 ^ (self.tick_count & 0xFFFF);
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                return;
            }

            // 継続観測用：recv_waiter がいる時だけ fast-send
            if self.tick_count != 0 && (self.tick_count % Self::IPC_KICK_PERIOD_TICKS) == 0 {
                let can_fast_send = self.endpoints[ep.0].recv_waiter.is_some();
                if can_fast_send {
                    let msg: u64 = 0x2222_0000_0000_0000u64 ^ (self.tick_count & 0xFFFF);
                    self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                    return;
                }
            }

            return;
        }

        // ------------------------------------------------------------
        // Task2: IPC server (recv -> reply)
        // ------------------------------------------------------------
        if task_idx == TASK2_INDEX {
            if let Some(msg) = self.tasks[task_idx].last_msg {
                crate::logging::info("ipc_msg_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("msg", msg);

                let reply: u64 = 0xABCD_0000_0000_0000u64 ^ (msg & 0xFFFF);

                self.tasks[task_idx].last_msg = None;
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcReply { ep, msg: reply });
                return;
            }

            self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
            return;
        }

        // ------------------------------------------------------------
        // それ以外（保険）
        // ------------------------------------------------------------
        if let Some(msg) = self.tasks[task_idx].last_msg {
            crate::logging::info("ipc_msg_received");
            crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
            crate::logging::info_u64("msg", msg);

            let reply: u64 = 0xABCD_0000_0000_0000u64 ^ (msg & 0xFFFF);

            self.tasks[task_idx].last_msg = None;
            self.tasks[task_idx].pending_syscall = Some(Syscall::IpcReply { ep, msg: reply });
            return;
        }

        self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
    }
}
