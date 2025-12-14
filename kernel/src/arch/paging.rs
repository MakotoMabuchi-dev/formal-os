// kernel/src/arch/paging.rs
//
// 役割:
// - x86_64 ページング処理の集約（unsafe を局所化）
// - low-half のままでも CR3 を安全に切替できるように guard を入れる
// - Step1/2: kernel high-alias を導入し self-check + exec test
// - Step3: high-alias に stack を切替えて kernel 本体へ移譲
//
// 設計方針:
// - 壊れていたら早めに panic（fail-stop）
// - ★B対応: map/unmap の失敗を Result として返し、上位で fail-stop できるようにする
//
// 分離前進（重要）:
// - user root から low-half の“全部コピー”はしない。
// - ただし OffsetPageTable が物理→仮想変換(phys_to_virt)でページテーブルを参照するため、
//   user root にも physmap(physical_memory_offset) の PML4 entry は必要。
// - よって kernel high-half(256..512) + physmap のみをコピーする。

use bootloader::BootInfo;
use bootloader::bootinfo::MemoryRegionType;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use x86_64::{
    PhysAddr,
    VirtAddr,
    registers::control::Cr3,
    structures::paging::{
        FrameAllocator,
        Mapper,
        OffsetPageTable,
        Page,
        PageTable,
        PageTableFlags,
        PhysFrame,
        Size4KiB,
        Translate,
    },
    structures::paging::mapper::{MapToError, UnmapError},
};

use crate::arch::virt_layout;
use crate::logging;
use crate::mm::PhysicalMemoryManager;
use crate::mem::paging::{MemAction, PageFlags};
use crate::mem::addr::PhysFrame as MyPhysFrame;

pub use crate::arch::virt_layout::{USER_PML4_INDEX, USER_SPACE_BASE, USER_SPACE_SIZE};

const ENABLE_REAL_PAGING: bool = true;
const ENABLE_HIGH_ALIAS_EXEC_TEST: bool = true;

// physmap の PML4 entry を“何個”コピーするか（安全側に少し多め）
const PHYSMAP_PML4_COPY_COUNT: usize = 4;

static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);
static ALLOW_REAL_CR3_SWITCH: AtomicBool = AtomicBool::new(false);

// low guard
static GUARD_CODE_VIRT: AtomicU64 = AtomicU64::new(0);
static GUARD_STACK_VIRT: AtomicU64 = AtomicU64::new(0);
static GUARD_CODE_PHYS: AtomicU64 = AtomicU64::new(0);
static GUARD_STACK_PHYS: AtomicU64 = AtomicU64::new(0);

// high guard (virt only; phys must match low phys)
static GUARD_CODE_HIGH_VIRT: AtomicU64 = AtomicU64::new(0);
static GUARD_STACK_HIGH_VIRT: AtomicU64 = AtomicU64::new(0);

// alias copy count（install 時に確定）
static ALIAS_COPY_COUNT: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy)]
pub enum PagingApplyError {
    MapFailed,
    UnmapFailed,
}

#[inline]
fn is_user_space_addr(v: VirtAddr) -> bool {
    let x = v.as_u64();
    x >= USER_SPACE_BASE && x < (USER_SPACE_BASE + USER_SPACE_SIZE)
}

#[inline]
fn enforce_user_mapping_policy(virt: VirtAddr, flags: PageTableFlags) {
    let in_user_slot = is_user_space_addr(virt);
    let user_accessible = flags.contains(PageTableFlags::USER_ACCESSIBLE);

    if user_accessible && !in_user_slot {
        logging::error("paging policy violation: USER mapping outside reserved user slot");
        logging::info_u64("virt_addr", virt.as_u64());
        logging::info_u64("flags_bits", flags.bits() as u64);
        panic!("USER mapping outside reserved user slot");
    }

    if !user_accessible && in_user_slot {
        logging::error("paging policy violation: KERNEL mapping inside reserved user slot");
        logging::info_u64("virt_addr", virt.as_u64());
        logging::info_u64("flags_bits", flags.bits() as u64);
        panic!("KERNEL mapping inside reserved user slot");
    }
}

#[inline]
fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    let off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    VirtAddr::new(off + phys.as_u64())
}

