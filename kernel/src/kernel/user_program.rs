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
        // Task0: driver
        // ------------------------------------------------------------
        if task_idx == TASK0_INDEX {
            // reply が来てたら観測してクリア
            if let Some(v) = self.tasks[task_idx].last_reply {
                crate::logging::info("ipc_reply_received");
                crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                crate::logging::info_u64("reply", v);
                self.tasks[task_idx].last_reply = None;
            }

            // --------------------------------------------------------
            // feature: ipc_demo_single_slow
            // --------------------------------------------------------
            #[cfg(feature = "ipc_demo_single_slow")]
            {
                // (A) Task0 初回 RUNNING の瞬間に 1 回だけ early send
                if !self.demo_early_sent_by_task0 {
                    self.demo_early_sent_by_task0 = true;

                    let msg: u64 = 0xEEEE_0000_0000_0000u64 ^ (self.tick_count & 0xFFFF);
                    self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                    return;
                }

                // (B) 周期 kick-send は “recv_waiter が居る時だけ” 実施（fast だけに寄せる）
                //
                // これにより、Task0 の周期送信が send_queue に積まれる（slow）可能性を抑える。
                if self.tick_count != 0 && (self.tick_count % Self::IPC_KICK_PERIOD_TICKS) == 0 {
                    // Endpoint0 の recv_waiter が居るかだけを見る（プロトタイプなので簡略）
                    let can_fast_send = self.endpoints[ep.0].recv_waiter.is_some();

                    if can_fast_send {
                        let msg: u64 = 0xD0D0_0000_0000_0000u64 ^ (self.tick_count & 0xFFFF);
                        self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                        return;
                    }
                }

                return;
            }

            // --------------------------------------------------------
            // 通常モード
            // --------------------------------------------------------
            if self.tick_count != 0 && (self.tick_count % Self::IPC_KICK_PERIOD_TICKS) == 0 {
                let msg: u64 = 0xD0D0_0000_0000_0000u64 ^ (self.tick_count & 0xFFFF);
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend { ep, msg });
                return;
            }

            return;
        }

        // ------------------------------------------------------------
        // Task1: 最初の kick send（1回だけ）
        // ------------------------------------------------------------
        if task_idx == TASK1_INDEX {
            // feature モードでは Task1 はノイズ源になりうるので送らない
            #[cfg(feature = "ipc_demo_single_slow")]
            {
                if let Some(v) = self.tasks[task_idx].last_reply {
                    crate::logging::info("ipc_reply_received");
                    crate::logging::info_u64("task_id", self.tasks[task_idx].id.0);
                    crate::logging::info_u64("reply", v);
                    self.tasks[task_idx].last_reply = None;
                }
                return;
            }

            // 通常モード：最初の kick send
            if !self.demo_sent_by_task1 {
                self.demo_sent_by_task1 = true;
                let msg: u64 = 0x1111_0000_0000_0000u64 ^ (self.tick_count & 0xFFFF);
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

            // まだ受信していないなら recv
            self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
            return;
        }

        // ------------------------------------------------------------
        // それ以外
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

        // まだ受信していないなら recv
        self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
    }
}
