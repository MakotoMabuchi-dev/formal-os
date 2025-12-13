// kernel/src/types.rs
/*!
 * types
 *
 * 役割:
 *   - カーネル全体で共有する素朴な型・定数を集約する（補助）。
 *
 * 方針:
 *   - 可能な限り mem::addr 側の型を再利用し、二重定義を避ける。
 *   - “本体の正” は mem::addr / arch::virt_layout とし、ここは補助に留める。
 */

#![allow(dead_code)]

use core::fmt;

pub use crate::mem::addr::PhysAddr;

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
        self.end_phys.0.saturating_sub(self.start_phys.0)
    }
}