unsafe fn phys_u64_to_virt_ptr(phys: u64) -> *mut u8 {
    let off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    (off + phys) as *mut u8
}

pub fn init(boot_info: &'static BootInfo) {
    logging::info("arch::paging::init: start");

    PHYSICAL_MEMORY_OFFSET.store(boot_info.physical_memory_offset, Ordering::Relaxed);

    logging::info("arch::paging::init: memory map dump start");
    for (i, region) in boot_info.memory_map.iter().enumerate() {
        let start = region.range.start_frame_number * 4096;
        let end = region.range.end_frame_number * 4096;

        logging::info("mem_region:");
        logging::info_u64("index", i as u64);
        logging::info_u64("start_phys", start);
        logging::info_u64("end_phys", end);

        match region.region_type {
            MemoryRegionType::Usable => logging::info("type = Usable"),
            MemoryRegionType::Reserved => logging::info("type = Reserved"),
            MemoryRegionType::AcpiReclaimable => logging::info("type = AcpiReclaimable"),
            MemoryRegionType::AcpiNvs => logging::info("type = AcpiNvs"),
            MemoryRegionType::BadMemory => logging::info("type = BadMemory"),
            _ => logging::info("type = Other"),
        }
    }
    logging::info("arch::paging::init: memory map dump end");
    logging::info("arch::paging::init: done");
}

fn to_x86_flags(flags: PageFlags) -> PageTableFlags {
    let mut res = PageTableFlags::empty();
    if flags.contains(PageFlags::PRESENT) { res |= PageTableFlags::PRESENT; }
    if flags.contains(PageFlags::WRITABLE) { res |= PageTableFlags::WRITABLE; }
    if flags.contains(PageFlags::USER) { res |= PageTableFlags::USER_ACCESSIBLE; }
    if flags.contains(PageFlags::NO_EXEC) { res |= PageTableFlags::NO_EXECUTE; }
    res
}

unsafe fn active_level_4_table() -> &'static mut PageTable {
    let (level_4_frame, _) = Cr3::read();
    let phys = level_4_frame.start_address();
    let virt = phys_to_virt(phys);
    &mut *(virt.as_mut_ptr::<PageTable>())
}

pub unsafe fn init_offset_page_table() -> OffsetPageTable<'static> {
    let l4 = active_level_4_table();
    let offset = VirtAddr::new(PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed));
    OffsetPageTable::new(l4, offset)
}

pub unsafe fn init_offset_page_table_for_root(root: MyPhysFrame) -> OffsetPageTable<'static> {
    let pml4_phys = PhysAddr::new(root.start_address().0);
    let pml4_virt = phys_to_virt(pml4_phys);
    let pml4 = &mut *(pml4_virt.as_mut_ptr::<PageTable>());

    let offset = VirtAddr::new(PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed));
    OffsetPageTable::new(pml4, offset)
}

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

pub fn configure_cr3_switch_safety(code_addr: u64, stack_addr: u64) {
    logging::info("arch::paging::configure_cr3_switch_safety");
    logging::info_u64("code_addr", code_addr);
    logging::info_u64("stack_addr", stack_addr);

    if !ENABLE_REAL_PAGING {
        logging::info("CR3 real switch: DISABLED (real paging disabled)");
        ALLOW_REAL_CR3_SWITCH.store(false, Ordering::Relaxed);
        return;
    }

    unsafe {
        let mapper = init_offset_page_table();

        let code_p = mapper.translate_addr(VirtAddr::new(code_addr)).map(|p| p.as_u64()).unwrap_or(0);
        let stack_p = mapper.translate_addr(VirtAddr::new(stack_addr)).map(|p| p.as_u64()).unwrap_or(0);

        if code_p == 0 || stack_p == 0 {
            logging::error("CR3 real switch: DISABLED (translate failed)");
            ALLOW_REAL_CR3_SWITCH.store(false, Ordering::Relaxed);
            return;
        }

        GUARD_CODE_VIRT.store(code_addr, Ordering::Relaxed);
        GUARD_STACK_VIRT.store(stack_addr, Ordering::Relaxed);
        GUARD_CODE_PHYS.store(code_p, Ordering::Relaxed);
        GUARD_STACK_PHYS.store(stack_p, Ordering::Relaxed);

        GUARD_CODE_HIGH_VIRT.store(virt_layout::kernel_high_alias_of_low(code_addr), Ordering::Relaxed);
        GUARD_STACK_HIGH_VIRT.store(virt_layout::kernel_high_alias_of_low(stack_addr), Ordering::Relaxed);

        logging::info("CR3 real switch: ENABLED (translate-based guard)");
        logging::info_u64("expected_code_phys", code_p);
        logging::info_u64("expected_stack_phys", stack_p);

        ALLOW_REAL_CR3_SWITCH.store(true, Ordering::Relaxed);
    }
}

