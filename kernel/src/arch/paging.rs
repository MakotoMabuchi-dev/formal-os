// kernel/src/arch/paging.rs
//
// 役割:
// - x86_64 アーキテクチャ向けのページング処理をまとめる場所。
// - ページテーブルの実際の操作（unsafe）は最終的にここに集約する。
// 現時点:
// - REAL PAGING を有効にして map_to/unmap を呼び出し、
//   map_to が成功した場合は実際に仮想アドレスへの read/write テストを行う。
// - BootInfo の memory_map をログにダンプして、物理メモリレイアウトを観察。
// - physmap offset を AtomicU64 に保持し、phys_to_virt に集約。
// - switch_address_space(root) は安全判定が通った場合のみ CR3 を書き換える（段階的導入）。
// - User 用 PML4 を「Kernel の上位半分コピー」で初期化する API を提供。
// - CR3 を切り替えなくても、任意の root PML4 を直接操作/検査する API を提供。
//   (low-half にカーネルがいる現状でも安全に “User AS のページテーブル” を更新できる)
//

use bootloader::BootInfo;
use bootloader::bootinfo::MemoryRegionType;
use crate::logging;
use crate::mm::PhysicalMemoryManager;
use crate::mem::paging::{MemAction, PageFlags};
use crate::mem::addr::PhysFrame as MyPhysFrame;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::{
    VirtAddr,
    PhysAddr,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{
        Mapper, FrameAllocator,
        Page,
        PhysFrame,
        PageTable,
        PageTableFlags,
        Size4KiB,
        OffsetPageTable,
        Translate, // ★ 追加: translate_addr を使うため
    },
    structures::paging::mapper::{MapToError, UnmapError},
};

/// 実際にページテーブルを書き換えるかどうかのフラグ。
const ENABLE_REAL_PAGING: bool = true;

/// physmap の仮想アドレスオフセット（BootInfo.physical_memory_offset）
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// CR3 を実際に切り替えてよいか（安全判定が通ったときだけ true）
static ALLOW_REAL_CR3_SWITCH: AtomicBool = AtomicBool::new(false);

fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    let off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    VirtAddr::new(off + phys.as_u64())
}

unsafe fn phys_u64_to_virt_ptr(phys: u64) -> *mut u8 {
    let off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    (off + phys) as *mut u8
}

/// paging サブシステムの初期化。
pub fn init(boot_info: &'static BootInfo) {
    logging::info("arch::paging::init: start");

    // あなたの環境では physical_memory_offset は u64
    PHYSICAL_MEMORY_OFFSET.store(boot_info.physical_memory_offset, Ordering::Relaxed);

    let mem_map = &boot_info.memory_map;
    logging::info("arch::paging::init: memory map dump start");

    for (i, region) in mem_map.iter().enumerate() {
        let start = region.range.start_frame_number * 4096;
        let end = region.range.end_frame_number * 4096;

        logging::info(" mem_region:");
        logging::info_u64("  index", i as u64);
        logging::info_u64("  start_phys", start);
        logging::info_u64("  end_phys", end);

        match region.region_type {
            MemoryRegionType::Usable => logging::info("  type = Usable"),
            MemoryRegionType::Reserved => logging::info("  type = Reserved"),
            MemoryRegionType::AcpiReclaimable => logging::info("  type = AcpiReclaimable"),
            MemoryRegionType::AcpiNvs => logging::info("  type = AcpiNvs"),
            MemoryRegionType::BadMemory => logging::info("  type = BadMemory"),
            other => {
                logging::info("  type = Other");
                let _ = other;
            }
        }
    }

    logging::info("arch::paging::init: memory map dump end");
    logging::info("arch::paging::init: done");
}

/// CR3 切替を「安全に」有効化するための設定関数。
pub fn configure_cr3_switch_safety(code_addr: u64, stack_addr: u64) {
    let kernel_start = crate::mem::layout::KERNEL_SPACE_START;

    let code_ok = code_addr >= kernel_start;
    let stack_ok = stack_addr >= kernel_start;

    logging::info("arch::paging::configure_cr3_switch_safety");
    logging::info_u64(" code_addr", code_addr);
    logging::info_u64(" stack_addr", stack_addr);
    logging::info_u64(" kernel_space_start", kernel_start);

    if code_ok && stack_ok {
        logging::info(" CR3 real switch: ENABLED");
        ALLOW_REAL_CR3_SWITCH.store(true, Ordering::Relaxed);
    } else {
        logging::info(" CR3 real switch: DISABLED (kernel not in high-half?)");
        ALLOW_REAL_CR3_SWITCH.store(false, Ordering::Relaxed);
    }
}

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

