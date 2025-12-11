// kernel/src/mem/layout.rs
//
// formal-os: x86_64 仮想アドレスレイアウト（仕様レベル）
//
// 目的:
// - 「ユーザ空間」と「カーネル空間＋物理メモリ窓(physmap)」の概念的な境界を固定しておく。
// - まだ実ページテーブルには反映しないが、将来の設計議論・フォーマル記述の前提になる。
// - 実装が変わっても、ここの定数を変えることで「OSが守るべきアドレス空間の型」を更新できる。
//
// 想定レイアウト（案）:
//
//   0x0000_0000_0000_0000 ..= 0x0000_7fff_ffff_ffff
//     - user 空間 (canonical low half)
//     - プロセスごとに異なるマッピングを持つ領域
//
//   0xffff_8000_0000_0000 ..= 0xffff_ffff_ffff_ffff
//     - kernel 空間 (canonical high half の一部)
//     - 内部に「カーネル専用領域」「物理メモリ窓(physmap)」などをサブ分割していく想定。
//     - ここは全タスクで共有される（ユーザタスクからは privilege で保護）。
//
// まだ細かい分割（例: カーネルコード/データ、heap、physmap 等）は決めず、
// 大きな 2分割だけをまず仕様として固定しておく。
//

/// ユーザ空間（low half）の開始アドレス。
pub const USER_SPACE_START: u64 = 0x0000_0000_0000_0000;

/// ユーザ空間（low half）の終了アドレス（暫定）。
pub const USER_SPACE_END: u64 = 0x0000_7fff_ffff_ffff;

/// カーネル空間（＋ physmap を含む high half）の開始アドレス（暫定）。
pub const KERNEL_SPACE_START: u64 = 0xffff_8000_0000_0000;

/// 将来、物理メモリ窓(physmap) をこのあたりに置く想定の開始アドレス（案）。
pub const PHYSMAP_START: u64 = 0xffff_8000_0000_0000;

/// physmap の終了アドレス（暫定）。今は「とりあえず 512GiB 分くらいを仮置き」のイメージ。
pub const PHYSMAP_END: u64 = 0xffff_87ff_ffff_ffff;
