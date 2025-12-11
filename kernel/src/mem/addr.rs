// src/mem/addr.rs
//
// 役割:
// - 物理アドレス / 仮想アドレス / フレーム / ページなど、メモリ関連の基本型を定義する。
// - まだページテーブルの実装には踏み込まず、「数値に型を付ける」ことが目的。
// やること:
// - u64 の生アドレス値に対して、「これは物理アドレス」「これは仮想ページ」と区別できるようにする。
// やらないこと:
// - CPU の CR3 や PTE などを直接触る処理は書かない（それは arch / hw 側で行う）。

use core::fmt;

/// 物理アドレス（バイト単位）
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct PhysAddr(pub u64);

/// 仮想アドレス（バイト単位）
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct VirtAddr(pub u64);

/// ページサイズ（4KiB 固定でスタート）
pub const PAGE_SIZE: u64 = 4096;

/// 物理フレーム（4KiB ごとの番号）
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct PhysFrame {
    pub number: u64, // frame index = phys_addr / PAGE_SIZE
}

/// 仮想ページ（4KiB ごとの番号）
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct VirtPage {
    pub number: u64, // page index = virt_addr / PAGE_SIZE
}

impl PhysAddr {
    /// 下位ビットを切り捨てて、ページ境界に揃える。
    pub fn align_down(self) -> PhysAddr {
        PhysAddr(self.0 & !(PAGE_SIZE - 1))
    }

    /// このアドレスが含まれる物理フレームを返す。
    pub fn frame(self) -> PhysFrame {
        PhysFrame {
            number: self.0 / PAGE_SIZE,
        }
    }
}

impl VirtAddr {
    /// 下位ビットを切り捨てて、ページ境界に揃える。
    pub fn align_down(self) -> VirtAddr {
        VirtAddr(self.0 & !(PAGE_SIZE - 1))
    }

    /// このアドレスが含まれる仮想ページを返す。
    pub fn page(self) -> VirtPage {
        VirtPage {
            number: self.0 / PAGE_SIZE,
        }
    }
}

impl PhysFrame {
    /// フレーム先頭の物理アドレスを返す。
    pub fn start_address(self) -> PhysAddr {
        PhysAddr(self.number * PAGE_SIZE)
    }

    /// インデックスから直接フレームを作る（テスト用途など）。
    pub const fn from_index(number: u64) -> Self {
        PhysFrame { number }
    }
}

impl VirtPage {
    /// ページ先頭の仮想アドレスを返す。
    pub fn start_address(self) -> VirtAddr {
        VirtAddr(self.number * PAGE_SIZE)
    }

    /// インデックスから直接仮想ページを作る。
    pub const fn from_index(number: u64) -> Self {
        VirtPage { number }
    }
}

// --- Debug 実装（ログで見やすくするため） ---

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 物理アドレスを 0x... 形式で表示
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 仮想アドレスを 0x... 形式で表示
        write!(f, "VirtAddr({:#x})", self.0)
    }
}

impl fmt::Debug for PhysFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // フレーム先頭の物理アドレスを表示
        write!(f, "PhysFrame({:#x})", self.start_address().0)
    }
}

impl fmt::Debug for VirtPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // ページ先頭の仮想アドレスを表示
        write!(f, "VirtPage({:#x})", self.start_address().0)
    }
}
