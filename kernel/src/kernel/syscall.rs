// kernel/src/kernel/syscall.rs
//
// syscall 境界（最小）
// - IPC syscall のみ
// - reply は payload を返す

use super::{EndpointId, KernelState, LogEvent};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    IpcRecv { ep: EndpointId },
    IpcSend { ep: EndpointId, msg: u64 },
    IpcReply { ep: EndpointId, msg: u64 },
}

impl KernelState {
    pub(super) fn handle_pending_syscall_if_any(&mut self) {
        let idx = self.current_task;
        let tid = self.tasks[idx].id;

        if let Some(sc) = self.tasks[idx].pending_syscall.take() {
            self.push_event(LogEvent::SyscallIssued { task: tid });
            self.handle_syscall(sc);
        }
    }

    fn handle_syscall(&mut self, sc: Syscall) {
        let tid = self.tasks[self.current_task].id;
        self.push_event(LogEvent::SyscallHandled { task: tid });

        match sc {
            Syscall::IpcRecv { ep } => self.ipc_recv(ep),
            Syscall::IpcSend { ep, msg } => self.ipc_send(ep, msg),
            Syscall::IpcReply { ep, msg } => self.ipc_reply(ep, msg),
        }
    }
}
