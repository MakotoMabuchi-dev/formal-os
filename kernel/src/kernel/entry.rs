// kernel/src/kernel/entry.rs
//
// formal-os: kernel entry glue
//
// 役割:
// - low entry から high-alias entry へ遷移する。
// - feature に応じて ring3 demo / ring3 mailbox demo / ring3 mailbox loop を起動する。
// - 通常時は KernelState を生成して tick ループを回す。
//
// 設計方針:
// - ring3 デモは「観測性」を最優先し、ログは ring0 でのみ出す。
// - user CR3 中は logging を触らない（#PF を避ける）ため quiet switch を使う。
// - ★重要: “ユーザコードの書込み” のために user CR3 に切り替えない。
//   physmap(physical_memory_offset) 経由で code_frame の物理メモリへ直接書く。
// - ring3_mailbox_loop は「カーネル内 tick と ring3 の int80」を混在させるため、
//   カーネル側の current_task/state 整合を事前に整える（prepare_ring3_loop_current_task）。

use bootloader::BootInfo;

use crate::{arch, logging};
use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};
use crate::mem::paging::{MemAction, PageFlags};

use super::KernelState;

// ring3_demo / ring3_mailbox だけが使う（未使用 warning を避ける）
#[cfg(any(feature = "ring3_demo", feature = "ring3_mailbox"))]
use crate::mm::PhysicalMemoryManager;

#[cfg(any(feature = "ring3_demo", feature = "ring3_mailbox"))]
use super::pagetable_init;

/// emergency 出力（panic 直前でも見える）
#[inline(always)]
fn eprint(s: &str) {
    crate::arch::interrupts::emergency_write_str(s);
}

/// emergency で u64 を出す
#[inline(always)]
fn eprint_hex(label: &str, v: u64) {
    crate::arch::interrupts::emergency_write_str(label);
    crate::arch::interrupts::emergency_write_hex_u64(v);
    crate::arch::interrupts::emergency_write_str("\n");
}

/// physmap 経由で「物理アドレス」にバイト列を書き込む
///
/// 前提:
/// - kernel CR3 では physmap が有効（physical_memory_offset が正しい）
#[inline(never)]
unsafe fn write_bytes_to_phys(phys_u64: u64, bytes: &[u8]) {
    eprint("[E] write_bytes_to_phys: begin\n");
    eprint_hex("[E] phys_u64=", phys_u64);
    eprint_hex("[E] physmap_off=", arch::paging::physical_memory_offset());

    // ここが false なら “physmap が current CR3 で壊れている”
    let ok = arch::paging::debug_physmap_can_access_phys(phys_u64);
    if !ok {
        eprint("[E] write_bytes_to_phys: physmap translate FAILED\n");
        panic!("write_bytes_to_phys: physmap translate failed");
    }

    let base = arch::paging::physical_memory_offset() + phys_u64;
    eprint_hex("[E] physmap_va=", base);

    let p = base as *mut u8;
    for (i, b) in bytes.iter().enumerate() {
        core::ptr::write_volatile(p.add(i), *b);
    }

    // 書けたかを 1 byte 読み返し
    let back0 = core::ptr::read_volatile(p);
    eprint_hex("[E] write_bytes_to_phys: wrote first byte=", back0 as u64);

    eprint("[E] write_bytes_to_phys: end\n");
}

