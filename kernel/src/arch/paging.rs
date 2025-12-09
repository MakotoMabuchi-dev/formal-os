// src/arch/paging.rs
//
// 役割:
// - x86_64 アーキテクチャ向けのページング処理をまとめる場所。
// - ページテーブルの実際の操作（unsafe）は最終的にここに集約する。
// 現時点:
// - REAL PAGING を有効にして map_to/unmap を呼び出し、
//   map_to が成功した場合は実際に仮想アドレスへの read/write テストを行う。
// - さらに BootInfo の memory_map をログにダンプして、
//   どの物理アドレス領域が Usable/Reserved かを確認できるようにする。

use bootloader::BootInfo;
use bootloader::bootinfo::MemoryRegionType;
use crate::logging;
use crate::mm::PhysicalMemoryManager;
use crate::mem::paging::{MemAction, PageFlags};

use core::ptr::{read_volatile, write_volatile};

use x86_64::{
    VirtAddr,
    PhysAddr,
    registers::control::Cr3,
    structures::paging::{
        Mapper, FrameAllocator,
        Page,
        PhysFrame,
        PageTable,
        PageTableFlags,
        Size4KiB,
        OffsetPageTable,
    },
    structures::paging::mapper::{MapToError, UnmapError},
};

/// 実際にページテーブルを書き換えるかどうかのフラグ。
const ENABLE_REAL_PAGING: bool = true;

/// 仮の物理メモリオフセット（今は identity map 前提で 0）。
const PHYSICAL_MEMORY_OFFSET: u64 = 0;

/// paging サブシステムの初期化。
/// - BootInfo の memory_map をダンプして、
///   どの物理アドレス領域が Usable/Reserved かを確認できるようにする。
pub fn init(boot_info: &'static BootInfo) {
    logging::info("arch::paging::init: start");

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
            MemoryRegionType::Usable => {
                logging::info("  type = Usable");
            }
            MemoryRegionType::Reserved => {
                logging::info("  type = Reserved");
            }
            MemoryRegionType::AcpiReclaimable => {
                logging::info("  type = AcpiReclaimable");
            }
            MemoryRegionType::AcpiNvs => {
                logging::info("  type = AcpiNvs");
            }
            MemoryRegionType::BadMemory => {
                logging::info("  type = BadMemory");
            }
            other => {
                logging::info("  type = Other");
                let _ = other;
            }
        }
    }

    logging::info("arch::paging::init: memory map dump end");
    logging::info("arch::paging::init: done");
}

/// 抽象 PageFlags → x86_64 の PageTableFlags への変換。
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
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let phys = level_4_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}

/// OffsetPageTable を初期化するためのヘルパー。
pub unsafe fn init_offset_page_table(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, physical_memory_offset)
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

/// MemAction を受け取り、ページテーブルを操作するための入口。
pub unsafe fn apply_mem_action(
    action: MemAction,
    phys_mem: &mut PhysicalMemoryManager,
) {
    match action {
        MemAction::Map { page, frame, flags } => {
            logging::info("arch::paging::apply_mem_action: Map");

            let virt_addr_u64 = page.start_address().0;
            let phys_addr_u64 = frame.start_address().0;
            let x86_page: Page<Size4KiB> =
                Page::containing_address(VirtAddr::new(virt_addr_u64));
            let x86_frame: PhysFrame<Size4KiB> =
                PhysFrame::containing_address(PhysAddr::new(phys_addr_u64));
            let x86_flags = to_x86_flags(flags);

            logging::info_u64(" virt_addr", virt_addr_u64);
            logging::info_u64(" phys_addr", phys_addr_u64);
            logging::info_u64(" flags_bits", flags.bits());

            if ENABLE_REAL_PAGING {
                logging::info(" REAL PAGING: map_to() will be executed");

                let phys_offset = VirtAddr::new(PHYSICAL_MEMORY_OFFSET);
                let mut mapper = init_offset_page_table(phys_offset);
                let mut frame_alloc = KernelFrameAllocator::new(phys_mem);

                match mapper.map_to(x86_page, x86_frame, x86_flags, &mut frame_alloc) {
                    Ok(flush) => {
                        flush.flush();
                        logging::info(" map_to: OK (flush done)");

                        // ★ 実際にこの仮想アドレスにアクセスしてみるテスト
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
            let x86_page: Page<Size4KiB> =
                Page::containing_address(VirtAddr::new(virt_addr_u64));

            logging::info_u64(" virt_addr", virt_addr_u64);

            if ENABLE_REAL_PAGING {
                logging::info(" REAL PAGING: unmap() will be executed");

                let phys_offset = VirtAddr::new(PHYSICAL_MEMORY_OFFSET);
                let mut mapper = init_offset_page_table(phys_offset);

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

                let _ = phys_mem; // 将来 frame を解放するときに使うかも
            } else {
                logging::info(" REAL PAGING: disabled (ENABLE_REAL_PAGING = false)");
            }
        }
    }
}

/// map_to のエラー内容をログに出すヘルパ。
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

/// unmap のエラー内容をログに出すヘルパ。
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
