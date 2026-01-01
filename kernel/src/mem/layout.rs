// kernel/src/mem/layout.rs
#![allow(dead_code)]

// formal-os: x86_64 仮想アドレスレイアウト（仕様レベル）
//
// 目的:
// - 「ユーザ空間」と「カーネル空間＋物理メモリ窓(physmap)」の概念的な境界を固定しておく。
// - まだ実ページテーブルには反映しないが、将来の設計議論・フォーマル記述の前提になる。
// - 実装が変わっても、ここの定数を変えることで「OSが守るべきアドレス空間の型」を更新できる。
//
// 重要（現行実装との整合）:
// - 現時点の実装は「low-half 全域を user として許可」ではなく、
//   1つの PML4 スロットを user slot として予約している。
// - USER_SPACE_* は「予約した user slot 範囲」を表す（= 実装のポリシーと一致）
//
// 想定レイアウト（現行の reserved slot 版）:
//
//   0x0000_1000_0000_0000 ..= 0x0000_107f_ffff_ffff
//     - user slot (PML4 index 32, 512GiB)
//     - プロセスごとに異なるマッピングを持つ領域（当面はこの範囲のみ許可）
//
//   0xffff_8000_0000_0000 ..= 0xffff_ffff_ffff_ffff
//     - kernel 空間 (canonical high half の一部)
//     - 内部に「カーネル専用領域」「物理メモリ窓(physmap)」などをサブ分割していく想定。
//     - ここは全タスクで共有される（ユーザタスクからは privilege で保護）。
//

/// 1つの PML4 エントリがカバーする仮想アドレス範囲（512GiB）
pub const PML4_SLOT_SIZE: u64 = 1u64 << 39;

/// 現行実装で user slot に予約する PML4 index
/// （arch::virt_layout と合わせる前提）
pub const USER_PML4_INDEX: u64 = 32;

/// ユーザ空間（reserved user slot）の開始アドレス。
pub const USER_SPACE_START: u64 = USER_PML4_INDEX * PML4_SLOT_SIZE;

/// ユーザ空間（reserved user slot）の終了アドレス。
pub const USER_SPACE_END: u64 = USER_SPACE_START + (PML4_SLOT_SIZE - 1);

/// カーネル空間（＋ physmap を含む high half）の開始アドレス（暫定）。
pub const KERNEL_SPACE_START: u64 = 0xffff_8000_0000_0000;

/// 将来、物理メモリ窓(physmap) をこのあたりに置く想定の開始アドレス（案）。
pub const PHYSMAP_START: u64 = 0xffff_8000_0000_0000;

/// physmap の終了アドレス（暫定）。今は「とりあえず 512GiB 分くらいを仮置き」のイメージ。
pub const PHYSMAP_END: u64 = 0xffff_87ff_ffff_ffff;