pub fn install_kernel_high_alias_from_current() {
    if !ENABLE_REAL_PAGING {
        logging::info("arch::paging::install_kernel_high_alias_from_current: skipped (real paging disabled)");
        return;
    }

    let code_low = GUARD_CODE_VIRT.load(Ordering::Relaxed);
    let stack_low = GUARD_STACK_VIRT.load(Ordering::Relaxed);

    let (rip, rsp, rbp) = unsafe {
        let mut rip: u64;
        let mut rsp: u64;
        let mut rbp: u64;
        core::arch::asm!(
        "lea {rip}, [rip]",
        "mov {rsp}, rsp",
        "mov {rbp}, rbp",
        rip = out(reg) rip,
        rsp = out(reg) rsp,
        rbp = out(reg) rbp,
        options(nomem, nostack, preserves_flags)
        );
        (rip, rsp, rbp)
    };

    let copy_count = virt_layout::recommend_alias_copy_count_from_addrs(&[
        code_low, stack_low, rip, rsp, rbp,
    ]);

    ALIAS_COPY_COUNT.store(copy_count, Ordering::Relaxed);

    let dst_base = virt_layout::KERNEL_ALIAS_DST_PML4_BASE_INDEX;

    logging::info("arch::paging::install_kernel_high_alias_from_current: start");
    logging::info_u64("alias_dst_base_pml4", dst_base as u64);
    logging::info_u64("alias_copy_count", copy_count as u64);

    unsafe {
        let pml4 = active_level_4_table();

        for src in 0..copy_count {
            let dst = dst_base + src;

            if pml4[src].is_unused() {
                continue;
            }

            if pml4[src].flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                logging::error("kernel alias source contains USER_ACCESSIBLE; abort");
                logging::info_u64("src_pml4_index", src as u64);
                panic!("kernel alias source contains USER_ACCESSIBLE");
            }

            pml4[dst] = pml4[src].clone();

            logging::info("installed kernel alias pml4 entry");
            logging::info_u64("src_pml4_index", src as u64);
            logging::info_u64("dst_pml4_index", dst as u64);
        }

        let (frame, flags) = Cr3::read();
        Cr3::write(frame, flags);
    }

    logging::info("arch::paging::install_kernel_high_alias_from_current: done");

    let code_p_exp = GUARD_CODE_PHYS.load(Ordering::Relaxed);
    let stack_p_exp = GUARD_STACK_PHYS.load(Ordering::Relaxed);

    if code_low != 0 && stack_low != 0 && code_p_exp != 0 && stack_p_exp != 0 {
        let code_high = virt_layout::kernel_high_alias_of_low(code_low);
        let stack_high = virt_layout::kernel_high_alias_of_low(stack_low);

        unsafe {
            let mapper = init_offset_page_table();
            let code_p = mapper.translate_addr(VirtAddr::new(code_high)).map(|p| p.as_u64()).unwrap_or(0);
            let stack_p = mapper.translate_addr(VirtAddr::new(stack_high)).map(|p| p.as_u64()).unwrap_or(0);

            if code_p != code_p_exp || stack_p != stack_p_exp {
                logging::error("kernel high-alias self-check: FAILED");
                logging::info_u64("expected_code_phys", code_p_exp);
                logging::info_u64("actual_code_phys", code_p);
                logging::info_u64("expected_stack_phys", stack_p_exp);
                logging::info_u64("actual_stack_phys", stack_p);
                panic!("kernel high-alias mapping mismatch");
            }
        }

        logging::info("kernel high-alias self-check: OK");
        logging::info_u64("code_high_virt", virt_layout::kernel_high_alias_of_low(code_low));
        logging::info_u64("stack_high_virt", virt_layout::kernel_high_alias_of_low(stack_low));
    }

    if ENABLE_HIGH_ALIAS_EXEC_TEST {
        run_kernel_high_alias_exec_test();
    }
}

