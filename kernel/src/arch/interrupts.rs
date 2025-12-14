// kernel/src/arch/interrupts.rs
//
// 役割:
// - IDT(Interrupt Descriptor Table) を初期化・再ロードする。
// - high-alias 移行後も例外が確実に handler に届く状態を作る。
//
// やること:
// - init(): 低アドレス側で最低限の IDT を構築してロード
// - reload_idt_high_alias(): IDT base と handler を high-alias 側へ寄せて再ロード
// - #DF は IST を使って安定したスタックで処理する（リセット回避）
//   ※ #PF/#GP はまず RSP0 で受けて「ハンドラに入る」ことを最優先する
//
// やらないこと:
// - 完全な割り込み(IRQ)配線
// - 例外復帰/プロセス殺し等の本格処理（今はデバッグ優先）
//
// 設計方針:
// - 例外ハンドラは lock を取らない（死にやすい）
// - まずは “リセット→止まる” にして、原因を見える化する

#![allow(dead_code)]

use core::mem;

use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use x86_64::instructions::tables::{lidt, DescriptorTablePointer};
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{
    InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode,
};

use crate::{arch::{gdt, virt_layout}, logging};

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

        // まずは「確実に入る」ことを優先（IST は使わない）
        idt.page_fault.set_handler_fn(page_fault_handler);
        idt.general_protection_fault
            .set_handler_fn(general_protection_fault_handler);

        // #DF は IST を使いたいので handler だけセット（ISTは high-alias 側で設定）
        idt.double_fault.set_handler_fn(double_fault_handler);

        *IDT_LOW.lock() = Some(idt);

        let base_low = idt_low_addr();
        let ptr = DescriptorTablePointer {
            limit: (mem::size_of::<InterruptDescriptorTable>() - 1) as u16,
            base: VirtAddr::new(base_low),
        };

        unsafe { lidt(&ptr) };
        logging::info("arch::interrupts::init: IDT loaded");
    });
}

/// high-alias 側の handler / IDT base へ寄せて IDT を再ロードする
pub fn reload_idt_high_alias() {
    interrupts::without_interrupts(|| {
        // init が未実行なら先に low で最低限ロード
        if IDT_LOW.lock().is_none() {
            drop(IDT_LOW.lock());
            init();
        }

        // ★ 重要：ユーザ実行中例外のスタック切替に必要な TSS/GDT を先に用意
        // ここで RSP0 が未設定だと #PF に入る前に死にやすい
        gdt::init_high_alias();

        let mut idt = InterruptDescriptorTable::new();

        // handler を high-alias アドレスへ寄せて登録
        unsafe {
            // #PF: まずは IST を使わず RSP0 で安定化（トリプルフォルト回避の王道）
            idt.page_fault
                .set_handler_fn(transmute_pf(high_alias_addr(page_fault_handler as u64)));

            // #GP: 同様に RSP0 で受ける
            idt.general_protection_fault
                .set_handler_fn(transmute_gpf(high_alias_addr(general_protection_fault_handler as u64)));

            // #DF: ここだけ IST を使う（定石）
            idt.double_fault
                .set_handler_fn(transmute_df(high_alias_addr(double_fault_handler as u64)))
                .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        }

        *IDT_HIGH.lock() = Some(idt);

        let base_low = idt_high_addr_low();
        let base_high = high_alias_addr(base_low);

        // 既存ログ（あなたの確認用）
        logging::info_u64("idt_base_low", base_low);
        logging::info_u64("idt_base_high", base_high);
        logging::info_u64("idt_base_high_pml4", virt_layout::pml4_index(base_high) as u64);

        let pf_low = page_fault_handler as u64;
        let pf_high = high_alias_addr(pf_low);
        logging::info_u64("pf_handler_low", pf_low);
        logging::info_u64("pf_handler_high", pf_high);
        logging::info_u64("pf_handler_high_pml4", virt_layout::pml4_index(pf_high) as u64);

        let gp_low = general_protection_fault_handler as u64;
        let gp_high = high_alias_addr(gp_low);
        logging::info_u64("gp_handler_low", gp_low);
        logging::info_u64("gp_handler_high", gp_high);
        logging::info_u64("gp_handler_high_pml4", virt_layout::pml4_index(gp_high) as u64);

        let df_low = double_fault_handler as u64;
        let df_high = high_alias_addr(df_low);
        logging::info_u64("df_handler_low", df_low);
        logging::info_u64("df_handler_high", df_high);
        logging::info_u64("df_handler_high_pml4", virt_layout::pml4_index(df_high) as u64);

        let ptr = DescriptorTablePointer {
            limit: (mem::size_of::<InterruptDescriptorTable>() - 1) as u16,
            base: VirtAddr::new(base_high),
        };

        unsafe { lidt(&ptr) };
        logging::info("arch::interrupts::reload_idt_high_alias: IDT reloaded (base+handlers=high-alias)");
    });
}

fn idt_low_addr() -> u64 {
    let guard = IDT_LOW.lock();
    let idt_ref = guard.as_ref().expect("IDT_LOW not initialized");
    idt_ref as *const _ as u64
}

fn idt_high_addr_low() -> u64 {
    let guard = IDT_HIGH.lock();
    let idt_ref = guard.as_ref().expect("IDT_HIGH not initialized");
    idt_ref as *const _ as u64
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

// ─────────────────────────────────────────────
// 緊急出力（ロック無し）
// - QEMU debugcon(0xE9) と COM1(0x3F8) の両方へ投げる
//   どちらか拾える環境なら “例外に入った” の確定ができる
// ─────────────────────────────────────────────

fn emergency_write_byte(b: u8) {
    unsafe {
        // QEMU debugcon
        Port::<u8>::new(0xE9).write(b);

        // COM1（初期化済みなら出る）
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
        let c = match n {
            0..=9 => b'0' + n,
            _ => b'a' + (n - 10),
        };
        emergency_write_byte(c);
    }
}

// ─────────────────────────────────────────────
// 例外ハンドラ（まずは “止める”）
// ─────────────────────────────────────────────

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    interrupts::disable();

    // x86_64 0.15: Cr2::read() が Result なので安全に吸収
    let cr2 = Cr2::read()
        .unwrap_or(VirtAddr::new(0))
        .as_u64();

    emergency_write_str("[EXC] #PF cr2=");
    emergency_write_hex_u64(cr2);
    emergency_write_str(" err=");
    emergency_write_hex_u64(error_code.bits() as u64);
    emergency_write_str(" rip=");
    emergency_write_hex_u64(stack_frame.instruction_pointer.as_u64());
    emergency_write_str(" rsp=");
    emergency_write_hex_u64(stack_frame.stack_pointer.as_u64());
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