unsafe fn active_level_4_table() -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let phys = level_4_frame.start_address();
    let virt = phys_to_virt(phys);
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}

pub unsafe fn init_offset_page_table() -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table();
    let offset = VirtAddr::new(PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed));
    OffsetPageTable::new(level_4_table, offset)
}

/// 任意の root(PML4) を指して OffsetPageTable を組み立てる
pub unsafe fn init_offset_page_table_for_root(root: MyPhysFrame) -> OffsetPageTable<'static> {
    let pml4_phys = PhysAddr::new(root.start_address().0);
    let pml4_virt = phys_to_virt(pml4_phys);
    let pml4_ptr: *mut PageTable = pml4_virt.as_mut_ptr();
    let pml4 = &mut *pml4_ptr;

    let offset = VirtAddr::new(PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed));
    OffsetPageTable::new(pml4, offset)
}

/// PhysicalMemoryManager を x86_64 の FrameAllocator として使う薄いラッパ。
pub struct KernelFrameAllocator<'a> {
    inner: &'a mut PhysicalMemoryManager,
}

impl<'a> KernelFrameAllocator<'a> {
    pub fn new(inner: &'a mut PhysicalMemoryManager) -> Self {
        KernelFrameAllocator { inner }
    }
}

unsafe impl<'a> FrameAllocator<Size4KiB> for KernelFrameAllocator<'a> {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        self.inner.allocate_frame()
    }
}

/// MemAction を受け取り、ページテーブルを操作するための入口（現CR3用）。
pub unsafe fn apply_mem_action(action: MemAction, phys_mem: &mut PhysicalMemoryManager) {
    apply_mem_action_with_mapper(action, None, phys_mem);
}

/// MemAction を指定 root のページテーブルに適用する（CR3は切替えない）
pub unsafe fn apply_mem_action_in_root(
    action: MemAction,
    root: MyPhysFrame,
    phys_mem: &mut PhysicalMemoryManager,
) {
    apply_mem_action_with_mapper(action, Some(root), phys_mem);
}

/// 共通実装：mapper を切替え可能にした apply
unsafe fn apply_mem_action_with_mapper(
    action: MemAction,
    root: Option<MyPhysFrame>,
    phys_mem: &mut PhysicalMemoryManager,
) {
    match action {
        MemAction::Map { page, frame, flags } => {
            logging::info("arch::paging::apply_mem_action: Map");

            let virt_addr_u64 = page.start_address().0;
            let phys_addr_u64 = frame.start_address().0;

            let x86_page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(virt_addr_u64));
            let x86_frame: PhysFrame<Size4KiB> =
                PhysFrame::containing_address(PhysAddr::new(phys_addr_u64));
            let x86_flags = to_x86_flags(flags);

            logging::info_u64(" virt_addr", virt_addr_u64);
            logging::info_u64(" phys_addr", phys_addr_u64);
            logging::info_u64(" flags_bits", flags.bits());

            if ENABLE_REAL_PAGING {
                logging::info(" REAL PAGING: map_to() will be executed");

                let mut mapper = match root {
                    Some(r) => init_offset_page_table_for_root(r),
                    None => init_offset_page_table(),
                };
                let mut frame_alloc = KernelFrameAllocator::new(phys_mem);

                match mapper.map_to(x86_page, x86_frame, x86_flags, &mut frame_alloc) {
                    Ok(flush) => {
                        flush.flush();
                        logging::info(" map_to: OK (flush done)");

                        // root=None（現CR3）だけ mem_test を行う
                        if root.is_none() {
                            let ptr = virt_addr_u64 as *mut u64;
                            let test_value: u64 = 0xDEAD_BEEF_DEAD_BEEFu64;

                            logging::info(" mem_test: writing test_value");
                            write_volatile(ptr, test_value);

                            let read_back = read_volatile(ptr);
                            logging::info_u64(" mem_test: read_back", read_back);

                            if read_back == test_value {
                                logging::info(" mem_test: OK (value matched)");
                            } else {
                                logging::error(" mem_test: MISMATCH!");
                            }
                        }
                    }
                    Err(err) => {
                        logging::error(" map_to: ERROR");
                        log_map_to_error(err);
                    }
                }
            } else {
                logging::info(" REAL PAGING: disabled (ENABLE_REAL_PAGING = false)");
            }
        }

        MemAction::Unmap { page } => {
            logging::info("arch::paging::apply_mem_action: Unmap");

            let virt_addr_u64 = page.start_address().0;
            let x86_page: Page<Size4KiB> = Page::containing_address(VirtAddr::new(virt_addr_u64));

            logging::info_u64(" virt_addr", virt_addr_u64);

            if ENABLE_REAL_PAGING {
                logging::info(" REAL PAGING: unmap() will be executed");

                let mut mapper = match root {
                    Some(r) => init_offset_page_table_for_root(r),
                    None => init_offset_page_table(),
                };

                match mapper.unmap(x86_page) {
                    Ok((_frame, flush)) => {
                        flush.flush();
                        logging::info(" unmap: OK (flush done)");
                    }
                    Err(err) => {
                        logging::error(" unmap: ERROR");
                        log_unmap_error(err);
                    }
                }

                let _ = phys_mem;
            } else {
                logging::info(" REAL PAGING: disabled (ENABLE_REAL_PAGING = false)");
            }
        }
    }
}

