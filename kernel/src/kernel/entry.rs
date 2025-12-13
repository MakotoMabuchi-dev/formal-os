// kernel/src/kernel/entry.rs
//
// formal-os: kernel entry glue
//
// 役割:
// - low entry で guard を設定し、high-alias をインストール
// - high-alias 側へスタックを切替えて本体ループへ移譲
//
// やること:
// - arch::paging の API を呼び出し、入口の段取りを組むだけ
//
// やらないこと:
// - スケジューラ/IPC/AddressSpace の中身（それは KernelState の責務）

use bootloader::BootInfo;

use crate::{arch, logging};

use super::KernelState;

/// high-alias 側で走る本体エントリ（ABI 固定）
#[inline(never)]
extern "C" fn kernel_high_entry(boot_info: &'static BootInfo) -> ! {
    logging::info("kernel_high_entry() [expected: high-alias]");
    arch::paging::debug_log_execution_context("kernel_high_entry");

    let mut kstate = KernelState::new(boot_info);
    kstate.bootstrap();

    let max_ticks = 120;
    for _ in 0..max_ticks {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
            break;
        }
        kstate.tick();
    }

    kstate.dump_events();
    arch::halt_loop();
}

/// low 側の入口（main から呼ばれる）
pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start() [low entry]");

    // guard は「これから high-alias 経由で走る本体コード」を基準にする
    let code_addr = kernel_high_entry as usize as u64;

    // 現在のスタック上の値のアドレスを取って guard 対象にする
    let stack_probe: u64 = 0;
    let stack_addr = &stack_probe as *const u64 as u64;

    arch::paging::configure_cr3_switch_safety(code_addr, stack_addr);

    // low 側 PML4 を high 側へ alias して self-test
    arch::paging::install_kernel_high_alias_from_current();

    // low 側で動いているはず
    arch::paging::debug_log_execution_context("before enter_kernel_high_alias");

    // high-alias 側へ（スタックも high-alias 側に切替えて CALL）
    arch::paging::enter_kernel_high_alias(kernel_high_entry, boot_info);
}
