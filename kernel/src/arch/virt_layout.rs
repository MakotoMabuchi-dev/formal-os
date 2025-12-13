// kernel/src/arch/virt_layout.rs
//
// 【役割】
//  x86_64 の仮想アドレスレイアウト（low-half / high-half / alias / user slot）を 1 箇所に集約。
//  paging.rs / kernel.rs に散らばる定数や計算の事故を防ぐ。
//
// 【やること】
//  - PML4 index 抽出
//  - canonical(48bit) な high 側アドレス生成
//  - kernel high-alias の “low->high” 変換
//  - user slot（PML4 1 スロット固定）定義
//  - guard(code/stack) から alias コピー数を推奨計算
//
// 【やらないこと】
//  - 実際のページテーブル操作（map/unmap）
//  - BootInfo 依存の配置決定

#![allow(dead_code)]

pub const PAGE_SIZE: u64 = 4096;

// 1 PML4 entry covers 512 GiB.
pub const PML4_ENTRY_COVERAGE: u64 = 1u64 << 39;

// canonical boundary (48-bit)
pub const CANONICAL_HIGH_MASK: u64 = 0xFFFF_0000_0000_0000;

// ----------------------
// User slot policy
// ----------------------

// User 空間は PML4 1 スロットに閉じ込める（512GiB）
pub const USER_PML4_INDEX: usize = 4;
pub const USER_SPACE_BASE: u64 = (USER_PML4_INDEX as u64) << 39;
pub const USER_SPACE_SIZE: u64 = 1u64 << 39;

// ----------------------
// Kernel high-alias policy
// ----------------------
//
// low 側の PML4 index 0..N を high 側の 508..(508+N) にコピーする運用。
// 508..511 を使うと canonical high になりやすく、衝突もしにくい。
//
pub const KERNEL_ALIAS_DST_PML4_BASE_INDEX: usize = 508;

// ----------------------
// Bit helpers
// ----------------------

/// PML4 index (bits 47..39)
pub const fn pml4_index(addr: u64) -> usize {
    ((addr >> 39) & 0x1FF) as usize
}

/// 「その PML4 スロット内オフセット」（下位 39bit）
pub const fn pml4_slot_offset(addr: u64) -> u64 {
    addr & (PML4_ENTRY_COVERAGE - 1)
}

/// idx(0..511) と offset(0..512GiB) から canonical な仮想アドレスを作る
pub const fn make_canonical_from_pml4(idx: usize, offset: u64) -> u64 {
    let addr48 = ((idx as u64) << 39) | (offset & (PML4_ENTRY_COVERAGE - 1));

    // idx>=256 なら bit47=1 なので high canonical
    if idx >= 256 {
        addr48 | CANONICAL_HIGH_MASK
    } else {
        addr48
    }
}

/// low addr を high-alias addr へ写す
///
/// - low の PML4 index を保持して dst_base に足し込む
/// - offset は PML4 スロット内オフセット（下位39bit）
///
/// 例: low の pml4=2 のアドレス -> high の pml4=510 の同じ offset
pub const fn kernel_high_alias_of_low(low_addr: u64) -> u64 {
    let low_idx = pml4_index(low_addr);
    let off = pml4_slot_offset(low_addr);

    let high_idx = KERNEL_ALIAS_DST_PML4_BASE_INDEX + low_idx;
    make_canonical_from_pml4(high_idx, off)
}

/// guard(code/stack) から「最低限必要な alias copy count」を推奨
///
/// 返り値は “src 側でコピーすべき個数” なので、src=0..count-1 をコピーする。
pub const fn recommend_alias_copy_count_from_guards(code_low: u64, stack_low: u64) -> usize {
    let mut max_idx = pml4_index(code_low);
    let s = pml4_index(stack_low);
    if s > max_idx {
        max_idx = s;
    }

    // max_idx=0 -> 1個, max_idx=3 -> 4個
    max_idx + 1
}
