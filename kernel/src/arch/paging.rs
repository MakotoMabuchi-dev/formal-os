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
// - map/unmap の失敗は Result として返し、上位で fail-stop できるようにする
//
// 分離前進（重要）:
// - user root から low-half の“全部コピー”はしない。
// - ただし OffsetPageTable が phys_to_virt 経由でページテーブルを参照するため、
//   user root にも physmap(physical_memory_offset) の PML4 entry は必要。
// - よって user root には kernel high-half(256..512) + physmap のみコピーする。
// - 例外配送（IDT/handler/IST/TSS）が high-alias window(508..511) に依存するため、
//   user root にも high-alias window を必ずコピーする。
//
// Top3対応（今回の本命）:
// - CR3 切替の preflight 検証を入れる（RIP/RSP/physmap/alias を切替前に検証）
// - physmap と USER slot(PML4 index) の衝突を仕様として禁止（assert）
// - #PF を "guarded 区間" で捕捉して復帰し、上位で task kill へ進められるようにする
//
// 重要（今回の不具合の根）:
// - user CR3 では low-half を持たない設計。
// - guarded_user_rw_u64() は PF_GUARD_* に書き込む必要がある。
// - PF_GUARD_* が low 側に置かれると、その store 自体が #PF になる。
// - よって PF_GUARD_* / LAST_PF_* は high-alias 側アドレスを使って読み書きする。
//
// ★追加（今回の修正）:
// - user CR3 中は logging が落ちやすいので、logging なしで CR3 を戻す API
//   switch_address_space_quiet(frame) を用意する。

use bootloader::BootInfo;
use bootloader::bootinfo::MemoryRegionType;

use core::cmp::min;
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

// CR3 preflight
const ENABLE_CR3_PREFLIGHT: bool = true;

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

// -----------------------------------------------------------------------------
// #PF guard/fixup
// -----------------------------------------------------------------------------

static PF_GUARD_ACTIVE: AtomicU64 = AtomicU64::new(0);
static PF_GUARD_RECOVER_RIP: AtomicU64 = AtomicU64::new(0);
static PF_GUARD_HIT: AtomicU64 = AtomicU64::new(0);

static LAST_PF_VALID: AtomicU64 = AtomicU64::new(0);
static LAST_PF_ADDR: AtomicU64 = AtomicU64::new(0);
static LAST_PF_ERR: AtomicU64 = AtomicU64::new(0);
static LAST_PF_RIP: AtomicU64 = AtomicU64::new(0);
static LAST_PF_RSP: AtomicU64 = AtomicU64::new(0);
static LAST_PF_IS_USER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct PageFaultInfo {
    pub addr: u64,
    pub err: u64,
    pub rip: u64,
    pub rsp: u64,
    pub is_user_fault: bool,
}

#[inline(always)]
unsafe fn guard_u64_ptr(addr_u64: u64) -> *mut u64 {
    // addr_u64 が low-half(=alias 元)なら high-alias に寄せる。
    // それ以外（すでに high 側など）はそのまま使う。
    let idx = virt_layout::pml4_index(addr_u64);

    // alias window は src=0..KERNEL_ALIAS_MAX_COPY_COUNT-1 を dst=508.. へコピーする設計。
    // したがって low PML4 idx が 0..=3 の場合は high-alias が存在する。
    if idx < virt_layout::KERNEL_ALIAS_MAX_COPY_COUNT {
        let high = virt_layout::kernel_high_alias_of_low(addr_u64);
        return high as *mut u64;
    }

    addr_u64 as *mut u64
}


pub fn record_page_fault(info: PageFaultInfo) {
    unsafe {
        let addr = guard_u64_ptr(&LAST_PF_ADDR as *const AtomicU64 as u64);
        let err  = guard_u64_ptr(&LAST_PF_ERR  as *const AtomicU64 as u64);
        let rip  = guard_u64_ptr(&LAST_PF_RIP  as *const AtomicU64 as u64);
        let rsp  = guard_u64_ptr(&LAST_PF_RSP  as *const AtomicU64 as u64);
        let isu  = guard_u64_ptr(&LAST_PF_IS_USER as *const AtomicU64 as u64);
        let val  = guard_u64_ptr(&LAST_PF_VALID as *const AtomicU64 as u64);

        core::ptr::write_volatile(addr, info.addr);
        core::ptr::write_volatile(err, info.err);
        core::ptr::write_volatile(rip, info.rip);
        core::ptr::write_volatile(rsp, info.rsp);
        core::ptr::write_volatile(isu, if info.is_user_fault { 1 } else { 0 });

        core::ptr::write_volatile(val, 1);
    }
}

