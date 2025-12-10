// kernel/src/kernel/pagetable_init.rs
//
// 役割:
// - 新しい L4 ページテーブルを 1フレーム分確保し、ゼロクリアして返す。
// - 返り値は「自前の」PhysFrame（mem::addr::PhysFrame）。
// - まだこの L4 を CR3 に設定したりはしない（Task1/2 の root_page_frame として持っておくだけ）。
//

use crate::mm::PhysicalMemoryManager;
use crate::mem::addr::{PhysFrame, PAGE_SIZE};
use x86_64::structures::paging::{PageTable, PhysFrame as X86PhysFrame, Size4KiB};
use core::ptr::write_bytes;

/// 新しい L4 ページテーブル用のフレームを 1枚確保し、
/// そのフレームをゼロクリアして、自前の PhysFrame 型で返す。
///
/// - 物理メモリは identity map (phys == virt) 前提で、
///   物理アドレスをそのまま kernel の仮想アドレスとして扱っている。
pub fn allocate_new_l4_table(
    phys_mem: &mut PhysicalMemoryManager,
) -> Option<PhysFrame> {
    // PhysicalMemoryManager から x86_64 の PhysFrame<Size4KiB> を 1枚もらう
    let x86_frame: X86PhysFrame<Size4KiB> = phys_mem.allocate_frame()?;

    // 物理アドレス
    let phys_addr_u64 = x86_frame.start_address().as_u64();

    // identity map 前提なので、そのまま仮想アドレスとして PageTable ポインタに変換
    let ptr = phys_addr_u64 as *mut PageTable;

    unsafe {
        // L4 ページテーブル全体をゼロクリア
        write_bytes(ptr, 0, 1);
    }

    // 自前の PhysFrame 型に変換（frame index = phys_addr / PAGE_SIZE）
    let index = phys_addr_u64 / PAGE_SIZE;
    Some(PhysFrame::from_index(index))
}
