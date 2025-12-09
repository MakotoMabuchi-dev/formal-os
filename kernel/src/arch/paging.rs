// src/arch/paging.rs
//
// 役割:
// - x86_64 アーキテクチャ向けのページング処理をまとめる場所。
// - ページテーブルの実際の操作（unsafe）は最終的にここに集約する。
// 今の段階:
// - BootInfo は受け取るが、physical_memory_offset はまだ使わない。
// - MemAction(Map/Unmap) を受け取り、x86_64 の Page/PhysFrame/Flags に変換してログ出力する。
// - さらに、「OffsetPageTable を初期化するための unsafe ヘルパー」を用意する。
//   （ただし、まだ実際には呼ばない = 実機ページテーブルは書き換えない）

use bootloader::BootInfo;
use crate::logging;
use crate::mem::paging::{MemAction, PageFlags};

use x86_64::{
    VirtAddr,
    PhysAddr,
    registers::control::Cr3,
    structures::paging::{
        Page,
        PhysFrame,
        PageTable,
        PageTableFlags,
        Size4KiB,
        OffsetPageTable,
    },
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

/// 「現在アクティブな L4 ページテーブル」への &mut PageTable を得る。
///
/// - CR3 レジスタから L4 テーブルの物理フレームを取得し、
///   physical_memory_offset を足して仮想アドレスに変換する。
/// - この関数は **仮想アドレス空間上に「物理メモリがどこにマップされているか」** を
///   知っている必要があり、間違うと即ページフォルトになるので、
///   今はまだ「道具として用意するだけ」で、実際には呼ばない。
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    // CR3 から L4 テーブルの物理フレームを取得
    let (level_4_frame, _) = Cr3::read();
    let phys = level_4_frame.start_address();

    // 物理メモリが "全体として physical_memory_offset だけずらされて" 仮想空間に
    // マップされていると仮定して、L4 テーブルの仮想アドレスを計算する。
    //
    //   virt = offset + phys
    //
    let virt = physical_memory_offset + phys.as_u64();

    // 計算した仮想アドレスを PageTable への生ポインタとして解釈する。
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    &mut *page_table_ptr
}

/// OffsetPageTable を初期化するためのヘルパー。
///
/// - physical_memory_offset: 「物理メモリが仮想空間のどこにマップされているか」のオフセット。
/// - 呼び出し側が offset を正しく指定しないと危険なので unsafe。
///
/// ★ 今の段階では、この関数は **どこからも呼ばない**。
///   → 将来、「物理メモリオフセットが正しく取れた段階」で、
///      ここを使って map_to 実装に進む。
pub unsafe fn init_offset_page_table(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, physical_memory_offset)
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

            // TODO（次のステップ）:
            //   1. 物理メモリオフセット physical_memory_offset をどこかで正しく取得する
            //   2. init_offset_page_table(physical_memory_offset) で OffsetPageTable を作る
            //   3. mapper.map_to(_x86_page, _x86_frame, _x86_flags, ...) を呼ぶ
            //   4. flush() で TLB を更新
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
