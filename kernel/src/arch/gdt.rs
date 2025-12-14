// kernel/src/arch/gdt.rs
//
// 役割:
// - GDT と TSS を初期化してロードする
// - IST / RSP0 を用意し、例外時に安定したスタックへ切り替える
//
// やること:
// - init_high_alias(): high-alias で参照できる GDT/TSS を作成し GDTR/TR を更新
// - #PF / #DF を IST で受けられるように TSS.ist を設定
//
// やらないこと:
// - ring3 本格移行のためのユーザセグメント設計（今は例外の安定化が優先）
// - per-cpu 構造（単一CPU前提）
//
// 設計方針:
// - GDT/TSS は “ロード後に動かない” 静的領域へ固定配置
// - TSS 内の RSP0/IST は high-alias 仮想アドレスを格納（low-half 依存を断つ）
// - IST index は x86_64 crate の set_stack_index と同じ 0-based を使う
//   （set_stack_index は内部で +1 して IST1..IST7 を選ぶ）

#![allow(dead_code)]

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::instructions::interrupts;
use x86_64::instructions::segmentation::{CS, DS, ES, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

use crate::{arch::virt_layout, logging};

/// x86_64 crate の set_stack_index は “0-based” を受け取り内部で +1 して IST1.. を選ぶ。
/// したがって、TSS.interrupt_stack_table の index も 0-based で揃える。
pub const DOUBLE_FAULT_IST_INDEX: u16 = 0; // IST1
pub const PAGE_FAULT_IST_INDEX: u16 = 1;   // IST2

const RSP0_STACK_SIZE: usize = 4096 * 8;
const IST_STACK_SIZE: usize = 4096 * 8;

static INIT_DONE: AtomicBool = AtomicBool::new(false);

static mut GDT: MaybeUninit<GlobalDescriptorTable> = MaybeUninit::uninit();
static mut TSS: MaybeUninit<TaskStateSegment> = MaybeUninit::uninit();
static mut SELECTORS: MaybeUninit<Selectors> = MaybeUninit::uninit();

#[repr(C)]
#[derive(Clone, Copy)]
struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

/// `#[repr(align(N))]` は static ではなく “型” に付ける必要があるため、
/// アライン済みのスタック領域はラッパ型で表現する。
#[repr(align(16))]
struct AlignedStack<const N: usize> {
    buf: [u8; N],
}

impl<const N: usize> AlignedStack<N> {
    #[inline(always)]
    fn top_ptr(&self) -> *const u8 {
        // スタックは上位へ向かって伸びるので、top = base + size
        unsafe { self.buf.as_ptr().add(N) }
    }
}

static mut RSP0_STACK: AlignedStack<RSP0_STACK_SIZE> = AlignedStack { buf: [0; RSP0_STACK_SIZE] };
static mut DF_IST_STACK: AlignedStack<IST_STACK_SIZE> = AlignedStack { buf: [0; IST_STACK_SIZE] };
static mut PF_IST_STACK: AlignedStack<IST_STACK_SIZE> = AlignedStack { buf: [0; IST_STACK_SIZE] };

#[inline(always)]
fn high_alias_u64(low: u64) -> u64 {
    virt_layout::kernel_high_alias_of_low(low)
}

#[inline(always)]
fn align_down_16(x: u64) -> u64 {
    x & !0xFu64
}

pub fn init_high_alias() {
    interrupts::without_interrupts(|| {
        if INIT_DONE.swap(true, Ordering::SeqCst) {
            return;
        }

        unsafe {
            // ----------------------------
            // 1) TSS を静的領域へ構築
            // ----------------------------
            let mut tss = TaskStateSegment::new();

            let rsp0_low = VirtAddr::from_ptr(RSP0_STACK.top_ptr()).as_u64();
            let df_ist_low = VirtAddr::from_ptr(DF_IST_STACK.top_ptr()).as_u64();
            let pf_ist_low = VirtAddr::from_ptr(PF_IST_STACK.top_ptr()).as_u64();

            // TSS に入れる stack pointer は 16-byte aligned に揃える
            let rsp0_high = VirtAddr::new(align_down_16(high_alias_u64(rsp0_low)));
            let df_ist_high = VirtAddr::new(align_down_16(high_alias_u64(df_ist_low)));
            let pf_ist_high = VirtAddr::new(align_down_16(high_alias_u64(pf_ist_low)));

            // ring3→ring0 のスタック（将来用）
            tss.privilege_stack_table[0] = rsp0_high;

            // 例外用 IST（#DF/#PF）
            tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = df_ist_high;
            tss.interrupt_stack_table[PAGE_FAULT_IST_INDEX as usize] = pf_ist_high;

            TSS.write(tss);

            // ----------------------------
            // 2) GDT を静的領域へ構築
            //   ★TSS descriptor base を high-alias にする
            // ----------------------------
            let mut gdt = GlobalDescriptorTable::new();

            // TSS の low アドレス → high-alias アドレスへ変換して descriptor に埋める
            let tss_low_ptr_u64 = TSS.as_ptr() as u64;
            let tss_high_ptr_u64 = high_alias_u64(tss_low_ptr_u64);
            let tss_high_ptr = tss_high_ptr_u64 as *const TaskStateSegment;
            let tss_high_ref: &'static TaskStateSegment = &*tss_high_ptr;

            let code_sel = gdt.append(Descriptor::kernel_code_segment());
            let data_sel = gdt.append(Descriptor::kernel_data_segment());
            let tss_sel = gdt.append(Descriptor::tss_segment(tss_high_ref));

            GDT.write(gdt);
            SELECTORS.write(Selectors {
                code: code_sel,
                data: data_sel,
                tss: tss_sel,
            });

            // ----------------------------
            // 3) GDTR を “high-alias base” でロード
            // ----------------------------
            let gdt_low_ptr_u64 = GDT.as_ptr() as u64;
            let gdt_high_ptr_u64 = high_alias_u64(gdt_low_ptr_u64);
            let gdt_high_ptr = gdt_high_ptr_u64 as *const GlobalDescriptorTable;
            let gdt_high_ref: &'static GlobalDescriptorTable = &*gdt_high_ptr;

            gdt_high_ref.load();

            // ----------------------------
            // 4) セグメント / TR を更新
            // ----------------------------
            let sel = SELECTORS.assume_init_ref();
            CS::set_reg(sel.code);
            DS::set_reg(sel.data);
            ES::set_reg(sel.data);
            SS::set_reg(sel.data);
            load_tss(sel.tss);

            // ----------------------------
            // 5) ログ（確認ポイント）
            // ----------------------------
            logging::info("arch::gdt::init_high_alias: GDT/TSS loaded");

            logging::info_u64("tss_low", tss_low_ptr_u64);
            logging::info_u64("tss_high", tss_high_ptr_u64);
            logging::info_u64("tss_high_pml4", virt_layout::pml4_index(tss_high_ptr_u64) as u64);

            logging::info_u64("rsp0_low", rsp0_low);
            logging::info_u64("rsp0_high", rsp0_high.as_u64());
            logging::info_u64("rsp0_high_pml4", virt_layout::pml4_index(rsp0_high.as_u64()) as u64);

            logging::info_u64("df_ist_index", DOUBLE_FAULT_IST_INDEX as u64);
            logging::info_u64("df_ist_low", df_ist_low);
            logging::info_u64("df_ist_high", df_ist_high.as_u64());
            logging::info_u64("df_ist_high_pml4", virt_layout::pml4_index(df_ist_high.as_u64()) as u64);

            logging::info_u64("pf_ist_index", PAGE_FAULT_IST_INDEX as u64);
            logging::info_u64("pf_ist_low", pf_ist_low);
            logging::info_u64("pf_ist_high", pf_ist_high.as_u64());
            logging::info_u64("pf_ist_high_pml4", virt_layout::pml4_index(pf_ist_high.as_u64()) as u64);
        }
    });
}
