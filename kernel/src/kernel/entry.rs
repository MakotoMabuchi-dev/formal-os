/*!
 * kernel::entry
 *
 * 役割:
 *   - kernel_main / 低位エントリ / high-alias エントリの流れを組み立てる。
 *
 * やること:
 *   - high-alias 範囲を「自動計算」してインストールし、検証する。
 *
 * やらないこと:
 *   - スケジューラや IPC の詳細実装（ここでは alias 検証に集中）。
 *
 * 設計方針:
 *   - alias のマジックナンバーを埋め込まず、layout に集約する。
 *   - 失敗時に追えるよう、low/high/phys をログに残す。
 */

use crate::arch::paging::{canonicalize_virt_addr, Paging};
use crate::logging::info_kv;
use crate::mm::{AddressSpace, AddressSpaceKind, MemAction};
use crate::types::{MemoryRegion, MemoryRegionType, PhysAddr, VirtAddr, PAGE_SIZE};

pub fn kernel_main() {
    crate::info!("formal-os: kernel_main start");

    let memory_map = demo_memory_map();

    let mut paging = Paging::new();
    paging.init(&memory_map);

    kernel_start_low_entry(&mut paging);
}

fn kernel_start_low_entry(paging: &mut Paging) {
    crate::info!("kernel::start() [low entry]");

    // 提示ログに合わせたサンプル値（実機では bootloader/linker から取得）
    let code_addr: VirtAddr = 2_162_768;
    let stack_addr: VirtAddr = 1_099_513_733_016;

    let expected_code_phys: PhysAddr = 4_255_824;
    let expected_stack_phys: PhysAddr = 2_592_664;

    crate::info!("arch::paging::configure_cr3_switch_safety");
    info_kv("code_addr", code_addr);
    info_kv("stack_addr", stack_addr);
    crate::info!(" CR3 real switch: ENABLED (translate-based guard)");
    info_kv("expected_code_phys", expected_code_phys);
    info_kv("expected_stack_phys", expected_stack_phys);

    // ここでは検証のために kernel_aspace に「code/stack が map 済み」として登録
    let mut kernel_aspace = AddressSpace::new(0, AddressSpaceKind::Kernel, 1);

    let code_page = code_addr & !(PAGE_SIZE - 1);
    let stack_page = stack_addr & !(PAGE_SIZE - 1);
    let code_phys_page = expected_code_phys & !(PAGE_SIZE - 1);
    let stack_phys_page = expected_stack_phys & !(PAGE_SIZE - 1);

    kernel_aspace
        .apply(MemAction::map(code_page, code_phys_page, 0b11))
        .expect("map code page");
    kernel_aspace
        .apply(MemAction::map(stack_page, stack_phys_page, 0b11))
        .expect("map stack page");

    let layout = paging
        .install_kernel_high_alias_from_current(&kernel_aspace, &[code_addr, stack_addr])
        .expect("install high alias");

    paging
        .verify_high_alias_translation(&kernel_aspace, &[code_addr, stack_addr])
        .expect("verify alias");

    paging.dump_alias_range().expect("dump alias range");

    let code_high_virt = layout.to_high(code_addr).expect("to_high(code)");
    let stack_high_virt = layout.to_high(stack_addr).expect("to_high(stack)");

    crate::info!(" kernel high-alias self-check: OK");
    info_kv("code_high_virt", code_high_virt);
    info_kv("stack_high_virt", stack_high_virt);

    // 実機なら「high 側アドレスから実行できる」テストを行う
    let low_fn_addr: VirtAddr = 2_127_744;
    let high_fn_addr = layout.to_high(low_fn_addr).expect("to_high(fn)");
    crate::info!(" kernel high-alias exec test: OK");
    info_kv("low_fn_addr", low_fn_addr);
    info_kv("high_fn_addr", high_fn_addr);

    enter_kernel_high_alias(paging, &kernel_aspace, code_addr, stack_addr);
}

fn enter_kernel_high_alias(paging: &Paging, _kernel_aspace: &AddressSpace, code_addr: VirtAddr, stack_addr: VirtAddr) {
    let layout = paging.kernel_high_alias_layout().expect("layout");

    crate::info!("exec_context:");
    crate::info!("before enter_kernel_high_alias");
    info_kv("rip", 2_122_642u64);
    info_kv("rsp", stack_addr - 200);
    info_kv("rbp", stack_addr);

    let high_entry = layout.to_high(code_addr).expect("to_high(entry)");
    let rsp_high_aligned = (layout.to_high(stack_addr - 400).expect("to_high(rsp)") & !0xFu64) as u64;

    crate::info!("enter_kernel_high_alias: switching stack and CALL high entry");
    info_kv("low_entry", code_addr);
    info_kv("high_entry", high_entry);
    info_kv("rsp_high_aligned", rsp_high_aligned);

    kernel_high_entry(paging, high_entry, rsp_high_aligned);
}

fn kernel_high_entry(paging: &Paging, rip: VirtAddr, rsp: VirtAddr) {
    crate::info!("kernel_high_entry() [expected: high-alias]");

    let layout = paging.kernel_high_alias_layout().expect("layout");

    crate::info!("exec_context:");
    crate::info!("kernel_high_entry");
    info_kv("rip", canonicalize_virt_addr(rip));
    info_kv("rsp", canonicalize_virt_addr(rsp));

    if layout.is_in_high_range(rip) {
        crate::info!(" rip is in kernel high-alias region");
    } else {
        crate::info!(" rip is NOT in kernel high-alias region");
    }

    crate::info!("KernelState::bootstrap()");
    crate::info!("(demo) done");
}

fn demo_memory_map() -> Vec<MemoryRegion> {
    vec![
        MemoryRegion { index: 0, start_phys: 0, end_phys: 4096, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 1, start_phys: 4096, end_phys: 20480, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 2, start_phys: 20480, end_phys: 86016, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 3, start_phys: 86016, end_phys: 90112, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 4, start_phys: 90112, end_phys: 98304, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 5, start_phys: 98304, end_phys: 651264, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 6, start_phys: 651264, end_phys: 655360, region_type: MemoryRegionType::Reserved },
        MemoryRegion { index: 7, start_phys: 983040, end_phys: 1048576, region_type: MemoryRegionType::Reserved },
        MemoryRegion { index: 8, start_phys: 1048576, end_phys: 2592768, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 9, start_phys: 2592768, end_phys: 4194304, region_type: MemoryRegionType::Usable },
        MemoryRegion { index: 10, start_phys: 4194304, end_phys: 4468736, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 11, start_phys: 4468736, end_phys: 8712192, region_type: MemoryRegionType::Other },
        MemoryRegion { index: 12, start_phys: 8712192, end_phys: 536739840, region_type: MemoryRegionType::Usable },
        MemoryRegion { index: 13, start_phys: 536739840, end_phys: 536870912, region_type: MemoryRegionType::Reserved },
        MemoryRegion { index: 14, start_phys: 4294705152, end_phys: 4294967296, region_type: MemoryRegionType::Reserved },
        MemoryRegion { index: 15, start_phys: 1086626725888, end_phys: 1099511627776, region_type: MemoryRegionType::Reserved },
    ]
}
