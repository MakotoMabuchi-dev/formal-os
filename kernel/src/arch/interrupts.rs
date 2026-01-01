// kernel/src/arch/interrupts.rs
//
// 役割:
// - IDT(Interrupt Descriptor Table) を初期化・再ロードする。
// - high-alias 移行後も例外が確実に handler に届く状態を作る。
// - ring3 MVP: int 0x80 を追加して user -> kernel の入口にする。
//
// 設計方針:
// - 例外ハンドラは lock を取らない
// - guarded 区間の #PF は CR2 範囲に関係なく fixup を最優先
// - それ以外は fail-stop（観測性優先）
//
// 重要:
// - x86_64 crate(0.15.x) の IDT は handler シグネチャが固定。
// - RIP fixup は InterruptStackFrame を raw pointer で InterruptStackFrameValue に見立てて更新する。
//
// ★ring3 MVP（安定版）:
// - “RAX を ring3 に返す” は x86-interrupt だと保証しづらい（レジスタを触れない）のでやらない。
// - 代わりに kernel が user stack (ret_slot) に直接書く。
// - iretq 前は必ず user_root に戻す（CR3 が kernel のままだと命令フェッチで #PF する）。
//
// 実装メモ:
// - ring3_* デモは paging 側に (user_root, kernel_root) を登録し、ここから参照する。

#![allow(dead_code)]

use core::mem;
use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::instructions::tables::{lidt, DescriptorTablePointer};
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{
    InterruptDescriptorTable,
    InterruptStackFrame,
    InterruptStackFrameValue,
    PageFaultErrorCode,
};
use x86_64::PrivilegeLevel;

use crate::{
    arch::{gdt, paging, virt_layout},
    logging,
};

type PageFaultHandler = extern "x86-interrupt" fn(InterruptStackFrame, PageFaultErrorCode);
type GpfHandler = extern "x86-interrupt" fn(InterruptStackFrame, u64);
type DoubleFaultHandler = extern "x86-interrupt" fn(InterruptStackFrame, u64) -> !;
type Int80Handler = extern "x86-interrupt" fn(InterruptStackFrame);

static IDT_LOW: Mutex<Option<InterruptDescriptorTable>> = Mutex::new(None);
static IDT_HIGH: Mutex<Option<InterruptDescriptorTable>> = Mutex::new(None);

static INT80_COUNT: AtomicU64 = AtomicU64::new(0);

// ---- ring3 demo roots cache ----
static DEMO_USER_ROOT_PHYS: AtomicU64 = AtomicU64::new(0);
static DEMO_KERNEL_ROOT_PHYS: AtomicU64 = AtomicU64::new(0);

// ---- debug: sys31 output counter ----
// 目的: ring3_mailbox_loop で “返信が毎回返る” を観測する。
// ログが多いなら上限を設ける（例: 最初の 64 回まで出す）。
static DBG_SYS31_COUNT: AtomicU64 = AtomicU64::new(0);
const DBG_SYS31_LIMIT: u64 = 64;

fn cache_demo_roots_if_needed() -> Option<(crate::mem::addr::PhysFrame, crate::mem::addr::PhysFrame)> {
    use crate::mem::addr::{PhysFrame, PAGE_SIZE};

    let u = DEMO_USER_ROOT_PHYS.load(Ordering::Relaxed);
    let k = DEMO_KERNEL_ROOT_PHYS.load(Ordering::Relaxed);
    if u != 0 && k != 0 {
        return Some((PhysFrame::from_index(u / PAGE_SIZE), PhysFrame::from_index(k / PAGE_SIZE)));
    }

    let (user_root, kernel_root) = paging::peek_ring3_demo_roots()?;

    DEMO_USER_ROOT_PHYS.store(user_root.start_address().0, Ordering::Relaxed);
    DEMO_KERNEL_ROOT_PHYS.store(kernel_root.start_address().0, Ordering::Relaxed);

    Some((user_root, kernel_root))
}

