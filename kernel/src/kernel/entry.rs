// kernel/src/kernel/entry.rs
//
// formal-os: kernel entry glue
//
// 役割:
// - low entry から high-alias entry へ遷移する。
// - (feature=ring3_demo) のとき、ring3 へ入って int 0x80 の往復を確認する。
// - 通常時は KernelState を生成して tick ループを回す。
//
// やること:
// - high-alias の準備（paging + IDT reload）後に high-alias entry に入る
// - ring3_demo: user root 作成 / user code+stack マップ / iretq で ring3 へ
//
// やらないこと:
// - 本格的なユーザローダ / ELF ロード（今は固定バイト列）
// - syscall/sysret の MSR 設定（まずは int 0x80）
//
// 設計方針:
// - ring3_demo は「観測性」を最優先し、ログは ring0 でのみ出す。
// - user CR3 中は logging を触らない（#PF を避ける）ため quiet switch を使う。

use bootloader::BootInfo;

use crate::{arch, logging};

use super::KernelState;

#[cfg(feature = "ring3_demo")]
use crate::mm::PhysicalMemoryManager;

#[cfg(feature = "ring3_demo")]
use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};

#[cfg(feature = "ring3_demo")]
use crate::mem::paging::{MemAction, PageFlags};

#[cfg(feature = "ring3_demo")]
use super::pagetable_init;