pub fn take_last_page_fault() -> Option<PageFaultInfo> {
    unsafe {
        let val = guard_u64_ptr(&LAST_PF_VALID as *const AtomicU64 as u64);
        let was_valid = core::ptr::read_volatile(val);
        if was_valid == 0 {
            return None;
        }
        core::ptr::write_volatile(val, 0);

        let addr = core::ptr::read_volatile( guard_u64_ptr(&LAST_PF_ADDR as *const AtomicU64 as u64));
        let err  = core::ptr::read_volatile( guard_u64_ptr(&LAST_PF_ERR  as *const AtomicU64 as u64));
        let rip  = core::ptr::read_volatile( guard_u64_ptr(&LAST_PF_RIP  as *const AtomicU64 as u64));
        let rsp  = core::ptr::read_volatile( guard_u64_ptr(&LAST_PF_RSP  as *const AtomicU64 as u64));
        let isu  = core::ptr::read_volatile( guard_u64_ptr(&LAST_PF_IS_USER as *const AtomicU64 as u64)) != 0;

        Some(PageFaultInfo { addr, err, rip, rsp, is_user_fault: isu })
    }
}

pub fn is_user_space_addr_u64(addr: u64) -> bool {
    addr >= USER_SPACE_BASE && addr < (USER_SPACE_BASE + USER_SPACE_SIZE)
}

pub fn pf_guard_try_fixup() -> Option<u64> {
    unsafe {
        let active  = guard_u64_ptr(&PF_GUARD_ACTIVE as *const AtomicU64 as u64);
        let recover = guard_u64_ptr(&PF_GUARD_RECOVER_RIP as *const AtomicU64 as u64);
        let hit     = guard_u64_ptr(&PF_GUARD_HIT as *const AtomicU64 as u64);

        if core::ptr::read_volatile(active) == 0 {
            return None;
        }
        let rip = core::ptr::read_volatile(recover);
        if rip == 0 {
            return None;
        }

        core::ptr::write_volatile(hit, 1);
        core::ptr::write_volatile(active, 0);
        Some(rip)
    }
}

pub fn guarded_user_rw_u64(ptr: *mut u64, value: u64) -> Result<u64, PageFaultInfo> {
    unsafe {
        core::ptr::write_volatile( guard_u64_ptr(&LAST_PF_VALID as *const AtomicU64 as u64), 0);
        core::ptr::write_volatile( guard_u64_ptr(&PF_GUARD_HIT as *const AtomicU64 as u64), 0);
    }

    let recover_ptr: *mut u64 = unsafe { guard_u64_ptr(&PF_GUARD_RECOVER_RIP as *const AtomicU64 as u64) };
    let active_ptr:  *mut u64 = unsafe { guard_u64_ptr(&PF_GUARD_ACTIVE     as *const AtomicU64 as u64) };
    let hit_ptr:     *mut u64 = unsafe { guard_u64_ptr(&PF_GUARD_HIT        as *const AtomicU64 as u64) };

    let mut read_back: u64;

    unsafe {
        core::arch::asm!(
        "lea rax, [rip + 2f]",
        "mov qword ptr [{recover}], rax",
        "mov qword ptr [{active}], 1",

        "mov qword ptr [{p}], {v}",
        "mov {out}, qword ptr [{p}]",

        "mov qword ptr [{active}], 0",
        "2:",
        recover = in(reg) recover_ptr,
        active  = in(reg) active_ptr,
        p = in(reg) ptr,
        v = in(reg) value,
        out = out(reg) read_back,
        out("rax") _,
        options(nostack, preserves_flags)
        );
    }

    let hit = unsafe { core::ptr::read_volatile(hit_ptr) };
    if hit != 0 {
        if let Some(info) = take_last_page_fault() {
            return Err(info);
        }
        return Err(PageFaultInfo { addr: 0, err: 0, rip: 0, rsp: 0, is_user_fault: true });
    }

    Ok(read_back)
}

// -----------------------------------------------------------------------------
// paging core
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum PagingApplyError {
    MapFailed,
    UnmapFailed,
}