pub fn init() {
    interrupts::without_interrupts(|| {
        if IDT_LOW.lock().is_some() {
            return;
        }

        let mut idt = InterruptDescriptorTable::new();
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault_handler);
        idt.double_fault.set_handler_fn(double_fault_handler);

        // ring3: int 0x80
        unsafe {
            idt[0x80]
                .set_handler_fn(int80_handler)
                .set_privilege_level(PrivilegeLevel::Ring3)
                .set_stack_index(gdt::PAGE_FAULT_IST_INDEX);
        }

        *IDT_LOW.lock() = Some(idt);

        let ptr = DescriptorTablePointer {
            limit: (mem::size_of::<InterruptDescriptorTable>() - 1) as u16,
            base: VirtAddr::new(idt_low_addr()),
        };
        unsafe { lidt(&ptr) };
        logging::info("arch::interrupts::init: IDT loaded");
    });
}

pub fn reload_idt_high_alias() {
    interrupts::without_interrupts(|| {
        if IDT_LOW.lock().is_none() {
            drop(IDT_LOW.lock());
            init();
        }

        gdt::init_high_alias();

        let mut idt = InterruptDescriptorTable::new();
        unsafe {
            idt.page_fault
                .set_handler_fn(transmute_pf(high_alias_addr(page_fault_handler as u64)));
            idt.general_protection_fault
                .set_handler_fn(transmute_gpf(high_alias_addr(general_protection_fault_handler as u64)));
            idt.double_fault
                .set_handler_fn(transmute_df(high_alias_addr(double_fault_handler as u64)))
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);

            idt[0x80]
                .set_handler_fn(transmute_int80(high_alias_addr(int80_handler as u64)))
                .set_privilege_level(PrivilegeLevel::Ring3)
                .set_stack_index(gdt::PAGE_FAULT_IST_INDEX);
        }

        *IDT_HIGH.lock() = Some(idt);

        let base_high = high_alias_addr(idt_high_addr_low());
        let ptr = DescriptorTablePointer {
            limit: (mem::size_of::<InterruptDescriptorTable>() - 1) as u16,
            base: VirtAddr::new(base_high),
        };

        unsafe { lidt(&ptr) };
        logging::info("arch::interrupts::reload_idt_high_alias: IDT reloaded (high-alias)");
    });
}

fn idt_low_addr() -> u64 {
    let guard = IDT_LOW.lock();
    guard.as_ref().expect("IDT_LOW not initialized") as *const _ as u64
}

fn idt_high_addr_low() -> u64 {
    let guard = IDT_HIGH.lock();
    guard.as_ref().expect("IDT_HIGH not initialized") as *const _ as u64
}

#[inline(always)]
fn high_alias_addr(low: u64) -> u64 {
    virt_layout::kernel_high_alias_of_low(low)
}

unsafe fn transmute_pf(addr: u64) -> PageFaultHandler {
    mem::transmute::<u64, PageFaultHandler>(addr)
}
unsafe fn transmute_gpf(addr: u64) -> GpfHandler {
    mem::transmute::<u64, GpfHandler>(addr)
}
unsafe fn transmute_df(addr: u64) -> DoubleFaultHandler {
    mem::transmute::<u64, DoubleFaultHandler>(addr)
}
unsafe fn transmute_int80(addr: u64) -> Int80Handler {
    mem::transmute::<u64, Int80Handler>(addr)
}

// ---- emergency output ----

fn emergency_write_byte(b: u8) {
    unsafe {
        Port::<u8>::new(0xE9).write(b);
        let mut lsr = Port::<u8>::new(0x3FD);
        let mut data = Port::<u8>::new(0x3F8);
        for _ in 0..10_000 {
            if (lsr.read() & 0x20) != 0 {
                break;
            }
        }
        data.write(b);
    }
}

pub(crate) fn emergency_write_str(s: &str) {
    for b in s.bytes() {
        emergency_write_byte(b);
    }
}

pub(crate) fn emergency_write_hex_u64(v: u64) {
    emergency_write_str("0x");
    for i in (0..16).rev() {
        let n = ((v >> (i * 4)) & 0xF) as u8;
        let c = if n < 10 { b'0' + n } else { b'a' + (n - 10) };
        emergency_write_byte(c);
    }
}

// ---- RIP fixup ----