type HighAliasExecTestFn = extern "C" fn(u64) -> u64;

extern "C" fn kernel_high_alias_exec_test_target(x: u64) -> u64 {
    x.wrapping_add(0x1111_2222_3333_4444u64)
}

fn run_kernel_high_alias_exec_test() {
    let low_fn: HighAliasExecTestFn = kernel_high_alias_exec_test_target;
    let low_addr = low_fn as usize as u64;
    let high_addr = virt_layout::kernel_high_alias_of_low(low_addr);

    let high_fn: HighAliasExecTestFn = unsafe { core::mem::transmute(high_addr as usize) };

    let arg = 0x0123_4567_89AB_CDEFu64;
    let expected = kernel_high_alias_exec_test_target(arg);
    let got = high_fn(arg);

    if got != expected {
        logging::error("kernel high-alias exec test: FAILED");
        logging::info_u64("low_fn_addr", low_addr);
        logging::info_u64("high_fn_addr", high_addr);
        logging::info_u64("expected", expected);
        logging::info_u64("got", got);
        panic!("kernel high-alias exec test failed");
    }

    logging::info("kernel high-alias exec test: OK");
    logging::info_u64("low_fn_addr", low_addr);
    logging::info_u64("high_fn_addr", high_addr);
}

pub unsafe fn apply_mem_action(
    action: MemAction,
    phys_mem: &mut PhysicalMemoryManager,
) -> Result<(), PagingApplyError> {
    apply_mem_action_with_mapper(action, None, phys_mem)
}

pub unsafe fn apply_mem_action_in_root(
    action: MemAction,
    root: MyPhysFrame,
    phys_mem: &mut PhysicalMemoryManager,
) -> Result<(), PagingApplyError> {
    apply_mem_action_with_mapper(action, Some(root), phys_mem)
}

unsafe fn apply_mem_action_with_mapper(
    action: MemAction,
    root: Option<MyPhysFrame>,
    phys_mem: &mut PhysicalMemoryManager,
) -> Result<(), PagingApplyError> {
    match action {
        MemAction::Map { page, frame, flags } => {
            logging::info("arch::paging::apply_mem_action: Map");

            let mut virt_u64 = page.start_address().0;
            let phys_u64 = frame.start_address().0;

            let xflags = to_x86_flags(flags);

            if xflags.contains(PageTableFlags::USER_ACCESSIBLE) {
                virt_u64 = USER_SPACE_BASE + virt_u64;
            }

            let virt = VirtAddr::new(virt_u64);
            enforce_user_mapping_policy(virt, xflags);

            logging::info_u64("virt_addr", virt_u64);
            logging::info_u64("phys_addr", phys_u64);
            logging::info_u64("flags_bits", xflags.bits() as u64);

            let page4k: Page<Size4KiB> = Page::containing_address(virt);
            let frame4k: PhysFrame<Size4KiB> = PhysFrame::containing_address(PhysAddr::new(phys_u64));

            if ENABLE_REAL_PAGING {
                logging::info("REAL PAGING: map_to() will be executed");

                let mut mapper = match root {
                    Some(r) => init_offset_page_table_for_root(r),
                    None => init_offset_page_table(),
                };
                let mut alloc = KernelFrameAllocator::new(phys_mem);

                match mapper.map_to(page4k, frame4k, xflags, &mut alloc) {
                    Ok(flush) => {
                        flush.flush();
                        logging::info("map_to: OK (flush done)");
                        return Ok(());
                    }
                    Err(e) => {
                        logging::error("map_to: ERROR");
                        log_map_to_error(e);
                        return Err(PagingApplyError::MapFailed);
                    }
                }
            }

            Ok(())
        }

        MemAction::Unmap { page } => {
            logging::info("arch::paging::apply_mem_action: Unmap");

            let mut virt_u64 = page.start_address().0;
            if root.is_some() {
                virt_u64 = USER_SPACE_BASE + virt_u64;
            }

            logging::info_u64("virt_addr", virt_u64);

            let page4k: Page<Size4KiB> = Page::containing_address(VirtAddr::new(virt_u64));

            if ENABLE_REAL_PAGING {
                logging::info("REAL PAGING: unmap() will be executed");

                let mut mapper = match root {
                    Some(r) => init_offset_page_table_for_root(r),
                    None => init_offset_page_table(),
                };

                match mapper.unmap(page4k) {
                    Ok((_f, flush)) => {
                        flush.flush();
                        logging::info("unmap: OK (flush done)");
                        return Ok(());
                    }
                    Err(e) => {
                        logging::error("unmap: ERROR");
                        log_unmap_error(e);
                        return Err(PagingApplyError::UnmapFailed);
                    }
                }
            }

            Ok(())
        }
    }
}

