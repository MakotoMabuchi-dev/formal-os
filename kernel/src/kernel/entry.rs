// kernel/src/kernel/entry.rs
//
// formal-os: kernel entry glue

use bootloader::BootInfo;

use crate::{arch, logging};

use super::KernelState;

#[inline(never)]
extern "C" fn kernel_high_entry(boot_info: &'static BootInfo) -> ! {
    logging::info("kernel_high_entry() [expected: high-alias]");
    arch::paging::debug_log_execution_context("kernel_high_entry");

    let mut kstate = KernelState::new(boot_info);
    kstate.bootstrap();

    // 通常運転の tick 数（デモ）
    // - ここは適宜増やしてOK（ログが増えるだけで意味は壊れない）
    let max_ticks = 120;

    for _ in 0..max_ticks {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
            break;
        }
        kstate.tick();
    }

    // ★追加: drain ticks
    //
    // 目的:
    // - 最終 tick が「IpcSend 直後」などの途中状態で終わりやすい。
    // - 特に今回のログでは、Task0 が IpcReply 待ちのまま終了していた。
    // - ここで数 tick 余分に回して、reply などの後続を進めてから dump する。
    //
    // 方針:
    // - should_halt を尊重
    // - 数は少なく固定（再現性重視）
    let drain_ticks = 4;
    for _ in 0..drain_ticks {
        if kstate.should_halt() {
            break;
        }
        kstate.tick();
    }

    kstate.dump_events();
    arch::halt_loop();
}

pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start() [low entry]");

    let code_addr = kernel_high_entry as usize as u64;

    let stack_probe: u64 = 0;
    let stack_addr = &stack_probe as *const u64 as u64;

    arch::paging::configure_cr3_switch_safety(code_addr, stack_addr);

    arch::paging::install_kernel_high_alias_from_current();

    // ★high-alias 導入後に IDT を “high base + high handlers” に切り替える
    arch::interrupts::reload_idt_high_alias();

    arch::paging::debug_log_execution_context("before enter_kernel_high_alias");

    arch::paging::enter_kernel_high_alias(kernel_high_entry, boot_info);
}
