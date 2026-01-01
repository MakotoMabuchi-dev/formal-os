// kernel/src/kernel/demo/mod.rs
//
// 役割:
// - デモ/テスト注入（fault injection）を KernelState から分離する。
// - feature で切り替わる “実験用分岐” をここに閉じ込める。
//
// 設計方針:
// - KernelState 本体の状態機械を汚さない（呼び出し側は hook を叩くだけ）
// - feature off でもコンパイルできるように、関数は常に存在させる
// - 注入ロジックは demo/* に分割して責務を小さく保つ

pub mod mem_faults;
pub mod ipc_faults;

use super::{EndpointId, KernelState, TaskId};

/// KernelState 初期化後に “テスト用の初期設定” を適用する
pub fn on_kernel_state_init(ks: &mut KernelState) {
    ipc_faults::on_kernel_state_init(ks);
}

/// mem_demo のタイミングで “注入” を試す
/// - 注入したら true（通常 mem_demo をスキップしてよい）
pub fn on_mem_demo(ks: &mut KernelState) -> bool {
    mem_faults::on_mem_demo(ks)
}

/// IpcRecv の直後に “テスト用イベント” を注入する（dead_partner_test など）
pub fn on_after_ipc_recv(ks: &mut KernelState, task_index: usize, tid: TaskId, ep: EndpointId) {
    ipc_faults::on_after_ipc_recv(ks, task_index, tid, ep);
}
