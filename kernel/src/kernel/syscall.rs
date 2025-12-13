// kernel/src/kernel/syscall.rs
//
// syscall 境界（最小）
// - CPU の syscall 命令はまだ使わず、タスクが pending_syscall を置く方式。
// - KernelState は「入口(handle_pending_syscall_if_any)」でそれを回収し、
//   handle_syscall で IPC にディスパッチするだけ。

use super::{EndpointId, KernelState, LogEvent};

/// syscall の最小セット（IPCのみ）
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    IpcRecv { ep: EndpointId },
    IpcSend { ep: EndpointId, msg: u64 },
    IpcReply { ep: EndpointId },
}

impl KernelState {
    /// pending_syscall を拾って処理する（1 tick で最大 1 回）
    pub(super) fn handle_pending_syscall_if_any(&mut self) {
        let idx = self.current_task;
        let tid = self.tasks[idx].id;

        if let Some(sc) = self.tasks[idx].pending_syscall.take() {
            self.push_event(LogEvent::SyscallIssued { task: tid });
            self.handle_syscall(sc);
        }
    }

    /// syscall の唯一の入口（ここから内部実装にディスパッチ）
    fn handle_syscall(&mut self, sc: Syscall) {
        let tid = self.tasks[self.current_task].id;
        self.push_event(LogEvent::SyscallHandled { task: tid });

        match sc {
            Syscall::IpcRecv { ep } => self.ipc_recv(ep),
            Syscall::IpcSend { ep, msg } => self.ipc_send(ep, msg),
            Syscall::IpcReply { ep } => self.ipc_reply(ep),
        }
    }
}