#[cfg(feature = "ring3_demo")]
#[inline(never)]
fn run_ring3_demo(boot_info: &'static BootInfo) -> ! {
    logging::info("ring3_demo: start");

    let kernel_root: PhysFrame = {
        let (l4, _) = x86_64::registers::control::Cr3::read();
        let phys_u64 = l4.start_address().as_u64();
        PhysFrame::from_index(phys_u64 / PAGE_SIZE)
    };

    let mut phys_mem = PhysicalMemoryManager::new(boot_info);
    let user_root: PhysFrame =
        pagetable_init::allocate_new_l4_table(&mut phys_mem).expect("ring3_demo: no more frames for user pml4");

    arch::paging::init_user_pml4_from_current(user_root);

    let code_frame_raw = phys_mem.allocate_frame().expect("ring3_demo: no frame for code");
    let stack_frame_raw = phys_mem.allocate_frame().expect("ring3_demo: no frame for stack");

    let code_phys = code_frame_raw.start_address().as_u64();
    let stack_phys = stack_frame_raw.start_address().as_u64();

    let code_frame = PhysFrame::from_index(code_phys / PAGE_SIZE);
    let stack_frame = PhysFrame::from_index(stack_phys / PAGE_SIZE);

    let user_code_page = VirtPage::from_index(0x120);
    let user_stack_page = VirtPage::from_index(0x121);

    let stack_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
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

    arch::paging::set_ring3_demo_roots(user_root, kernel_root);

    unsafe {
        let bytes: &[u8] = &[
            0x48, 0xC7, 0x44, 0x24, 0xF0, 0x01, 0x00, 0x00, 0x00, // sysno=1
            0x48, 0xC7, 0x44, 0x24, 0xE8, 0x11, 0x11, 0x00, 0x00, // a0
            0x48, 0xC7, 0x44, 0x24, 0xE0, 0x22, 0x22, 0x00, 0x00, // a1
            0x48, 0xC7, 0x44, 0x24, 0xD8, 0x33, 0x33, 0x00, 0x00, // a2
            0xCD, 0x80,
            0x48, 0x8B, 0x44, 0x24, 0xD0, // mov rax, [rsp-48]
            0x48, 0x89, 0x44, 0x24, 0xF8, // mov [rsp-8], rax
            0xCD, 0x80,
            0xCD, 0x80,
            0xEB, 0xFE,
        ];
        write_bytes_to_phys(code_phys, bytes);
    }

    unsafe {
        arch::paging::apply_mem_action_in_root(MemAction::Unmap { page: user_code_page }, user_root, &mut phys_mem)
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

    let user_rip = arch::paging::USER_SPACE_BASE + user_code_page.start_address().0;
    let user_rsp = (arch::paging::USER_SPACE_BASE + user_stack_page.start_address().0 + PAGE_SIZE) & !0xFu64;

    let user_cs: u16 = arch::gdt::user_code_selector().0 | 3;
    let user_ss: u16 = arch::gdt::user_data_selector().0 | 3;

    logging::info("ring3_demo: entering ring3 via iretq");
    logging::info_u64("user_rip", user_rip);
    logging::info_u64("user_rsp", user_rsp);

    logging::set_vga_enabled(false);
    arch::paging::switch_address_space_quiet(user_root);

    unsafe { arch::ring3::enter_user_mode_iretq(user_rip, user_rsp, user_cs, user_ss) }
}

#[cfg(feature = "ring3_mailbox")]
#[inline(never)]
fn run_ring3_mailbox_demo(boot_info: &'static BootInfo) -> ! {
    logging::info("ring3_mailbox: start");

    let kernel_root: PhysFrame = {
        let (l4, _) = x86_64::registers::control::Cr3::read();
        PhysFrame::from_index(l4.start_address().as_u64() / PAGE_SIZE)
    };

    let mut phys_mem = PhysicalMemoryManager::new(boot_info);
    let user_root: PhysFrame =
        pagetable_init::allocate_new_l4_table(&mut phys_mem).expect("ring3_mailbox: no more frames for user pml4");

    arch::paging::init_user_pml4_from_current(user_root);

    let code_frame_raw = phys_mem.allocate_frame().expect("ring3_mailbox: no frame for code");
    let stack_frame_raw = phys_mem.allocate_frame().expect("ring3_mailbox: no frame for stack");

    let code_phys = code_frame_raw.start_address().as_u64();
    let stack_phys = stack_frame_raw.start_address().as_u64();

    let code_frame = PhysFrame::from_index(code_phys / PAGE_SIZE);
    let stack_frame = PhysFrame::from_index(stack_phys / PAGE_SIZE);

    let user_code_page = VirtPage::from_index(0x120);
    let user_stack_page = VirtPage::from_index(0x121);

    let stack_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
    let code_flags_init = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

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
            .expect("ring3_mailbox: map user code(init RW) failed");

        arch::paging::apply_mem_action_in_root(
            MemAction::Map {
                page: user_stack_page,
                frame: stack_frame,
                flags: stack_flags,
            },
            user_root,
            &mut phys_mem,
        )
            .expect("ring3_mailbox: map user stack failed");
    }

    arch::paging::set_ring3_demo_roots(user_root, kernel_root);

    unsafe {
        let bytes: &[u8] = &[
            0x48, 0xC7, 0x44, 0x24, 0xF0, 0x0B, 0x00, 0x00, 0x00, // sysno=11
            0x48, 0xC7, 0x44, 0x24, 0xE8, 0x00, 0x00, 0x00, 0x00, // ep=0
            0x48, 0xC7, 0x44, 0x24, 0xE0, 0x34, 0x12, 0x00, 0x00, // msg=0x1234
            0x48, 0xC7, 0x44, 0x24, 0xD8, 0x00, 0x00, 0x00, 0x00, // a2=0
            0xCD, 0x80,
            0x48, 0x8B, 0x44, 0x24, 0xD0,
            0x48, 0x89, 0x44, 0x24, 0xF8,
            0xCD, 0x80,
            0xCD, 0x80,
            0xEB, 0xFE,
        ];
        write_bytes_to_phys(code_phys, bytes);
    }

    let user_rip = arch::paging::USER_SPACE_BASE + user_code_page.start_address().0;
    let user_rsp = (arch::paging::USER_SPACE_BASE + user_stack_page.start_address().0 + PAGE_SIZE) & !0xFu64;

    let user_cs: u16 = arch::gdt::user_code_selector().0 | 3;
    let user_ss: u16 = arch::gdt::user_data_selector().0 | 3;

    logging::info("ring3_mailbox: entering ring3 via iretq");
    logging::info_u64("user_rip", user_rip);
    logging::info_u64("user_rsp", user_rsp);

    logging::set_vga_enabled(false);
    arch::paging::switch_address_space_quiet(user_root);

    unsafe { arch::ring3::enter_user_mode_iretq(user_rip, user_rsp, user_cs, user_ss) }
}

