// kernel/src/kernel/syscall.rs
//
// syscall 境界（最小）
// - IPC syscall のみ
// - reply は payload を返す
//
// トレース（feature で切替）
// - ipc_trace_syscall: syscall 境界の trace（kind/msg/task/ep を出す）
// - ipc_trace_paths:   “fast/slow/delivered/blocked” 等の経路（ipc.rs 側）
//
// 設計方針:
// - logging 側に新 API を要求しない（info / info_u64 のみで完結）
// - TaskId / EndpointId は newtype 前提でも OK（ここでは中身にアクセスするだけ）
// - no_std 前提で “ヒープ確保なし” で出せる形にする（固定文字列 + u64）

use super::{EndpointId, KernelState, LogEvent, TaskKillReason};

#[cfg(feature = "dead_partner_test")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "dead_partner_test")]
static DEAD_PARTNER_TEST_FIRED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Syscall {
    IpcRecv { ep: EndpointId },
    IpcSend { ep: EndpointId, msg: u64 },
    IpcReply { ep: EndpointId, msg: u64 },
}

impl KernelState {
    /// 現在タスクの pending_syscall があれば取り出して実行する。
    pub(super) fn handle_pending_syscall_if_any(&mut self) {
        let idx = self.current_task;
        let tid = self.tasks[idx].id;

        if let Some(sc) = self.tasks[idx].pending_syscall.take() {
            self.push_event(LogEvent::SyscallIssued { task: tid });
            self.handle_syscall(sc);
        }
    }

    fn handle_syscall(&mut self, sc: Syscall) {
        let task_index = self.current_task;
        let tid = self.tasks[task_index].id;

        // NOTE: 「Handled」は実行開始の観測点として使っている（現状のログ設計に合わせる）
        self.push_event(LogEvent::SyscallHandled { task: tid });

        match sc {
            Syscall::IpcRecv { ep } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Recv, tid, ep, None);

                self.ipc_recv(ep);

                // ------------------------------------------------------------
                // dead_partner_test:
                //   receiver（Task3: id=3）が recv した直後に 1 回だけ kill して、
                //   sender（reply waiter）が DEAD partner rescue されることを検証する。
                //
                // 狙い:
                // - sender が reply_queue 等で待っている状態を作る
                // - receiver を kill
                // - resolve_ipc_reply_waiters_for_dead_partner() が走り、
                //   sender が READY に戻り last_reply=IPC_ERR_DEAD_PARTNER になる
                // ------------------------------------------------------------
                #[cfg(feature = "dead_partner_test")]
                {
                    // TaskId は newtype の中身で判定（task_id=3 を receiver とする）
                    if tid.0 == 3 && !DEAD_PARTNER_TEST_FIRED.swap(true, Ordering::SeqCst) {
                        crate::logging::error("dead_partner_test: kill receiver right after IpcRecv");
                        crate::logging::info_u64("killed_task_id", tid.0);
                        crate::logging::info_u64("ep_id", ep.0 as u64);

                        // 「理由」は最小で OK（テスト用）
                        self.kill_task(
                            task_index,
                            TaskKillReason::UserPageFault {
                                addr: 0,
                                err: 0,
                                rip: 0,
                            },
                        );

                        // kill_task が schedule を呼び得るので、この後は何もしない
                        return;
                    }
                }
            }

            Syscall::IpcSend { ep, msg } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Send, tid, ep, Some(msg));

                self.ipc_send(ep, msg);
            }

            Syscall::IpcReply { ep, msg } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Reply, tid, ep, Some(msg));

                self.ipc_reply(ep, msg);
            }
        }
    }
}

#[cfg(feature = "ipc_trace_syscall")]
#[derive(Clone, Copy)]
enum TraceKind {
    Recv,
    Send,
    Reply,
}

#[cfg(feature = "ipc_trace_syscall")]
fn trace_ipc(kind: TraceKind, tid: super::TaskId, ep: EndpointId, msg: Option<u64>) {
    // kind は固定文字列で出す（alloc不要／見やすい）
    match kind {
        TraceKind::Recv => crate::logging::info("ipc_trace kind=ipc_recv"),
        TraceKind::Send => crate::logging::info("ipc_trace kind=ipc_send"),
        TraceKind::Reply => crate::logging::info("ipc_trace kind=ipc_reply"),
    }

    // TaskId / EndpointId は newtype の中身をそのまま出す
    crate::logging::info_u64("task_id", tid.0);
    crate::logging::info_u64("ep_id", ep.0 as u64);

    if let Some(m) = msg {
        crate::logging::info_u64("msg", m);
    }
}
