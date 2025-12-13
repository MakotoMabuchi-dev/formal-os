// kernel/src/kernel/pagetable_init.rs
//
// 役割:
// - タスク用の新しい root(PML4) を 1フレーム分確保して返す。
// - 物理フレームの中身（PageTable の初期化）は arch::paging に寄せる。
//
// やること:
// - PhysicalMemoryManager から 4KiB フレームを 1 枚確保
// - 自前 PhysFrame に変換して返す
//
// やらないこと:
// - ここでフレームをゼロクリアする（unsafe を増やさない）
//   ※ init_user_pml4_from_current() が 512 エントリを上書きするのでゼロクリアは必須ではない
// - CR3 の切替（スケジューラの責務）

use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, PAGE_SIZE};

pub fn allocate_new_l4_table(phys_mem: &mut PhysicalMemoryManager) -> Option<PhysFrame> {
    let raw = phys_mem.allocate_frame()?;
    let phys_u64 = raw.start_address().as_u64();
    let index = phys_u64 / PAGE_SIZE;
    Some(PhysFrame::from_index(index))
}
