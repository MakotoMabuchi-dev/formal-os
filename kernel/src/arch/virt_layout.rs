// kernel/src/arch/virt_layout.rs
//
// 役割:
// - x86_64 の仮想アドレス空間レイアウト（PML4 スロット割り当て）と、
//   その計算を行う純粋関数を提供する。
//
// やること:
// - USER 空間の PML4 スロット位置と範囲の定義
// - kernel low-half → kernel high-alias 変換（同一物理を別仮想で参照）
// - PML4 index 抽出などのビット演算ヘルパ
//
// やらないこと:
// - ページテーブルを触る（arch::paging 側の責務）
//
// 設計方針:
// - ここは「アドレス計算だけ」に限定し、副作用を持たせない
// - high-alias は paging 側のコピー規則（dst = base + src）と完全に一致させる

/// 1つの PML4 エントリがカバーする仮想アドレス範囲（512GiB）
pub const PML4_SLOT_SIZE: u64 = 1u64 << 39;

/// USER 空間に予約する PML4 index
///
/// 重要:
/// - physmap が低い PML4 index に来る環境がある（今回のログでは physmap = 3）
/// - paging 側は physmap を「少し多めに」コピーする（PHYSMAP_PML4_COPY_COUNT）
/// - そのため USER slot を physmap 近傍（例: 3..7）に置くと衝突して危険
pub const USER_PML4_INDEX: usize = 32;

/// PML4 index の開始アドレス（slot の base）を返す
#[inline(always)]
pub const fn pml4_index_base_addr(index: usize) -> u64 {
    canonicalize_virt((index as u64) << 39)
}

/// USER 空間ベース（PML4 index USER_PML4_INDEX の開始アドレス）
pub const USER_SPACE_BASE: u64 = pml4_index_base_addr(USER_PML4_INDEX);

/// USER 空間サイズ（PML4 1スロット分: 512GiB）
pub const USER_SPACE_SIZE: u64 = PML4_SLOT_SIZE;

/// kernel high-alias を配置する先の PML4 index（508..511）
pub const KERNEL_ALIAS_DST_PML4_BASE_INDEX: usize = 508;

/// base から使えるスロット数（508..=511 の 4スロット）
pub const KERNEL_ALIAS_MAX_COPY_COUNT: usize = 512 - KERNEL_ALIAS_DST_PML4_BASE_INDEX;

/// 指定アドレスの PML4 index（bits 47..39）
#[inline(always)]
pub const fn pml4_index(addr: u64) -> usize {
    ((addr >> 39) & 0x1ff) as usize
}

/// 48bit canonical への正規化（bit47 を sign-extend）
#[inline(always)]
pub const fn canonicalize_virt(addr: u64) -> u64 {
    let sign_bit = 1u64 << 47;
    if (addr & sign_bit) != 0 {
        addr | 0xffff_0000_0000_0000
    } else {
        addr & 0x0000_ffff_ffff_ffff
    }
}

/// low 側アドレスを、high-alias 側へ写像する。
#[inline(always)]
pub fn kernel_high_alias_of_low(low_addr: u64) -> u64 {
    let low_idx = pml4_index(low_addr);
    let offset_in_slot = low_addr & (PML4_SLOT_SIZE - 1);

    debug_assert!(
        low_idx < KERNEL_ALIAS_MAX_COPY_COUNT,
        "low pml4 index too large for alias window"
    );

    let high_idx = KERNEL_ALIAS_DST_PML4_BASE_INDEX + low_idx;
    pml4_index_base_addr(high_idx) + offset_in_slot
}

// -----------------------------------------------------------------------------
// alias copy count recommendation (optional)
// - paging 側で MAX 固定を採用している場合は不要で unused になりがちなので、feature 化する
// -----------------------------------------------------------------------------

/// alias に必要な copy_count を「最大 pml4_index + 1」で返す共通ロジック。
/// - 返り値は 1..=KERNEL_ALIAS_MAX_COPY_COUNT にクランプする
/// - 0 アドレス（未初期化値）は無視する
#[cfg(feature = "alias_copycount_auto")]
#[inline(always)]
pub fn recommend_alias_copy_count_from_addrs(addrs: &[u64]) -> usize {
    let mut max_idx: usize = 0;
    let mut any = false;

    for &a in addrs {
        if a == 0 {
            continue;
        }
        any = true;

        let idx = pml4_index(a);
        if idx > max_idx {
            max_idx = idx;
        }
    }

    let mut res = if any { max_idx + 1 } else { 1 };
    if res == 0 {
        res = 1;
    }
    if res > KERNEL_ALIAS_MAX_COPY_COUNT {
        res = KERNEL_ALIAS_MAX_COPY_COUNT;
    }

    res
}

/// guard（code/stack）から alias に必要なコピー数を推定する
#[cfg(feature = "alias_copycount_auto")]
#[inline(always)]
pub fn recommend_alias_copy_count_from_guards(code_low: u64, stack_low: u64) -> usize {
    recommend_alias_copy_count_from_addrs(&[code_low, stack_low])
}

/// code/rsp/rbp を使って copy_count を推定したい場合の拡張版
#[cfg(feature = "alias_copycount_auto")]
#[inline(always)]
pub fn recommend_alias_copy_count_from_context(code_low: u64, rsp_low: u64, rbp_low: u64) -> usize {
    recommend_alias_copy_count_from_addrs(&[code_low, rsp_low, rbp_low])
}
