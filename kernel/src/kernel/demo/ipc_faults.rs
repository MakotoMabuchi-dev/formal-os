// kernel/src/kernel/demo/ipc_faults.rs
//
// 役割:
// - IPC 系の fault injection / テスト用初期設定を集約する。
// - endpoint_close_test / dead_partner_test など “テスト都合の分岐” を本体から排除する。
//
// 方針:
// - feature off では完全に no-op
// - 本体状態機械を壊さない（kill は KernelState の正規 API で行う）
// - 「本物の fault」と「テスト注入」を混線させない（reason を分ける）

use super::super::{EndpointId, KernelState, TaskId};

/// KernelState 初期化後の “テスト用初期設定”
pub fn on_kernel_state_init(ks: &mut KernelState) {
    #[cfg(feature = "endpoint_close_test")]
    {
        use super::super::{IPC_DEMO_EP0, TASK2_ID};

        ks.endpoints[IPC_DEMO_EP0.0].owner = Some(TASK2_ID);
        return;
    }

    // feature off のときは何もしない
    let _ = ks;
}

/// IpcRecv の直後に注入（dead_partner_test）
///
/// 目的:
/// - partner が死んだ場合の rescue（reply_waiter 等）を確実に踏ませる
pub fn on_after_ipc_recv(ks: &mut KernelState, task_index: usize, tid: TaskId, ep: EndpointId) {
    #[cfg(feature = "dead_partner_test")]
    {
        use core::sync::atomic::{AtomicBool, Ordering};
        use super::super::TaskKillReason;

        static FIRED: AtomicBool = AtomicBool::new(false);

        // 受信側（TaskId=3）を一度だけ kill
        if tid.0 == 3 && !FIRED.swap(true, Ordering::SeqCst) {
            // “テスト注入” がログで判別できるように、コードを固定で出す
            // ★修正: u64 に統一（TaskKillReason::DemoInjected { code: u64 } と整合）
            let demo_code: u64 = 0xD34D_0001;

            crate::logging::error("dead_partner_test: kill receiver (DemoInjected) right after IpcRecv");
            crate::logging::info_u64("killed_task_id", tid.0);
            crate::logging::info_u64("ep_id", ep.0 as u64);
            crate::logging::info_u64("demo_code", demo_code);

            ks.demo_kill_task(task_index, TaskKillReason::DemoInjected { code: demo_code });
        }
        return;
    }

    // dead_partner_test 無効時だけ参照して unused warning を避ける
    #[cfg(not(feature = "dead_partner_test"))]
    let _ = (ks, task_index, tid, ep);
}
