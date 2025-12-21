// kernel/src/kernel/syscall.rs
//
// syscall 境界（最小）
// - IPC syscall のみ
// - reply は payload を返す
//
// トレース（feature で切替）
// - ipc_trace_syscall: syscall 境界の trace（kind/msg/task/ep を出す）
// - ipc_trace_paths:   将来 “fast/slow/delivered/blocked” 等の経路も出す（ipc_trace_syscall を内包）
//
// 設計方針:
// - logging 側に新 API を要求しない（info / info_u64 のみで完結）
// - TaskId / EndpointId の実体型に依存しない（newtype でもOK）
// - no_std 前提で “ヒープ確保なし” で出せる形にする（固定文字列 + u64）
//
// NOTE:
// - TaskId/EndpointId を u64 に直接変換しない（型に依存する）
//   代わりに “値の raw bytes” から安定ハッシュを作って出す。
//   これで「毎回同じ」問題が解消される。

use super::{EndpointId, KernelState, LogEvent};

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

        self.push_event(LogEvent::SyscallHandled { task: tid });

        match sc {
            Syscall::IpcRecv { ep } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Recv, &tid, &ep, None);

                self.ipc_recv(ep)
            }
            Syscall::IpcSend { ep, msg } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Send, &tid, &ep, Some(msg));

                self.ipc_send(ep, msg)
            }
            Syscall::IpcReply { ep, msg } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Reply, &tid, &ep, Some(msg));

                self.ipc_reply(ep, msg)
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
fn trace_ipc(kind: TraceKind, tid: &super::TaskId, ep: &EndpointId, msg: Option<u64>) {
    // kind は固定文字列で出す（alloc不要／見やすい）
    match kind {
        TraceKind::Recv => crate::logging::info("ipc_trace kind=ipc_recv"),
        TraceKind::Send => crate::logging::info("ipc_trace kind=ipc_send"),
        TraceKind::Reply => crate::logging::info("ipc_trace kind=ipc_reply"),
    }

    // TaskId/EndpointId は型に依存せず “値の raw bytes” からハッシュ化して出す
    crate::logging::info_u64("task_id_hash", stable_hash64_of_bytes(tid));
    crate::logging::info_u64("ep_id_hash", stable_hash64_of_bytes(ep));

    if let Some(m) = msg {
        crate::logging::info_u64("msg", m);
    }
}

/// 値のメモリ表現（raw bytes）を FNV-1a 64bit でハッシュする。
/// - Copy 型なら安全に bytes を読める（参照から読むだけ）
/// - newtype でも enum でも「値が変われば hash が変わる」ので識別に使える
#[cfg(feature = "ipc_trace_syscall")]
fn stable_hash64_of_bytes<T>(v: &T) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut h = FNV_OFFSET;
    let p = (v as *const T) as *const u8;
    let n = core::mem::size_of::<T>();

    // 生バイト列を読む（デバッグ用途のハッシュなのでこれで十分）
    let bytes = unsafe { core::slice::from_raw_parts(p, n) };
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}