#[cfg(feature = "ring3_demo")]
#[inline(never)]
fn run_ring3_demo(boot_info: &'static BootInfo) -> ! {
    logging::info("ring3_demo: start");

    // 0) kernel root を保存（戻すため）
    let kernel_root: PhysFrame = {
        let (level_4_frame, _) = x86_64::registers::control::Cr3::read();
        let phys_u64 = level_4_frame.start_address().as_u64();
        PhysFrame::from_index(phys_u64 / PAGE_SIZE)
    };

    // 1) user root を作る
    let mut phys_mem = PhysicalMemoryManager::new(boot_info);

    let user_root: PhysFrame = match pagetable_init::allocate_new_l4_table(&mut phys_mem) {
        Some(f) => f,
        None => panic!("ring3_demo: no more frames for user pml4"),
    };

    arch::paging::init_user_pml4_from_current(user_root);

    // 2) user code/stack を 1ページずつ確保して user_root に map
    let code_frame_raw = phys_mem.allocate_frame().expect("ring3_demo: no frame for code");
    let stack_frame_raw = phys_mem.allocate_frame().expect("ring3_demo: no frame for stack");

    let code_phys = code_frame_raw.start_address().as_u64();
    let stack_phys = stack_frame_raw.start_address().as_u64();

    let code_frame = PhysFrame::from_index(code_phys / PAGE_SIZE);
    let stack_frame = PhysFrame::from_index(stack_phys / PAGE_SIZE);

    // USER 空間内の固定ページ（paging 側が USER_SPACE_BASE を足す）
    let user_code_page = VirtPage::from_index(0x120);
    let user_stack_page = VirtPage::from_index(0x121);

    let stack_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

    // code は init 中だけ RW、その後 RW を外す（RX相当）
    let code_flags_init = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
    let code_flags_final = PageFlags::PRESENT | PageFlags::USER;

    unsafe {
        arch::paging::apply_mem_action_in_root(
            MemAction::Map {
                page: user_code_page,
                frame: code_frame,
                flags: code_flags_init,
            },
            user_root,
            &mut phys_mem,
        )
            .expect("ring3_demo: map user code(init RW) failed");

        arch::paging::apply_mem_action_in_root(
            MemAction::Map {
                page: user_stack_page,
                frame: stack_frame,
                flags: stack_flags,
            },
            user_root,
            &mut phys_mem,
        )
            .expect("ring3_demo: map user stack failed");
    }

    // ------------------------------------------------------------
    // 3) ユーザコードを書き込む（user VA に直接書く）
    //
    // mailbox ABI:
    //   [rsp-16]=sysno(=1)
    //   [rsp-24]=a0(=0x1111)
    //   [rsp-32]=a1(=0x2222)
    //   [rsp-40]=a2(=0x3333)
    //   [rsp-48]=ret_slot（kernel が書く）
    //   [rsp-8] =echo（user が ret_slot を読んで書く）
    //
    // flow:
    //   set mailbox -> int80
    //   mov rax,[rsp-48] -> mov [rsp-8],rax -> int80
    //   int80 -> jmp $
    // ------------------------------------------------------------

    let user_code_va =
        (arch::paging::USER_SPACE_BASE + user_code_page.start_address().0) as *mut u8;

    // demo roots を登録（int80 handler が参照する）
    arch::paging::set_ring3_demo_roots(user_root, kernel_root);

    // user_root に切替（ログ無し）
    arch::paging::switch_address_space_quiet(user_root);

    unsafe {
        let bytes: &[u8] = &[
            // mov qword [rsp-16], 1
            0x48, 0xC7, 0x44, 0x24, 0xF0, 0x01, 0x00, 0x00, 0x00,
            // mov qword [rsp-24], 0x1111
            0x48, 0xC7, 0x44, 0x24, 0xE8, 0x11, 0x11, 0x00, 0x00,
            // mov qword [rsp-32], 0x2222
            0x48, 0xC7, 0x44, 0x24, 0xE0, 0x22, 0x22, 0x00, 0x00,
            // mov qword [rsp-40], 0x3333
            0x48, 0xC7, 0x44, 0x24, 0xD8, 0x33, 0x33, 0x00, 0x00,

            // int 0x80
            0xCD, 0x80,

            // mov rax, [rsp-48]
            0x48, 0x8B, 0x44, 0x24, 0xD0,
            // mov [rsp-8], rax
            0x48, 0x89, 0x44, 0x24, 0xF8,

            // int 0x80
            0xCD, 0x80,

            // int 0x80
            0xCD, 0x80,

            // jmp $
            0xEB, 0xFE,
        ];

        for (i, b) in bytes.iter().enumerate() {
            core::ptr::write_volatile(user_code_va.add(i), *b);
        }
    }

    // kernel_root に戻す（ログ無し）
    arch::paging::switch_address_space_quiet(kernel_root);


    // 3.5) code ページを RX 相当に戻す（RW を外す）
    unsafe {
        arch::paging::apply_mem_action_in_root(
            MemAction::Unmap { page: user_code_page },
            user_root,
            &mut phys_mem,
        )
            .expect("ring3_demo: unmap user code to drop WRITABLE failed");

        arch::paging::apply_mem_action_in_root(
            MemAction::Map {
                page: user_code_page,
                frame: code_frame,
                flags: code_flags_final,
            },
            user_root,
            &mut phys_mem,
        )
            .expect("ring3_demo: remap user code(final RX) failed");
    }

    // 4) ring3 へ入るための RIP/RSP/selector を決める
    let user_rip = arch::paging::USER_SPACE_BASE + user_code_page.start_address().0;
    let user_rsp =
        (arch::paging::USER_SPACE_BASE + user_stack_page.start_address().0 + PAGE_SIZE) & !0xFu64;

    let user_cs: u16 = arch::gdt::user_code_selector().0 | 3;
    let user_ss: u16 = arch::gdt::user_data_selector().0 | 3;

    // 5) CR3 を user_root に切替えて ring3 へ（iretq）
    logging::info("ring3_demo: entering ring3 via iretq");
    logging::info_u64("user_rip", user_rip);
    logging::info_u64("user_rsp", user_rsp);

    arch::paging::switch_address_space_quiet(user_root);

    unsafe { arch::ring3::enter_user_mode_iretq(user_rip, user_rsp, user_cs, user_ss) }
}

#[inline(never)]
extern "C" fn kernel_high_entry(boot_info: &'static BootInfo) -> ! {
    logging::info("kernel_high_entry() [expected: high-alias]");
    arch::paging::debug_log_execution_context("kernel_high_entry");

    #[cfg(feature = "ring3_demo")]
    {
        // 戻らないので、以降は到達しない（意図どおり）
        run_ring3_demo(boot_info);
    }

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

    // high-alias 導入後に IDT を “high base + high handlers” に切り替える
    arch::interrupts::reload_idt_high_alias();

    arch::paging::debug_log_execution_context("before enter_kernel_high_alias");

    arch::paging::enter_kernel_high_alias(kernel_high_entry, boot_info);
}