#[inline(always)]
fn set_exception_rip(stack_frame: &mut InterruptStackFrame, new_rip: u64) {
    unsafe {
        let p_isf: *mut InterruptStackFrame = stack_frame as *mut InterruptStackFrame;
        let p_u8: *mut u8 = p_isf as *mut u8;
        let p_val: *mut InterruptStackFrameValue = p_u8 as *mut InterruptStackFrameValue;
        (*p_val).instruction_pointer = VirtAddr::new(new_rip);
    }
}

// ---- int80 handler ----

extern "x86-interrupt" fn int80_handler(stack_frame: InterruptStackFrame) {
    #[cfg(feature = "ring3_demo")]
    {
        int80_handler_ring3_demo(stack_frame);
        return;
    }

    int80_handler_mailbox(stack_frame);
}

#[cfg(feature = "ring3_demo")]
fn int80_handler_ring3_demo(stack_frame: InterruptStackFrame) {
    let n = INT80_COUNT.fetch_add(1, Ordering::SeqCst) + 1;

    let user_rip = stack_frame.instruction_pointer.as_u64();
    let user_rsp = stack_frame.stack_pointer.as_u64();

    let p_sysno = (user_rsp.wrapping_sub(16)) as *const u64;
    let p_a0 = (user_rsp.wrapping_sub(24)) as *const u64;
    let p_a1 = (user_rsp.wrapping_sub(32)) as *const u64;
    let p_a2 = (user_rsp.wrapping_sub(40)) as *const u64;
    let p_retslot = (user_rsp.wrapping_sub(48)) as *mut u64;

    let p_user_echo = (user_rsp.wrapping_sub(8)) as *const u64;

    let (user_root, kernel_root) = match paging::peek_ring3_demo_roots() {
        Some(v) => v,
        None => {
            emergency_write_str("[INT80] demo_roots: NONE\n");
            crate::arch::halt_loop();
        }
    };

    if n == 1 {
        let sysno = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_sysno).unwrap_or(0);
        let a0 = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_a0).unwrap_or(0);
        let a1 = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_a1).unwrap_or(0);
        let a2 = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_a2).unwrap_or(0);

        emergency_write_str("[INT80] syscall enter\n");
        emergency_write_str(" rip="); emergency_write_hex_u64(user_rip);
        emergency_write_str(" rsp="); emergency_write_hex_u64(user_rsp);
        emergency_write_str("\n");
        emergency_write_str(" sysno="); emergency_write_hex_u64(sysno);
        emergency_write_str(" a0="); emergency_write_hex_u64(a0);
        emergency_write_str(" a1="); emergency_write_hex_u64(a1);
        emergency_write_str(" a2="); emergency_write_hex_u64(a2);
        emergency_write_str("\n");

        let ret = if sysno == 1 { a0.wrapping_add(a1).wrapping_add(a2) } else { 0 };
        let _ = paging::guarded_user_rw_u64_in_root(user_root, kernel_root, p_retslot, ret);

        paging::switch_address_space_quiet(user_root);
        return;
    }

    if n == 2 {
        emergency_write_str("[INT80] verify user_echo\n");
        emergency_write_str(" rip="); emergency_write_hex_u64(user_rip);
        emergency_write_str(" rsp="); emergency_write_hex_u64(user_rsp);
        emergency_write_str("\n");

        let echo = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_user_echo).unwrap_or(0);
        emergency_write_str(" echo="); emergency_write_hex_u64(echo);
        emergency_write_str("\n");

        paging::switch_address_space_quiet(user_root);
        return;
    }

    emergency_write_str("[INT80] final\n");
    let echo = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_user_echo).unwrap_or(0);
    emergency_write_str(" echo="); emergency_write_hex_u64(echo);
    emergency_write_str("\n");
    emergency_write_str("[INT80] done -> halt\n");
    crate::arch::halt_loop();
}

