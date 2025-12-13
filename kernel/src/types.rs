/*!
 * types
 *
 * 役割:
 *   - カーネル全体で共有する素朴な型・定数を集約する。
 *
 * やること:
 *   - 物理/仮想アドレス、ページサイズ、PML4 index 計算などの共通ユーティリティ。
 *
 * やらないこと:
 *   - ページングや CR3 切替などの arch 依存処理。
 *
 * 設計方針:
 *   - 依存を増やさず、共通処理をここに寄せる。
 */

use core::fmt;

pub type PhysAddr = u64;
pub type VirtAddr = u64;

pub const PAGE_SIZE: u64 = 4096;

pub const PML4_ENTRY_COUNT: usize = 512;
pub const PML4_INDEX_BITS: u64 = 9;
pub const PML4_ENTRY_SHIFT: u64 = 39; // 512GiB
pub const PML4_ENTRY_SIZE: u64 = 1u64 << PML4_ENTRY_SHIFT;
pub const PML4_INDEX_MASK: u64 = (1u64 << PML4_INDEX_BITS) - 1;

/// 仮想アドレスから PML4 index を取り出す（48-bit 仮想前提）
pub fn pml4_index(virt: VirtAddr) -> usize {
    ((virt >> PML4_ENTRY_SHIFT) & PML4_INDEX_MASK) as usize
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryRegionType {
    Usable,
    Reserved,
    Other,
}

impl fmt::Display for MemoryRegionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryRegionType::Usable => write!(f, "Usable"),
            MemoryRegionType::Reserved => write!(f, "Reserved"),
            MemoryRegionType::Other => write!(f, "Other"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub index: usize,
    pub start_phys: PhysAddr,
    pub end_phys: PhysAddr,
    pub region_type: MemoryRegionType,
}

impl MemoryRegion {
    pub fn size_bytes(&self) -> u64 {
        self.end_phys.saturating_sub(self.start_phys)
    }
}
