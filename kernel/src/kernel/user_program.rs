// kernel/src/kernel/user_program.rs
//
// user program（デモ）
// - Running task が syscall を発行する。
// - evil_ipc では「不正ep」を混ぜ、カーネルが安全に拒否/無視できることを観測する。
// - pf_demo では「未マップ user VA」へアクセスして #PF を観測する（fail-stop）。

use super::{
    EndpointId, KernelState, Syscall, TaskState, IPC_DEMO_EP0, TASK0_INDEX, TASK1_INDEX, TASK2_INDEX,
};

impl KernelState {
    pub(super) fn user_step_issue_syscall(&mut self, task_idx: usize) {
        if task_idx >= self.num_tasks {
            return;
        }
        if self.tasks[task_idx].state != TaskState::Running {
            return;
        }
        if self.tasks[task_idx].pending_syscall.is_some() {
            return;
        }

        // pf_demo: 「User AS が RUNNING になった最初の1回だけ」必ず #PF を起こす
        #[cfg(feature = "pf_demo")]
        {
            let as_idx = self.tasks[task_idx].address_space_id.0;

            if !self.pf_demo_done && self.address_spaces[as_idx].kind == super::AddressSpaceKind::User {
                self.pf_demo_done = true;

                crate::logging::error("pf_demo: forcing a page fault by reading unmapped user VA");

                // mem_demo の mapped page と被りにくい位置
                let unmapped = (crate::arch::paging::USER_SPACE_BASE + 0x4000) as *const u64;

                unsafe {
                    // ここで #PF → handler が serial に出して halt
                    let _ = core::ptr::read_volatile(unmapped);
                }
            }
        }

        // evil_ipc: たまに“不正ep”を投げる（カーネルが安全に拒否/無視できることを確認する）
        #[cfg(feature = "evil_ipc")]
        {
            if task_idx == TASK0_INDEX && (self.tick_count % 13 == 0) {
                self.tasks[task_idx].pending_syscall =
                    Some(Syscall::IpcReply { ep: EndpointId(999) });
                crate::logging::info("evil_ipc: issued IpcReply to invalid ep (expect safe reject)");
                return;
            }
        }

        let ep = IPC_DEMO_EP0;

        if task_idx == TASK2_INDEX {
            if self.demo_msgs_delivered < 2 {
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcRecv { ep });
                return;
            }
            if self.demo_replies_sent < 2 {
                self.tasks[task_idx].pending_syscall = Some(Syscall::IpcReply { ep });
                return;
            }

            self.demo_msgs_delivered = 0;
            self.demo_replies_sent = 0;
            self.demo_sent_by_task2 = false;
            self.demo_sent_by_task1 = false;
            self.tasks[TASK2_INDEX].last_msg = None;

            crate::logging::info("user_program: demo cycle reset");
            return;
        }

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