fn int80_handler_mailbox(stack_frame: InterruptStackFrame) {
    let user_rsp = stack_frame.stack_pointer.as_u64();

    // mailbox ABI offsets
    const OFF_SYSNO: u64 = 16;
    const OFF_A0: u64 = 24;
    const OFF_A1: u64 = 32;
    const OFF_A2: u64 = 40;
    const OFF_RET: u64 = 48;

    let p_sysno = (user_rsp.wrapping_sub(OFF_SYSNO)) as *const u64;
    let p_a0 = (user_rsp.wrapping_sub(OFF_A0)) as *const u64;
    let p_a1 = (user_rsp.wrapping_sub(OFF_A1)) as *const u64;
    let p_a2 = (user_rsp.wrapping_sub(OFF_A2)) as *const u64;
    let p_retslot = (user_rsp.wrapping_sub(OFF_RET)) as *mut u64;

    let (user_root, kernel_root) = match cache_demo_roots_if_needed() {
        Some(v) => v,
        None => {
            emergency_write_str("[INT80] roots: NONE\n");
            crate::arch::halt_loop();
        }
    };

    let sysno = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_sysno).unwrap_or(0);
    let a0 = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_a0).unwrap_or(0);
    let a1 = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_a1).unwrap_or(0);
    let a2 = paging::guarded_user_read_u64_in_root(user_root, kernel_root, p_a2).unwrap_or(0);

    let ret = crate::kernel::with_kernel_state(|ks| crate::kernel::mailbox_dispatch(ks, sysno, a0, a1, a2))
        .unwrap_or(0);

    // sysno=31 の戻り値を N 回まで emergency に出す（観測用）
    if sysno == 31 {
        let c = DBG_SYS31_COUNT.fetch_add(1, Ordering::Relaxed);
        if c < DBG_SYS31_LIMIT {
            emergency_write_str("[INT80] sys31 ret=");
            emergency_write_hex_u64(ret);
            emergency_write_str("\n");
        }
    }

    let _ = paging::guarded_user_rw_u64_in_root(user_root, kernel_root, p_retslot, ret);

    // iretq 前に user_root に戻す
    paging::switch_address_space_quiet(user_root);
}

// ---- exception handlers ----

extern "x86-interrupt" fn page_fault_handler(mut stack_frame: InterruptStackFrame, error_code: PageFaultErrorCode) {
    interrupts::disable();

    let cr2 = Cr2::read().unwrap_or(VirtAddr::new(0)).as_u64();
    let rip = stack_frame.instruction_pointer.as_u64();
    let rsp = stack_frame.stack_pointer.as_u64();

    paging::record_page_fault(paging::PageFaultInfo {
        addr: cr2,
        err: error_code.bits() as u64,
        rip,
        rsp,
        is_user_fault: false,
    });

    if let Some(recover_rip) = paging::pf_guard_try_fixup() {
        emergency_write_str("[EXC] #PF guarded => fixup\n");
        set_exception_rip(&mut stack_frame, recover_rip);
        return;
    }

    emergency_write_str("[EXC] #PF unguarded\n");
    emergency_write_str(" cr2="); emergency_write_hex_u64(cr2);
    emergency_write_str(" err="); emergency_write_hex_u64(error_code.bits() as u64);
    emergency_write_str(" rip="); emergency_write_hex_u64(rip);
    emergency_write_str(" rsp="); emergency_write_hex_u64(rsp);
    emergency_write_str("\n");

    crate::arch::halt_loop();
}

extern "x86-interrupt" fn general_protection_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) {
    interrupts::disable();

    emergency_write_str("[EXC] #GP err=");
    emergency_write_hex_u64(error_code);
    emergency_write_str(" rip=");
    emergency_write_hex_u64(stack_frame.instruction_pointer.as_u64());
    emergency_write_str(" rsp=");
    emergency_write_hex_u64(stack_frame.stack_pointer.as_u64());
    emergency_write_str("\n");

    crate::arch::halt_loop();
}

extern "x86-interrupt" fn double_fault_handler(stack_frame: InterruptStackFrame, error_code: u64) -> ! {
    interrupts::disable();

    emergency_write_str("[EXC] #DF err=");
    emergency_write_hex_u64(error_code);
    emergency_write_str(" rip=");
    emergency_write_hex_u64(stack_frame.instruction_pointer.as_u64());
    emergency_write_str(" rsp=");
    emergency_write_hex_u64(stack_frame.stack_pointer.as_u64());
    emergency_write_str("\n");

    crate::arch::halt_loop();
}