/// 指定 root のページテーブルで「virt_addr がどの phys に解決されるか」をログする
pub fn debug_translate_in_root(root: MyPhysFrame, virt_addr_u64: u64) {
    if !ENABLE_REAL_PAGING {
        logging::info(" debug_translate_in_root: REAL PAGING disabled");
        return;
    }

    unsafe {
        let mapper = init_offset_page_table_for_root(root);
        let v = VirtAddr::new(virt_addr_u64);

        match mapper.translate_addr(v) {
            Some(p) => {
                logging::info(" translate: OK");
                logging::info_u64("  virt_addr", virt_addr_u64);
                logging::info_u64("  phys_addr", p.as_u64());
            }
            None => {
                logging::info(" translate: NONE (not mapped)");
                logging::info_u64("  virt_addr", virt_addr_u64);
            }
        }
    }
}

fn log_map_to_error(err: MapToError<Size4KiB>) {
    match err {
        MapToError::FrameAllocationFailed => {
            logging::error("  MapToError::FrameAllocationFailed");
        }
        MapToError::ParentEntryHugePage => {
            logging::error("  MapToError::ParentEntryHugePage");
        }
        MapToError::PageAlreadyMapped(old_frame) => {
            logging::error("  MapToError::PageAlreadyMapped");
            let addr = old_frame.start_address().as_u64();
            logging::info_u64("   already_mapped_phys_addr", addr);
        }
    }
}

fn log_unmap_error(err: UnmapError) {
    match err {
        UnmapError::PageNotMapped => {
            logging::error("  UnmapError::PageNotMapped");
        }
        UnmapError::InvalidFrameAddress(phys_addr) => {
            logging::error("  UnmapError::InvalidFrameAddress");
            let addr: PhysAddr = phys_addr;
            logging::info_u64("   invalid_frame_phys_addr", addr.as_u64());
        }
        UnmapError::ParentEntryHugePage => {
            logging::error("  UnmapError::ParentEntryHugePage");
        }
    }
}

/// 抽象API: アドレス空間を切り替える
pub fn switch_address_space(root: Option<MyPhysFrame>) {
    match root {
        Some(frame) => {
            logging::info("switch_address_space: would switch to root_page_frame");
            logging::info_u64(" root_page_frame_index", frame.number);

            if ALLOW_REAL_CR3_SWITCH.load(Ordering::Relaxed) {
                let phys = PhysAddr::new(frame.start_address().0);
                let x86_frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(phys);

                unsafe {
                    Cr3::write(x86_frame, Cr3Flags::empty());
                }

                logging::info("switch_address_space: CR3 switched (real)");
            } else {
                logging::info("switch_address_space: CR3 switch skipped (safety not enabled)");
            }
        }
        None => {
            logging::info("switch_address_space: no root_page_frame (None)");
        }
    }
}

/// User 用 PML4 を「現CR3の上位半分コピー」で初期化する
pub fn init_user_pml4_from_current(new_root: MyPhysFrame) {
    use x86_64::structures::paging::PageTable;

    let (cur_l4, _) = Cr3::read();
    let cur_phys = cur_l4.start_address().as_u64();

    let new_phys = new_root.start_address().0;

    let cur_ptr = unsafe { phys_u64_to_virt_ptr(cur_phys) as *const PageTable };
    let new_ptr = unsafe { phys_u64_to_virt_ptr(new_phys) as *mut PageTable };

    unsafe {
        let cur = &*cur_ptr;
        let new = &mut *new_ptr;

        for i in 0..256 {
            new[i].set_unused();
        }
        for i in 256..512 {
            new[i] = cur[i].clone();
        }
    }
}
