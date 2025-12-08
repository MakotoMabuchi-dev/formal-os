// src/arch/paging.rs
//
// 役割:
// - x86_64 アーキテクチャ向けのページング処理をまとめる場所。
// - ページテーブルの実際の操作（unsafe）は最終的にここに集約する。
// 今の段階:
// - BootInfo は受け取るが、physical_memory_offset は使わない（フィールドが無いため）。
// - MemAction(Map/Unmap) を受け取り、x86_64 の Page/PhysFrame/Flags に変換してログ出力する。
// - 実際のページテーブル書き換えはまだ行わず TODO にしておく。

use bootloader::BootInfo;
use crate::logging;
use crate::mem::paging::{MemAction, PageFlags};

use x86_64::{
    VirtAddr,
    PhysAddr,
    structures::paging::{Page, PhysFrame, PageTableFlags, Size4KiB},
};

/// paging サブシステムの初期化。
/// - 現状の BootInfo には physical_memory_offset フィールドが無いため、
///   ここでは単に「初期化した」というログだけ出す。
pub fn init(boot_info: &'static BootInfo) {
    logging::info("arch::paging::init: start");

    // BootInfo 自体は将来の拡張のために受け取っておく。
    // 今は memory_map などを眺めることもできるが、使わないので unused 回避だけする。
    let _ = boot_info;

    logging::info("arch::paging::init: done (no physical_memory_offset in BootInfo)");
}

/// 抽象 PageFlags → x86_64 の PageTableFlags への変換。
/// - ここでは最低限のフラグだけ対応させている。
fn to_x86_flags(flags: PageFlags) -> PageTableFlags {
    let mut res = PageTableFlags::empty();

    if flags.contains(PageFlags::PRESENT) {
        res |= PageTableFlags::PRESENT;
    }
    if flags.contains(PageFlags::WRITABLE) {
        res |= PageTableFlags::WRITABLE;
    }
    if flags.contains(PageFlags::USER) {
        res |= PageTableFlags::USER_ACCESSIBLE;
    }
    if flags.contains(PageFlags::NO_EXEC) {
        res |= PageTableFlags::NO_EXECUTE;
    }

    res
}

/// MemAction を受け取り、将来的にページテーブルを操作するための入口。
///
/// 今はまだ「x86_64 の Page/PhysFrame/Flags に変換してログを出すだけ」
/// で、実際の map_to/unmap は TODO として残しておく。
pub unsafe fn apply_mem_action(action: MemAction) {
    match action {
        MemAction::Map { page, frame, flags } => {
            logging::info("arch::paging::apply_mem_action: Map");

            // 抽象型 → 具体的なアドレス/ページ/フレームに変換
            let virt_addr_u64 = page.start_address().0;
            let phys_addr_u64 = frame.start_address().0;
            let _x86_page: Page<Size4KiB> =
                Page::containing_address(VirtAddr::new(virt_addr_u64));
            let _x86_frame: PhysFrame<Size4KiB> =
                PhysFrame::containing_address(PhysAddr::new(phys_addr_u64));
            let _x86_flags = to_x86_flags(flags);

            logging::info_u64(" virt_addr", virt_addr_u64);
            logging::info_u64(" phys_addr", phys_addr_u64);
            logging::info_u64(" flags_bits", flags.bits());

            // TODO: ここで実際にページテーブルを書き換える:
            //   1. CR3 から現在の L4 PageTable を取り出す
            //   2. OffsetPageTable などを使って map_to(_x86_page, _x86_frame, _x86_flags, ...)
            //   3. flush() で TLB を更新
        }

        MemAction::Unmap { page } => {
            logging::info("arch::paging::apply_mem_action: Unmap");

            let virt_addr_u64 = page.start_address().0;
            let _x86_page: Page<Size4KiB> =
                Page::containing_address(VirtAddr::new(virt_addr_u64));

            logging::info_u64(" virt_addr", virt_addr_u64);

            // TODO:
            //   1. _x86_page に対応する PTE を探す
            //   2. PTE を無効化し、必要なら物理フレームを解放
            //   3. TLB を flush
        }
    }
}