#[inline]
fn is_user_space_addr(v: VirtAddr) -> bool {
    is_user_space_addr_u64(v.as_u64())
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

// physmap と USER slot の衝突を仕様として禁止（assert）
fn assert_no_physmap_user_slot_collision() {
    let physmap_off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    let physmap_pml4 = virt_layout::pml4_index(physmap_off);

    if USER_PML4_INDEX >= 256 {
        logging::error("SPEC VIOLATION: USER_PML4_INDEX must be < 256");
        logging::info_u64("USER_PML4_INDEX", USER_PML4_INDEX as u64);
        panic!("USER_PML4_INDEX must be < 256");
    }

    if physmap_pml4 == USER_PML4_INDEX {
        logging::error("SPEC VIOLATION: physmap PML4 index collides with USER slot");
        logging::info_u64("physmap_pml4_index", physmap_pml4 as u64);
        logging::info_u64("USER_PML4_INDEX", USER_PML4_INDEX as u64);
        panic!("physmap collides with USER slot (PML4 index)");
    }

    if physmap_pml4 < 256 {
        let end = min(physmap_pml4 + PHYSMAP_PML4_COPY_COUNT, 256);
        if (physmap_pml4..end).contains(&USER_PML4_INDEX) {
            logging::error("SPEC VIOLATION: physmap PML4 copy range overlaps USER slot");
            logging::info_u64("physmap_pml4_start", physmap_pml4 as u64);
            logging::info_u64("physmap_pml4_end", end as u64);
            logging::info_u64("USER_PML4_INDEX", USER_PML4_INDEX as u64);
            panic!("physmap copy range overlaps USER slot");
        }
    }
}

pub fn init(boot_info: &'static BootInfo) {
    logging::info("arch::paging::init: start");

    PHYSICAL_MEMORY_OFFSET.store(boot_info.physical_memory_offset, Ordering::Relaxed);
    assert_no_physmap_user_slot_collision();

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

// -----------------------------------------------------------------------------
// CR3 preflight
// -----------------------------------------------------------------------------

fn read_rip_rsp_rbp() -> (u64, u64, u64) {
    unsafe {
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
    }
}

unsafe fn translate_u64(mapper: &OffsetPageTable<'static>, v: u64) -> u64 {
    mapper.translate_addr(VirtAddr::new(v)).map(|p| p.as_u64()).unwrap_or(0)
}

fn preflight_check_before_cr3_write(target: MyPhysFrame) {
    if !ENABLE_REAL_PAGING || !ENABLE_CR3_PREFLIGHT {
        return;
    }

    assert_no_physmap_user_slot_collision();

    let target_phys_u64 = target.start_address().0;

    let (cur_l4, _) = Cr3::read();
    let cur_phys_u64 = cur_l4.start_address().as_u64();
    if cur_phys_u64 == target_phys_u64 {
        return;
    }

    let (rip, rsp, rbp) = read_rip_rsp_rbp();

    let code_low = GUARD_CODE_VIRT.load(Ordering::Relaxed);
    let stack_low = GUARD_STACK_VIRT.load(Ordering::Relaxed);

    let code_high = GUARD_CODE_HIGH_VIRT.load(Ordering::Relaxed);
    let stack_high = GUARD_STACK_HIGH_VIRT.load(Ordering::Relaxed);

    let exp_code_phys = GUARD_CODE_PHYS.load(Ordering::Relaxed);
    let exp_stack_phys = GUARD_STACK_PHYS.load(Ordering::Relaxed);

    unsafe {
        let tgt_mapper = init_offset_page_table_for_root(target);

        // 必須: RIP/RSP が target で引けること
        let rip_phys_tgt = translate_u64(&tgt_mapper, rip);
        let rsp_phys_tgt = translate_u64(&tgt_mapper, rsp);
        let rbp_phys_tgt = translate_u64(&tgt_mapper, rbp);

        if rip_phys_tgt == 0 || rsp_phys_tgt == 0 {
            logging::error("CR3 preflight: target translate failed (RIP/RSP)");
            logging::info_u64("rip", rip);
            logging::info_u64("rsp", rsp);
            logging::info_u64("rbp", rbp);
            logging::info_u64("rip_phys_tgt", rip_phys_tgt);
            logging::info_u64("rsp_phys_tgt", rsp_phys_tgt);
            logging::info_u64("rbp_phys_tgt", rbp_phys_tgt);
            panic!("CR3 preflight failed (target missing RIP/RSP mapping)");
        }

        // 参考: RBP は必須にしない
        if rbp != 0 && rbp_phys_tgt == 0 {
            logging::info("CR3 preflight: note: target RBP translate failed (non-fatal in this phase)");
            logging::info_u64("rbp", rbp);
            logging::info_u64("rbp_phys_tgt", rbp_phys_tgt);
        }

        // physmap が target に存在すること
        let pml4_phys = PhysAddr::new(target_phys_u64);
        let pml4_virt = phys_to_virt(pml4_phys);
        let pml4_phys_got = tgt_mapper.translate_addr(pml4_virt).map(|p| p.as_u64()).unwrap_or(0);
        if pml4_phys_got != target_phys_u64 {
            logging::error("CR3 preflight: physmap missing/broken in target root");
            logging::info_u64("target_pml4_phys", target_phys_u64);
            logging::info_u64("target_pml4_virt", pml4_virt.as_u64());
            logging::info_u64("translated_phys", pml4_phys_got);
            panic!("CR3 preflight failed (physmap missing)");
        }

        // guard(low) は user root では存在しない（仕様）
        let is_user_root = {
            let user_slot_phys = translate_u64(&tgt_mapper, virt_layout::pml4_index_base_addr(USER_PML4_INDEX));
            user_slot_phys == 0
        };

        if !is_user_root {
            if code_low != 0 && stack_low != 0 && exp_code_phys != 0 && exp_stack_phys != 0 {
                let code_phys_tgt = translate_u64(&tgt_mapper, code_low);
                let stack_phys_tgt = translate_u64(&tgt_mapper, stack_low);
                if code_phys_tgt != exp_code_phys || stack_phys_tgt != exp_stack_phys {
                    logging::error("CR3 preflight: guard(low) phys mismatch in kernel root");
                    panic!("CR3 preflight failed (guard low mismatch)");
                }
            }
        } else {
            logging::info("CR3 preflight: skipping guard(low) check for user root (by design)");
        }

        // guard(high) は user root でも必須（IDT/TSS/stack がここに依存）
        if code_high != 0 && stack_high != 0 && exp_code_phys != 0 && exp_stack_phys != 0 {
            let code_phys_tgt = translate_u64(&tgt_mapper, code_high);
            let stack_phys_tgt = translate_u64(&tgt_mapper, stack_high);
            if code_phys_tgt != exp_code_phys || stack_phys_tgt != exp_stack_phys {
                logging::error("CR3 preflight: guard(high) phys mismatch in target");
                logging::info_u64("expected_code_phys", exp_code_phys);
                logging::info_u64("got_code_phys", code_phys_tgt);
                logging::info_u64("expected_stack_phys", exp_stack_phys);
                logging::info_u64("got_stack_phys", stack_phys_tgt);
                panic!("CR3 preflight failed (guard high mismatch)");
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Public APIs used by kernel/*
// -----------------------------------------------------------------------------

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

/// logging を一切行わずに CR3 を書き換える（user CR3 中からの復帰用）
pub fn switch_address_space_quiet(frame: MyPhysFrame) {
    let phys = PhysAddr::new(frame.start_address().0);
    let x86_frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(phys);

    let (_cur_frame, cur_flags) = Cr3::read();
    unsafe { Cr3::write(x86_frame, cur_flags); }

    // ★ログなし検証（fail-stop）
    let (now, _) = Cr3::read();
    if now.start_address().as_u64() != frame.start_address().0 {
        panic!("CR3 write failed (readback mismatch)");
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

            preflight_check_before_cr3_write(frame);

            // ★ここに寄せる
            switch_address_space_quiet(frame);
        }
        None => {
            logging::info("switch_address_space: no root_page_frame (None)");
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

// -----------------------------------------------------------------------------
// High-alias install and exec test
// -----------------------------------------------------------------------------

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

// -----------------------------------------------------------------------------
// map/unmap apply API
// -----------------------------------------------------------------------------

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
                        Ok(())
                    }
                    Err(e) => {
                        logging::error("map_to: ERROR");
                        log_map_to_error(e);
                        Err(PagingApplyError::MapFailed)
                    }
                }
            } else {
                Ok(())
            }
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
                        Ok(())
                    }
                    Err(e) => {
                        logging::error("unmap: ERROR");
                        log_unmap_error(e);
                        Err(PagingApplyError::UnmapFailed)
                    }
                }
            } else {
                Ok(())
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

// -----------------------------------------------------------------------------
// user root init
// -----------------------------------------------------------------------------

pub fn init_user_pml4_from_current(new_root: MyPhysFrame) {
    let (cur_l4, _) = Cr3::read();
    let cur_phys = cur_l4.start_address().as_u64();
    let new_phys = new_root.start_address().0;

    let cur_ptr = unsafe { phys_u64_to_virt_ptr(cur_phys) as *const PageTable };
    let new_ptr = unsafe { phys_u64_to_virt_ptr(new_phys) as *mut PageTable };

    let physmap_off = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    let physmap_pml4 = virt_layout::pml4_index(physmap_off);

    assert_no_physmap_user_slot_collision();

    let alias_base = virt_layout::KERNEL_ALIAS_DST_PML4_BASE_INDEX;
    let alias_cnt = {
        let n = ALIAS_COPY_COUNT.load(Ordering::Relaxed);
        if n == 0 { virt_layout::KERNEL_ALIAS_MAX_COPY_COUNT } else { min(n, virt_layout::KERNEL_ALIAS_MAX_COPY_COUNT) }
    };

    unsafe {
        let cur_p4 = &*cur_ptr;
        let user_p4 = &mut *new_ptr;

        for i in 0..512 {
            user_p4[i].set_unused();
        }

        // 1) physmap（OffsetPageTable が page table walk できるために必要）
        for i in physmap_pml4..min(physmap_pml4 + PHYSMAP_PML4_COPY_COUNT, 256) {
            if cur_p4[i].is_unused() { continue; }
            if cur_p4[i].flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                logging::error("init_user_pml4_from_current: physmap entry has USER_ACCESSIBLE; abort");
                logging::info_u64("pml4_index", i as u64);
                panic!("physmap pml4 entry contains USER_ACCESSIBLE");
            }
            user_p4[i] = cur_p4[i].clone();
        }

        // 2) kernel high-half（通常の kernel 領域）
        for i in 256..512 {
            if cur_p4[i].is_unused() { continue; }
            if cur_p4[i].flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                logging::error("init_user_pml4_from_current: kernel pml4 entry has USER_ACCESSIBLE; abort");
                logging::info_u64("pml4_index", i as u64);
                panic!("kernel pml4 entry contains USER_ACCESSIBLE");
            }
            user_p4[i] = cur_p4[i].clone();
        }

        // 2.5) high-alias window（IDT/IST/TSS/handler が依存）
        for i in 0..alias_cnt {
            let idx = alias_base + i;
            if idx >= 512 { break; }
            if cur_p4[idx].is_unused() { continue; }
            if cur_p4[idx].flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                logging::error("init_user_pml4_from_current: alias window has USER_ACCESSIBLE; abort");
                logging::info_u64("pml4_index", idx as u64);
                panic!("alias window pml4 entry contains USER_ACCESSIBLE");
            }
            user_p4[idx] = cur_p4[idx].clone();
        }

        // 3) USER slot は空
        logging::info("init_user_pml4_from_current: clearing user pml4 entry");
        logging::info_u64("pml4_index", USER_PML4_INDEX as u64);
        user_p4[USER_PML4_INDEX].set_unused();

        logging::info("init_user_pml4_from_current: copied kernel high-half + physmap (+alias window)");
        logging::info_u64("kernel_pml4_base", 256);
        logging::info_u64("physmap_pml4_index", physmap_pml4 as u64);
        logging::info_u64("alias_dst_base_pml4", alias_base as u64);
        logging::info_u64("alias_copy_count", alias_cnt as u64);
    }

    if ENABLE_REAL_PAGING && ENABLE_CR3_PREFLIGHT {
        preflight_check_before_cr3_write(new_root);
    }
}

// -----------------------------------------------------------------------------
// debug helpers used by kernel/entry.rs
// -----------------------------------------------------------------------------

pub fn debug_log_execution_context(tag: &str) {
    let (rip, rsp, rbp) = read_rip_rsp_rbp();

    logging::info("exec_context:");
    logging::info(tag);
    logging::info_u64("rip", rip);
    logging::info_u64("rsp", rsp);
    logging::info_u64("rbp", rbp);
    logging::info_u64("rip_pml4", virt_layout::pml4_index(rip) as u64);
    logging::info_u64("rsp_pml4", virt_layout::pml4_index(rsp) as u64);
    logging::info_u64("rbp_pml4", virt_layout::pml4_index(rbp) as u64);
}

pub fn enter_kernel_high_alias(
    entry: extern "C" fn(&'static BootInfo) -> !,
    boot_info: &'static BootInfo,
) -> ! {
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