pub fn debug_translate_in_root(root: MyPhysFrame, virt_addr_u64: u64) {
    if !ENABLE_REAL_PAGING {
        logging::info("debug_translate_in_root: REAL PAGING disabled");
        return;
    }

    unsafe {
        let mapper = init_offset_page_table_for_root(root);
        let v = VirtAddr::new(virt_addr_u64);
        match mapper.translate_addr(v) {
            Some(p) => {
                logging::info("translate: OK");
                logging::info_u64("virt_addr", virt_addr_u64);
                logging::info_u64("phys_addr", p.as_u64());
            }
            None => {
                logging::info("translate: NONE (not mapped)");
                logging::info_u64("virt_addr", virt_addr_u64);
            }
        }
    }
}

fn log_map_to_error(err: MapToError<Size4KiB>) {
    match err {
        MapToError::FrameAllocationFailed => logging::error("MapToError::FrameAllocationFailed"),
        MapToError::ParentEntryHugePage => logging::error("MapToError::ParentEntryHugePage"),
        MapToError::PageAlreadyMapped(old) => {
            logging::error("MapToError::PageAlreadyMapped");
            logging::info_u64("already_mapped_phys_addr", old.start_address().as_u64());
        }
    }
}

fn log_unmap_error(err: UnmapError) {
    match err {
        UnmapError::PageNotMapped => logging::error("UnmapError::PageNotMapped"),
        UnmapError::InvalidFrameAddress(p) => {
            logging::error("UnmapError::InvalidFrameAddress");
            logging::info_u64("invalid_frame_phys_addr", PhysAddr::from(p).as_u64());
        }
        UnmapError::ParentEntryHugePage => logging::error("UnmapError::ParentEntryHugePage"),
    }
}

pub fn switch_address_space(root: Option<MyPhysFrame>) {
    match root {
        Some(frame) => {
            logging::info("switch_address_space: would switch to root_page_frame");
            logging::info_u64("root_page_frame_index", frame.number);

            if !ALLOW_REAL_CR3_SWITCH.load(Ordering::Relaxed) {
                logging::info("switch_address_space: CR3 switch skipped (guard disabled)");
                return;
            }

            let phys = PhysAddr::new(frame.start_address().0);
            let x86_frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(phys);

            let (_cur_frame, cur_flags) = Cr3::read();
            unsafe { Cr3::write(x86_frame, cur_flags); }

            logging::info("switch_address_space: CR3 switched (real)");
        }
        None => logging::info("switch_address_space: no root_page_frame (None)"),
    }
}

