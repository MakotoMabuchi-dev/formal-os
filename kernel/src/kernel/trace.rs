// kernel/src/kernel/trace.rs
//
// 低コスト trace（観測性）を 1 箇所に集約する。
// - syscall 境界（IpcSend/Recv/Reply の入口）を trace できる
// - IPC 内部の fast/slow/delivered/no_waiter 等の “経路” を trace できる
//
// 設計方針:
// - logging 側に新 API を要求しない（info / info_u64 のみで完結）
// - TaskId / EndpointId の実体型に依存しない（newtype でもOK）
// - no_std 前提で heap 確保なし（固定文字列 + u64）
// - unsafe はここだけに閉じ込める（フォーマル化しやすくする）
//
// feature:
// - ipc_trace_syscall: syscall 境界 trace を有効化
// - ipc_trace_paths:   経路 trace を有効化（ipc_trace_syscall を内包）
//
// 使い方:
// - syscall.rs で trace_ipc_syscall_* を呼ぶ
// - ipc.rs で trace_ipc_path(...) を呼ぶ

use super::{EndpointId, TaskId};

#[cfg(feature = "ipc_trace_syscall")]
#[derive(Clone, Copy)]
pub enum IpcSyscallKind {
    Recv,
    Send,
    Reply,
}

// ★重要：IpcPathEvent は “常に存在” させる（feature off でもコンパイル可能にする）
#[derive(Clone, Copy)]
pub enum IpcPathEvent {
    SendFast,
    SendSlow,
    RecvFast,
    RecvSlow,
    ReplyDelivered,
    ReplyNoWaiter,
}

/// syscall 境界 trace（入口）: recv
#[inline(always)]
pub fn trace_ipc_syscall_recv(tid: &TaskId, ep: &EndpointId) {
    #[cfg(feature = "ipc_trace_syscall")]
    trace_ipc_syscall(IpcSyscallKind::Recv, tid, ep, None);
    #[cfg(not(feature = "ipc_trace_syscall"))]
    {
        let _ = tid;
        let _ = ep;
    }
}

/// syscall 境界 trace（入口）: send
#[inline(always)]
pub fn trace_ipc_syscall_send(tid: &TaskId, ep: &EndpointId, msg: u64) {
    #[cfg(feature = "ipc_trace_syscall")]
    trace_ipc_syscall(IpcSyscallKind::Send, tid, ep, Some(msg));
    #[cfg(not(feature = "ipc_trace_syscall"))]
    {
        let _ = tid;
        let _ = ep;
        let _ = msg;
    }
}

/// syscall 境界 trace（入口）: reply
#[inline(always)]
pub fn trace_ipc_syscall_reply(tid: &TaskId, ep: &EndpointId, msg: u64) {
    #[cfg(feature = "ipc_trace_syscall")]
    trace_ipc_syscall(IpcSyscallKind::Reply, tid, ep, Some(msg));
    #[cfg(not(feature = "ipc_trace_syscall"))]
    {
        let _ = tid;
        let _ = ep;
        let _ = msg;
    }
}

/// IPC 内部の経路 trace（出口）
/// - ipc_trace_paths feature の時だけ 1 行を必ず出す
#[inline(always)]
pub fn trace_ipc_path(ev: IpcPathEvent) {
    #[cfg(feature = "ipc_trace_paths")]
    {
        match ev {
            IpcPathEvent::SendFast => crate::logging::info("ipc_trace_paths send=fast"),
            IpcPathEvent::SendSlow => crate::logging::info("ipc_trace_paths send=slow"),
            IpcPathEvent::RecvFast => crate::logging::info("ipc_trace_paths recv=fast"),
            IpcPathEvent::RecvSlow => crate::logging::info("ipc_trace_paths recv=slow"),
            IpcPathEvent::ReplyDelivered => crate::logging::info("ipc_trace_paths reply=delivered"),
            IpcPathEvent::ReplyNoWaiter => crate::logging::info("ipc_trace_paths reply=no_waiter"),
        }
    }
    #[cfg(not(feature = "ipc_trace_paths"))]
    {
        let _ = ev;
    }
}

#[cfg(feature = "ipc_trace_syscall")]
fn trace_ipc_syscall(kind: IpcSyscallKind, tid: &TaskId, ep: &EndpointId, msg: Option<u64>) {
    match kind {
        IpcSyscallKind::Recv => crate::logging::info("ipc_trace kind=ipc_recv"),
        IpcSyscallKind::Send => crate::logging::info("ipc_trace kind=ipc_send"),
        IpcSyscallKind::Reply => crate::logging::info("ipc_trace kind=ipc_reply"),
    }

    crate::logging::info_u64("task_id_hash", stable_hash64_of_bytes(tid));
    crate::logging::info_u64("ep_id_hash", stable_hash64_of_bytes(ep));

    if let Some(m) = msg {
        crate::logging::info_u64("msg", m);
    }
}

/// 値のメモリ表現（raw bytes）を FNV-1a 64bit でハッシュする。
///
/// NOTE:
/// - これは “識別用のデバッグハッシュ” であり、永続IDではない。
/// - unsafe はこの関数に閉じ込める。
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
