// kernel/src/mem/addr.rs
//
// 役割:
// - 物理アドレス / 仮想アドレス / フレーム / ページなど、メモリ関連の基本型を定義する。
// - まだページテーブルの実装には踏み込まず、「数値に型を付ける」ことが目的。

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
    pub const fn new(addr: u64) -> Self {
        PhysAddr(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub fn align_down(self) -> PhysAddr {
        PhysAddr(self.0 & !(PAGE_SIZE - 1))
    }

    pub fn frame(self) -> PhysFrame {
        PhysFrame {
            number: self.0 / PAGE_SIZE,
        }
    }
}

impl VirtAddr {
    pub const fn new(addr: u64) -> Self {
        VirtAddr(addr)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub fn align_down(self) -> VirtAddr {
        VirtAddr(self.0 & !(PAGE_SIZE - 1))
    }

    pub fn page(self) -> VirtPage {
        VirtPage {
            number: self.0 / PAGE_SIZE,
        }
    }
}

impl From<u64> for PhysAddr {
    fn from(v: u64) -> Self {
        PhysAddr(v)
    }
}

impl From<u64> for VirtAddr {
    fn from(v: u64) -> Self {
        VirtAddr(v)
    }
}

impl From<PhysAddr> for u64 {
    fn from(v: PhysAddr) -> Self {
        v.0
    }
}

impl From<VirtAddr> for u64 {
    fn from(v: VirtAddr) -> Self {
        v.0
    }
}

impl PhysFrame {
    pub fn start_address(self) -> PhysAddr {
        PhysAddr(self.number * PAGE_SIZE)
    }

    pub const fn from_index(number: u64) -> Self {
        PhysFrame { number }
    }
}

impl VirtPage {
    pub fn start_address(self) -> VirtAddr {
        VirtAddr(self.number * PAGE_SIZE)
    }

    pub const fn from_index(number: u64) -> Self {
        VirtPage { number }
    }
}

impl fmt::Debug for PhysAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysAddr({:#x})", self.0)
    }
}

impl fmt::Debug for VirtAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtAddr({:#x})", self.0)
    }
}

impl fmt::Debug for PhysFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PhysFrame({:#x})", self.start_address().0)
    }
}

impl fmt::Debug for VirtPage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VirtPage({:#x})", self.start_address().0)
    }
}
