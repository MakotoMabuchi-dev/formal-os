/*
役割:
- x86_64 の仮想アドレス空間レイアウト（PML4 スロット割り当て）と、
  その計算を行う純粋関数を提供する。

やること:
- USER 空間の PML4 スロット位置と範囲の定義
- kernel low-half → kernel high-alias 変換（同一物理を別仮想で参照）
- PML4 index 抽出などのビット演算ヘルパ
- high-alias に必要な「コピー数」の推奨（guards / 実行コンテキスト）

やらないこと:
- ページテーブルを触る（それは arch::paging 側の責務）
- 物理メモリ管理（mm 側の責務）

設計方針:
- ここは「アドレス計算だけ」に限定し、副作用を持たせない
- high-alias は paging 側のコピー規則（dst = base + src）と完全に一致させる
- 返り値（copy_count）は alias 窓の幅を超えないよう上限を持つ（過大コピー防止）
*/

/// 1つの PML4 エントリがカバーする仮想アドレス範囲（512GiB）
pub const PML4_SLOT_SIZE: u64 = 1u64 << 39;

/// USER 空間に予約する PML4 index（あなたのログでは 4 を使っている前提）
pub const USER_PML4_INDEX: usize = 4;

/// PML4 index の開始アドレス（slot の base）を返す
#[inline(always)]
pub const fn pml4_index_base_addr(index: usize) -> u64 {
    canonicalize_virt((index as u64) << 39)
}

/// USER 空間ベース（PML4 index 4 の開始アドレス）
pub const USER_SPACE_BASE: u64 = pml4_index_base_addr(USER_PML4_INDEX);

/// USER 空間サイズ（PML4 1スロット分: 512GiB）
pub const USER_SPACE_SIZE: u64 = PML4_SLOT_SIZE;

/// kernel high-alias を配置する先の PML4 index（あなたのログの値と一致させる）
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
        // 上位を 1 で埋める
        addr | 0xffff_0000_0000_0000
    } else {
        // 上位を 0 にする（念のため 48bit に丸める）
        addr & 0x0000_ffff_ffff_ffff
    }
}

/// low 側アドレスを、high-alias 側へ写像する。
///
/// 重要:
/// - paging 側では `dst = KERNEL_ALIAS_DST_PML4_BASE_INDEX + src` として
///   PML4 エントリをコピーしている。
/// - なので、ここでも low の PML4 index を保ったまま、dst 側へ移す必要がある。
///
/// 例:
/// - low が PML4=0 の場合 → high は PML4=508
/// - low が PML4=2 の場合 → high は PML4=510
#[inline(always)]
pub fn kernel_high_alias_of_low(low_addr: u64) -> u64 {
    let low_idx = pml4_index(low_addr);
    let offset_in_slot = low_addr & (PML4_SLOT_SIZE - 1);

    // dst 側は 508..511 の 4スロットを想定
    // low_idx が 0..3 以外なら、設計（alias 窓の幅）と不一致。
    debug_assert!(
        low_idx < KERNEL_ALIAS_MAX_COPY_COUNT,
        "low pml4 index too large for alias window"
    );

    let high_idx = KERNEL_ALIAS_DST_PML4_BASE_INDEX + low_idx;
    pml4_index_base_addr(high_idx) + offset_in_slot
}

/// alias に必要な copy_count を「最大 pml4_index + 1」で返す共通ロジック。
/// - 返り値は 1..=KERNEL_ALIAS_MAX_COPY_COUNT にクランプする
/// - 0 アドレス（未初期化値）は無視する
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

    // 何も無い（全部0）場合は最小 1 スロットだけコピー（fail-safe）
    let mut res = if any { max_idx + 1 } else { 1 };

    if res == 0 {
        res = 1;
    }

    // alias 窓の幅を超えないように上限を設ける
    if res > KERNEL_ALIAS_MAX_COPY_COUNT {
        res = KERNEL_ALIAS_MAX_COPY_COUNT;
    }

    res
}

/// guard（code/stack）から alias に必要なコピー数（src 側の 0..N）を推定する。
/// 返り値 N は「src index 0..N-1 をコピーする」想定の個数。
#[inline(always)]
pub fn recommend_alias_copy_count_from_guards(code_low: u64, stack_low: u64) -> usize {
    recommend_alias_copy_count_from_addrs(&[code_low, stack_low])
}

/// code/rsp/rbp を使って copy_count を推定したい場合の拡張版。
#[inline(always)]
pub fn recommend_alias_copy_count_from_context(code_low: u64, rsp_low: u64, rbp_low: u64) -> usize {
    recommend_alias_copy_count_from_addrs(&[code_low, rsp_low, rbp_low])
}