#[cfg(feature = "ring3_mailbox_loop")]
#[inline(never)]
fn run_ring3_mailbox_loop_demo(_boot_info: &'static BootInfo, kstate: &mut KernelState) -> ! {
    logging::info("ring3_mailbox_loop: start");

    // ★重要: ring3 ループは「Task1(User) が走っている」前提で整合を取る
    // - current_task/state/ready_queue を最小限で整える
    kstate.prepare_ring3_loop_current_task();

    let kernel_root: PhysFrame = {
        let (l4, _) = x86_64::registers::control::Cr3::read();
        PhysFrame::from_index(l4.start_address().as_u64() / PAGE_SIZE)
    };

    let user_root: PhysFrame = kstate.address_spaces[1]
        .root_page_frame
        .expect("ring3_mailbox_loop: user root must exist");

    let code_frame_raw = kstate.phys_mem.allocate_frame().expect("ring3_mailbox_loop: no frame for code");
    let stack_frame_raw = kstate.phys_mem.allocate_frame().expect("ring3_mailbox_loop: no frame for stack");

    let code_phys = code_frame_raw.start_address().as_u64();
    let stack_phys = stack_frame_raw.start_address().as_u64();

    let code_frame = PhysFrame::from_index(code_phys / PAGE_SIZE);
    let stack_frame = PhysFrame::from_index(stack_phys / PAGE_SIZE);

    let user_code_page = VirtPage::from_index(0x120);
    let user_stack_page = VirtPage::from_index(0x121);

    let stack_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;
    let code_flags_init = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER;

    unsafe {
        arch::paging::apply_mem_action_in_root(
            MemAction::Map {
                page: user_code_page,
                frame: code_frame,
                flags: code_flags_init,
            },
            user_root,
            &mut kstate.phys_mem,
        )
            .expect("ring3_mailbox_loop: map user code(init RW) failed");

        arch::paging::apply_mem_action_in_root(
            MemAction::Map {
                page: user_stack_page,
                frame: stack_frame,
                flags: stack_flags,
            },
            user_root,
            &mut kstate.phys_mem,
        )
            .expect("ring3_mailbox_loop: map user stack failed");
    }

    logging::info("ring3_mailbox_loop: mapped user code+stack OK");
    logging::info_u64("ring3_mailbox_loop: user_root_phys", user_root.start_address().0);
    logging::info_u64("ring3_mailbox_loop: kernel_root_phys", kernel_root.start_address().0);

    arch::paging::set_ring3_demo_roots(user_root, kernel_root);

    // panic 位置特定用
    eprint("[E] after roots registered\n");

    unsafe {
        // ★繰り返しを入れるので 1024 だと溢れやすい。余裕を持って 4096 にする。
        let mut bytes_vec: [u8; 4096] = [0; 4096];
        let mut n: usize = 0;

        #[inline(always)]
        fn ensure_cap(buf_len: usize, cur: usize, add: usize, tag: &'static str) {
            if cur + add > buf_len {
                crate::arch::interrupts::emergency_write_str("[E] bytes_vec overflow at ");
                crate::arch::interrupts::emergency_write_str(tag);
                crate::arch::interrupts::emergency_write_str(" cur=");
                crate::arch::interrupts::emergency_write_hex_u64(cur as u64);
                crate::arch::interrupts::emergency_write_str(" add=");
                crate::arch::interrupts::emergency_write_hex_u64(add as u64);
                crate::arch::interrupts::emergency_write_str("\n");
                panic!("ring3_mailbox_loop: bytes_vec overflow");
            }
        }

        fn mov_rsp_off_imm32(buf: &mut [u8; 4096], idx: &mut usize, off: i8, imm: u32) {
            ensure_cap(buf.len(), *idx, 9, "mov_rsp_off_imm32");
            buf[*idx + 0] = 0x48;
            buf[*idx + 1] = 0xC7;
            buf[*idx + 2] = 0x44;
            buf[*idx + 3] = 0x24;
            buf[*idx + 4] = off as u8;
            buf[*idx + 5] = (imm & 0xFF) as u8;
            buf[*idx + 6] = ((imm >> 8) & 0xFF) as u8;
            buf[*idx + 7] = ((imm >> 16) & 0xFF) as u8;
            buf[*idx + 8] = ((imm >> 24) & 0xFF) as u8;
            *idx += 9;
        }

        macro_rules! push {
            ($tag:expr; $($b:expr),+ $(,)?) => {{
                let add = 0 $(+ { let _ = $b; 1 })+;
                ensure_cap(bytes_vec.len(), n, add, $tag);
                $(bytes_vec[n] = $b; n += 1;)+
            }};
        }

        // ------------------------------------------------------------
        // 目標:
        // - send → tick → take_reply を複数回回して “往復” を確認する
        // - 取得した ret_slot は echo([rsp-8]) に都度上書き（観測は最後でOK）
        // ------------------------------------------------------------
        for round in 0..4u32 {
            // sys11: ipc_send(ep=0, msg=0x1234 + round)
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -16, 11);
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -24, 0);
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -32, 0x1234 + round);
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -40, 0);
            push!("int80_send"; 0xCD, 0x80);

            // tick を少し回して receiver を動かす（重いなら 8→4 に落としてOK）
            for _ in 0..8 {
                mov_rsp_off_imm32(&mut bytes_vec, &mut n, -16, 30);
                mov_rsp_off_imm32(&mut bytes_vec, &mut n, -24, 0);
                mov_rsp_off_imm32(&mut bytes_vec, &mut n, -32, 0);
                mov_rsp_off_imm32(&mut bytes_vec, &mut n, -40, 0);
                push!("int80_tick"; 0xCD, 0x80);
            }

            // sys31: take_last_reply
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -16, 31);
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -24, 0);
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -32, 0);
            mov_rsp_off_imm32(&mut bytes_vec, &mut n, -40, 0);
            push!("int80_take_reply"; 0xCD, 0x80);

            // ret_slot([rsp-48]) -> echo([rsp-8])（観測用）
            push!("copy_ret_to_echo";
                0x48, 0x8B, 0x44, 0x24, 0xD0, // mov rax, [rsp-48]
                0x48, 0x89, 0x44, 0x24, 0xF8, // mov [rsp-8], rax
            );
        }

        // jmp $
        push!("jmp"; 0xEB, 0xFE);

        eprint("[E] about to write bytes to phys\n");
        eprint_hex("[E] code_phys=", code_phys);

        write_bytes_to_phys(code_phys, &bytes_vec[..n]);
        eprint("[E] bytes written OK\n");
    }

    // ------------------------------------------------------------
    // code を「書き込み後に RX」に落とす
    // - ring3_mailbox_loop_skip_rx が有効なら skip（debug）
    // ------------------------------------------------------------
    #[cfg(not(feature = "ring3_mailbox_loop_skip_rx"))]
    {
        logging::info("ring3_mailbox_loop: remap code to RX (drop WRITABLE)");

        let code_flags_rx = PageFlags::PRESENT | PageFlags::USER;

        unsafe {
            arch::paging::apply_mem_action_in_root(
                MemAction::Unmap { page: user_code_page },
                user_root,
                &mut kstate.phys_mem,
            )
                .expect("ring3_mailbox_loop: unmap user code to drop WRITABLE failed");

            arch::paging::apply_mem_action_in_root(
                MemAction::Map {
                    page: user_code_page,
                    frame: code_frame,
                    flags: code_flags_rx,
                },
                user_root,
                &mut kstate.phys_mem,
            )
                .expect("ring3_mailbox_loop: remap user code(final RX) failed");
        }
    }

    #[cfg(feature = "ring3_mailbox_loop_skip_rx")]
    {
        logging::info("ring3_mailbox_loop: skip RX remap (debug)");
    }

    let user_rip = arch::paging::USER_SPACE_BASE + user_code_page.start_address().0;
    let user_rsp = (arch::paging::USER_SPACE_BASE + user_stack_page.start_address().0 + PAGE_SIZE) & !0xFu64;

    let user_cs: u16 = arch::gdt::user_code_selector().0 | 3;
    let user_ss: u16 = arch::gdt::user_data_selector().0 | 3;

    logging::info("ring3_mailbox_loop: entering ring3 via iretq");
    logging::info_u64("user_rip", user_rip);
    logging::info_u64("user_rsp", user_rsp);

    logging::set_vga_enabled(false);
    arch::paging::switch_address_space_quiet(user_root);

    unsafe { arch::ring3::enter_user_mode_iretq(user_rip, user_rsp, user_cs, user_ss) }
}

