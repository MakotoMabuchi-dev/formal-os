// src/mm/pagetable_init.rs
//
// 役割：
// - per-task page table を構築するために、
//   「新しい L4 ページテーブルを作る」処理をまとめる。
// - まだ kernel 空間のマッピングを完全コピーはしない。
//   （最低限、ゼロクリアの L4 テーブルを作るだけ）
//
// 将来：
// - kernel 空間部分のコピー
// - L3/L2/L1 テーブルの自動生成
// - ユーザ空間アドレスの割り当て（まだ先）
//

use x86_64::{
    structures::paging::{PageTable, Size4KiB, PhysFrame},
};
use crate::mm::PhysicalMemoryManager;
use core::ptr::write_bytes;

pub fn allocate_new_l4_table(
    phys_mem: &mut PhysicalMemoryManager,
) -> Option<PhysFrame<Size4KiB>> {
    // まず 1 フレーム確保する
    let frame = phys_mem.allocate_frame()?;

    // 物理フレーム → 仮想アドレスに変換
    // いまは identity map 前提（PHYSICAL_MEMORY_OFFSET=0）なので、
    // 物理アドレスをそのまま mutable pointer として扱う。
    let phys_addr = frame.start_address().as_u64();
    let ptr = phys_addr as *mut PageTable;

    unsafe {
        // L4 page table をゼロクリア（全エントリ無効）
        write_bytes(ptr, 0, 1);
    }

    Some(frame)
}
