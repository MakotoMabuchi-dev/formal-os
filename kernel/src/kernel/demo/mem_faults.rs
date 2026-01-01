// kernel/src/kernel/demo/mem_faults.rs
//
// 役割:
// - mem_demo 系の fault injection を集約する。
// - evil_double_map / evil_unmap_not_mapped のような “意図的異常系” をここに閉じ込める。
//
// 方針:
// - 再現性を最優先（Task固定・1回だけ等）
// - panic しない（エラーは syscall 戻り値で観測）
// - ★重要: KernelState 本体の mem_demo 状態機械（mem_demo_stage）を汚さない
//   → 注入の進行状態は demo 側の static で管理する

use super::super::KernelState;

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// mem_demo のタイミングで fault injection を試す。
/// - 何か注入したら true（通常 mem_demo はスキップしてよい）
pub fn on_mem_demo(ks: &mut KernelState) -> bool {
    #[cfg(feature = "evil_double_map")]
    {
        return evil_double_map(ks);
    }

    #[cfg(feature = "evil_unmap_not_mapped")]
    {
        return evil_unmap_not_mapped(ks);
    }

    // feature off
    let _ = ks;
    false
}

// -----------------------------------------------------------------------------
// evil_double_map
// - 同一ページを 2 回 Map して、2 回目が AlreadyMapped を返すことを確認
// -----------------------------------------------------------------------------

#[cfg(feature = "evil_double_map")]
fn evil_double_map(ks: &mut KernelState) -> bool {
    use super::super::{TaskState, TASK0_INDEX, TASK1_INDEX};
    use super::super::Syscall;
    use crate::mem::paging::PageFlags;

    // 0: 未実行, 1: 1回目済み, 2: 2回目済み(終了)
    static STAGE: AtomicU8 = AtomicU8::new(0);

    let task_idx = ks.current_task;

    if task_idx == TASK0_INDEX {
        return false;
    }
    if task_idx >= ks.num_tasks || ks.tasks[task_idx].state == TaskState::Dead {
        return true;
    }

    if task_idx != TASK1_INDEX {
        return false;
    }

    if ks.tasks[task_idx].pending_syscall.is_some() {
        return true;
    }

    let stage = STAGE.load(Ordering::Relaxed);
    if stage >= 2 {
        return false;
    }

    let page = ks.demo_page_for_task(task_idx);
    let flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

    if stage == 0 {
        crate::logging::info("evil_double_map: PageMap #1");
        ks.tasks[task_idx].pending_syscall = Some(Syscall::PageMap { page, flags });
        STAGE.store(1, Ordering::Relaxed);
        return true;
    }

    crate::logging::info("evil_double_map: PageMap #2 (expect AlreadyMapped)");
    ks.tasks[task_idx].pending_syscall = Some(Syscall::PageMap { page, flags });
    STAGE.store(2, Ordering::Relaxed);
    true
}

// -----------------------------------------------------------------------------
// evil_unmap_not_mapped
// - 未Map のページを Unmap して NotMapped を返すことを確認
// -----------------------------------------------------------------------------

#[cfg(feature = "evil_unmap_not_mapped")]
fn evil_unmap_not_mapped(ks: &mut KernelState) -> bool {
    use super::super::{TaskState, TASK0_INDEX, TASK1_INDEX};
    use super::super::Syscall;

    static FIRED: AtomicBool = AtomicBool::new(false);

    let task_idx = ks.current_task;

    if task_idx == TASK0_INDEX {
        return false;
    }
    if task_idx >= ks.num_tasks || ks.tasks[task_idx].state == TaskState::Dead {
        return true;
    }

    if task_idx != TASK1_INDEX {
        return false;
    }

    if ks.tasks[task_idx].pending_syscall.is_some() {
        return true;
    }

    if FIRED.swap(true, Ordering::SeqCst) {
        return false;
    }

    let page = ks.demo_page_for_task(task_idx);

    crate::logging::info("evil_unmap_not_mapped: PageUnmap (expect NotMapped)");
    ks.tasks[task_idx].pending_syscall = Some(Syscall::PageUnmap { page });
    true
}