#[inline(never)]
extern "C" fn kernel_high_entry(boot_info: &'static BootInfo) -> ! {
    logging::info("kernel_high_entry() [expected: high-alias]");
    arch::paging::debug_log_execution_context("kernel_high_entry");

    #[cfg(feature = "ring3_demo")]
    {
        run_ring3_demo(boot_info);
    }

    #[cfg(all(not(feature = "ring3_demo"), feature = "ring3_mailbox"))]
    {
        let mut kstate = KernelState::new(boot_info);
        super::state_ref::register_kernel_state(&mut kstate);

        kstate.bootstrap();
        for _ in 0..3 {
            if kstate.should_halt() {
                break;
            }
            kstate.tick();
        }

        run_ring3_mailbox_demo(boot_info);
    }

    #[cfg(all(
        not(feature = "ring3_demo"),
        not(feature = "ring3_mailbox"),
        feature = "ring3_mailbox_loop"
    ))]
    {
        logging::info("ring3_mailbox_loop: preparing KernelState and entering ring3");

        let mut kstate = KernelState::new(boot_info);
        super::state_ref::register_kernel_state(&mut kstate);

        kstate.bootstrap();
        run_ring3_mailbox_loop_demo(boot_info, &mut kstate);
    }

    // ------------------------------------------------------------
    // 通常起動（デモ feature が無いとき）
    // ------------------------------------------------------------
    let mut kstate = KernelState::new(boot_info);
    super::state_ref::register_kernel_state(&mut kstate);

    kstate.bootstrap();
    for _ in 0..120 {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
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
    arch::interrupts::reload_idt_high_alias();

    arch::paging::debug_log_execution_context("before enter_kernel_high_alias");
    arch::paging::enter_kernel_high_alias(kernel_high_entry, boot_info);
}
