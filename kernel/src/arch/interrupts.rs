// kernel/src/arch/interrupts.rs
//
// 役割:
// - IDT(Interrupt Descriptor Table) を初期化・再ロードする。
// - high-alias 移行後も例外が確実に handler に届く状態を作る。
//
// 設計方針:
// - 例外ハンドラは lock を取らない
// - guarded 区間の #PF は CR2 範囲に関係なく fixup を最優先
// - それ以外は fail-stop（観測性優先）
//
// 重要:
// - x86_64 crate(0.15.x) の IDT は handler シグネチャが固定。
//   extern "x86-interrupt" fn(InterruptStackFrame, ..) の形に揃える必要がある。
// - RIP fixup は InterruptStackFrame を raw pointer で InterruptStackFrameValue に見立てて更新する。

#![allow(dead_code)]

use core::mem;

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

use crate::{
    arch::{gdt, paging, virt_layout},
    logging,
};

type PageFaultHandler = extern "x86-interrupt" fn(InterruptStackFrame, PageFaultErrorCode);
type GpfHandler = extern "x86-interrupt" fn(InterruptStackFrame, u64);
type DoubleFaultHandler = extern "x86-interrupt" fn(InterruptStackFrame, u64) -> !;

static IDT_LOW: Mutex<Option<InterruptDescriptorTable>> = Mutex::new(None);
static IDT_HIGH: Mutex<Option<InterruptDescriptorTable>> = Mutex::new(None);

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

fn emergency_write_str(s: &str) {
    for b in s.bytes() {
        emergency_write_byte(b);
    }
}

fn emergency_write_hex_u64(v: u64) {
    emergency_write_str("0x");
    for i in (0..16).rev() {
        let n = ((v >> (i * 4)) & 0xF) as u8;
        let c = if n < 10 { b'0' + n } else { b'a' + (n - 10) };
        emergency_write_byte(c);
    }
}

// ---- RIP fixup ----
// InterruptStackFrame の内部表現は安定保証されないが、0.15.x では Value と同等の内容を保持する前提で
// raw pointer で見立てて instruction_pointer のみを書き換える（guarded fixup 用途に限定）

#[inline(always)]
fn set_exception_rip(stack_frame: &mut InterruptStackFrame, new_rip: u64) {
    unsafe {
        // 直キャストは E0606 で弾かれることがあるので、u8* を挟んで行う
        let p_isf: *mut InterruptStackFrame = stack_frame as *mut InterruptStackFrame;
        let p_u8: *mut u8 = p_isf as *mut u8;
        let p_val: *mut InterruptStackFrameValue = p_u8 as *mut InterruptStackFrameValue;

        (*p_val).instruction_pointer = VirtAddr::new(new_rip);
    }
}

// ---- handlers ----

extern "x86-interrupt" fn page_fault_handler(
    mut stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
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

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
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

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
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