/// User root の初期化（physmap を含めて最小限コピー）
pub fn init_user_pml4_from_current(new_root: MyPhysFrame) {
    let (cur_l4, _) = Cr3::read();
    let cur_phys = cur_l4.start_address().as_u64();
    let new_phys = new_root.start_address().0;

    let cur_ptr = unsafe { phys_u64_to_virt_ptr(cur_phys) as *const PageTable };
    let new_ptr = unsafe { phys_u64_to_virt_ptr(new_phys) as *mut PageTable };

    let physmap_off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    let physmap_pml4 = virt_layout::pml4_index(physmap_off);

    unsafe {
        let cur_p4 = &*cur_ptr;
        let user_p4 = &mut *new_ptr;

        for i in 0..512 {
            user_p4[i].set_unused();
        }

        // 1) physmap をコピー（これが無いと user CR3 中にページテーブルを触れない）
        for i in physmap_pml4..core::cmp::min(physmap_pml4 + PHYSMAP_PML4_COPY_COUNT, 256) {
            if cur_p4[i].is_unused() {
                continue;
            }
            if cur_p4[i].flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                logging::error("init_user_pml4_from_current: physmap entry has USER_ACCESSIBLE; abort");
                logging::info_u64("pml4_index", i as u64);
                panic!("physmap pml4 entry contains USER_ACCESSIBLE");
            }
            user_p4[i] = cur_p4[i].clone();
        }

        // 2) kernel high-half をコピー（high-alias 508..511 も含まれる）
        for i in 256..512 {
            if cur_p4[i].is_unused() {
                continue;
            }
            if cur_p4[i].flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                logging::error("init_user_pml4_from_current: kernel pml4 entry has USER_ACCESSIBLE; abort");
                logging::info_u64("pml4_index", i as u64);
                panic!("kernel pml4 entry contains USER_ACCESSIBLE");
            }
            user_p4[i] = cur_p4[i].clone();
        }

        // 3) user slot は空
        logging::info("init_user_pml4_from_current: clearing user pml4 entry");
        logging::info_u64("pml4_index", USER_PML4_INDEX as u64);
        user_p4[USER_PML4_INDEX as usize].set_unused();

        logging::info("init_user_pml4_from_current: copied kernel high-half + physmap");
        logging::info_u64("kernel_pml4_base", 256);
        logging::info_u64("physmap_pml4_index", physmap_pml4 as u64);
    }
}

pub fn debug_log_execution_context(tag: &str) {
    let (rip, rsp, rbp) = unsafe {
        let mut rip: u64;
        let mut rsp: u64;
        let mut rbp: u64;
        core::arch::asm!(
        "lea {rip}, [rip]",
        "mov {rsp}, rsp",
        "mov {rbp}, rbp",
        rip = out(reg) rip,
        rsp = out(reg) rsp,
        rbp = out(reg) rbp,
        options(nomem, nostack, preserves_flags)
        );
        (rip, rsp, rbp)
    };

    logging::info("exec_context:");
    logging::info(tag);
    logging::info_u64("rip", rip);
    logging::info_u64("rsp", rsp);
    logging::info_u64("rbp", rbp);
    logging::info_u64("rip_pml4", virt_layout::pml4_index(rip) as u64);
    logging::info_u64("rsp_pml4", virt_layout::pml4_index(rsp) as u64);
    logging::info_u64("rbp_pml4", virt_layout::pml4_index(rbp) as u64);
}

pub fn enter_kernel_high_alias(entry: extern "C" fn(&'static BootInfo) -> !, boot_info: &'static BootInfo) -> ! {
    logging::info("enter_kernel_high_alias: switching stack and CALL high entry");

    let low_entry = entry as usize as u64;
    let high_entry = virt_layout::kernel_high_alias_of_low(low_entry);

    let (rsp_low, rbp_low) = unsafe {
        let mut rsp: u64;
        let mut rbp: u64;
        core::arch::asm!(
        "mov {rsp}, rsp",
        "mov {rbp}, rbp",
        rsp = out(reg) rsp,
        rbp = out(reg) rbp,
        options(nomem, nostack, preserves_flags)
        );
        (rsp, rbp)
    };

    let rsp_high = virt_layout::kernel_high_alias_of_low(rsp_low) & !0xFu64;
    let rbp_high = virt_layout::kernel_high_alias_of_low(rbp_low);

    logging::info_u64("low_entry", low_entry);
    logging::info_u64("high_entry", high_entry);
    logging::info_u64("rsp_low", rsp_low);
    logging::info_u64("rsp_high_aligned", rsp_high);
    logging::info_u64("rbp_low", rbp_low);
    logging::info_u64("rbp_high", rbp_high);

    unsafe {
        core::arch::asm!(
        "mov rsp, {new_rsp}",
        "mov rbp, {new_rbp}",
        "mov rdi, {arg0}",
        "call {target}",
        new_rsp = in(reg) rsp_high,
        new_rbp = in(reg) rbp_high,
        arg0 = in(reg) boot_info as *const BootInfo,
        target = in(reg) high_entry,
        options(noreturn)
        );
    }
}
