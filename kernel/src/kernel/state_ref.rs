// kernel/src/kernel/state_ref.rs
//
// 役割:
// - arch(割り込み) 側から KernelState にアクセスするための “唯一の入口” を提供する。
//
// やること:
// - KernelState の raw pointer(アドレス) を登録する。
// - 呼び出し側は with_kernel_state() 経由でのみ &mut KernelState を得る。
//
// やらないこと:
// - 複雑な同期（単一コア前提・割り込み中の短時間利用のみ）
// - KernelState の所有権移動（所有は entry.rs 側のまま）

use core::sync::atomic::{AtomicU64, Ordering};

use super::KernelState;

// 0 なら未登録
static KERNEL_STATE_ADDR: AtomicU64 = AtomicU64::new(0);

/// KernelState の参照を登録する（entry.rs から呼ぶ）
pub fn register_kernel_state(ks: &mut KernelState) {
    let addr = ks as *mut KernelState as u64;
    KERNEL_STATE_ADDR.store(addr, Ordering::SeqCst);
}

/// KernelState の参照を解除する（必要なら）
#[allow(dead_code)]
pub fn unregister_kernel_state() {
    KERNEL_STATE_ADDR.store(0, Ordering::SeqCst);
}

/// KernelState を一時的に借用して処理する（arch 側はこれだけ使う）
pub fn with_kernel_state<R>(f: impl FnOnce(&mut KernelState) -> R) -> Option<R> {
    let addr = KERNEL_STATE_ADDR.load(Ordering::SeqCst);
    if addr == 0 {
        return None;
    }

    let p = addr as *mut KernelState;

    // Safety:
    // - register_kernel_state() は KernelState の生存期間中のみ呼ばれる前提
    // - 割り込みハンドラからの短時間利用に限定
    Some(unsafe { f(&mut *p) })
}
