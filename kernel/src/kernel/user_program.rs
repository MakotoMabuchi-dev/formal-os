// kernel/src/kernel/user_program.rs
//
// user program（デモ）
// - “IpcDemo” の代替として、Running task が自分の意思で syscall を発行する。
// - ここは「カーネル内部のデモ用状態機械」であり、後でユーザ空間ができたら置き換える想定。

use super::{
    KernelState, Syscall,
    IPC_DEMO_EP0, TASK0_INDEX, TASK1_INDEX, TASK2_INDEX,
};

impl KernelState {
    //
    // user program（Running task が syscall を発行する）
    //
    pub(super) fn user_step_issue_syscall(&mut self, task_idx: usize) {
        if task_idx >= self.num_tasks {
            return;
        }
        if self.tasks[task_idx].state != super::TaskState::Running {
            return;
        }
        if self.tasks[task_idx].pending_syscall.is_some() {
            // すでに発行済み（まだ処理されていない）なら二重発行しない
            return;
        }

        let ep = IPC_DEMO_EP0;

        // Receiver (Task3 = TASK2_INDEX)
        if task_idx == TASK2_INDEX {
            if self.demo_msgs_delivered < 2 {
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
                return;
            }
            if self.demo_replies_sent < 2 {
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcReply { ep });
                return;
            }

            // 周回終了 → 状態リセット
            self.demo_msgs_delivered = 0;
            self.demo_replies_sent = 0;
            self.demo_sent_by_task2 = false;
            self.demo_sent_by_task1 = false;
            self.tasks[TASK2_INDEX].last_msg = None;

            crate::logging::info("user_program: demo cycle reset");
            return;
        }

        // Sender A (Task2 = TASK1_INDEX): “recv_waiter が立っている(1回目)” のときだけ送る
        if task_idx == TASK1_INDEX {
            if !self.demo_sent_by_task2 {
                let e = &self.endpoints[ep.0];
                if e.recv_waiter == Some(TASK2_INDEX) && self.demo_msgs_delivered == 0 {
                    self.demo_sent_by_task2 = true;
                    self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend {
                        ep,
                        msg: 0x1111_0000_0000_0000u64,
                    });
                    return;
                }
            }
            return;
        }

        // Sender B (Task1 = TASK0_INDEX): “recv_waiter が立っている(2回目)” のときだけ送る
        if task_idx == TASK0_INDEX {
            if !self.demo_sent_by_task1 {
                let e = &self.endpoints[ep.0];
                if e.recv_waiter == Some(TASK2_INDEX) && self.demo_msgs_delivered == 1 {
                    self.demo_sent_by_task1 = true;
                    self.tasks[task_idx].pending_syscall = Some(Syscall::IpcSend {
                        ep,
                        msg: 0x2222_0000_0000_0000u64,
                    });
                    return;
                }
            }
            return;
        }
    }
}
