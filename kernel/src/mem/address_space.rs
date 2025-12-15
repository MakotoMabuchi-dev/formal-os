// kernel/src/mem/address_space.rs
//
// 役割:
// - 論理アドレス空間（プロセスやカーネルのメモリ空間）を表現する。
// - どの仮想ページがどの物理フレームにどの権限でマップされているかを保持する。
//
// 設計方針（プロトタイプ→フォーマル化を意識）:
// - unsafe は持ち込まない（arch 側に閉じ込める）。
// - kill 後始末で「Dead task の user mapping が残らない」を保証できる API を提供する。
// - 実ページテーブル操作は行わない（論理状態のみ）。

use crate::mem::addr::{PhysFrame, VirtPage};
use crate::mem::paging::{MemAction, PageFlags};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressSpaceKind {
    Kernel,
    User,
}

#[derive(Clone, Copy)]
pub struct Mapping {
    pub page: VirtPage,
    pub frame: PhysFrame,
    pub flags: PageFlags,
}

const MAX_MAPPINGS: usize = 64;

pub struct AddressSpace {
    pub kind: AddressSpaceKind,
    pub root_page_frame: Option<PhysFrame>,
    mappings: [Option<Mapping>; MAX_MAPPINGS],
}

#[derive(Clone, Copy, Debug)]
pub enum AddressSpaceError {
    AlreadyMapped,
    NotMapped,
    CapacityExceeded,
}

impl AddressSpace {
    pub fn new_kernel() -> Self {
        AddressSpace {
            kind: AddressSpaceKind::Kernel,
            root_page_frame: None,
            mappings: [None; MAX_MAPPINGS],
        }
    }

    pub fn new_user() -> Self {
        AddressSpace {
            kind: AddressSpaceKind::User,
            root_page_frame: None,
            mappings: [None; MAX_MAPPINGS],
        }
    }

    pub fn apply(&mut self, action: MemAction) -> Result<(), AddressSpaceError> {
        match action {
            MemAction::Map { page, frame, flags } => {
                for entry in self.mappings.iter() {
                    if let Some(m) = entry {
                        if m.page == page {
                            return Err(AddressSpaceError::AlreadyMapped);
                        }
                    }
                }

                for entry in self.mappings.iter_mut() {
                    if entry.is_none() {
                        *entry = Some(Mapping { page, frame, flags });
                        return Ok(());
                    }
                }

                Err(AddressSpaceError::CapacityExceeded)
            }

            MemAction::Unmap { page } => {
                for entry in self.mappings.iter_mut() {
                    if let Some(m) = entry {
                        if m.page == page {
                            *entry = None;
                            return Ok(());
                        }
                    }
                }
                Err(AddressSpaceError::NotMapped)
            }
        }
    }

    pub fn mapping_count(&self) -> usize {
        self.mappings.iter().filter(|m| m.is_some()).count()
    }

    pub fn for_each_mapping<F>(&self, mut f: F)
    where
        F: FnMut(&Mapping),
    {
        for entry in self.mappings.iter() {
            if let Some(ref m) = entry {
                f(m);
            }
        }
    }

    // -------------------------------------------------------------------------
    // Step1 (Top3): kill 後始末のための補助 API
    // -------------------------------------------------------------------------

    /// user mapping（flags に USER が付いている mapping）の「ページ」だけを列挙する。
    ///
    /// 目的:
    /// - kill 後始末で「実ページテーブル側の unmap」を確実に行うための材料。
    /// - Vec を使わず、固定長配列に収集できるようにする（再現性重視）。
    pub fn for_each_user_mapping_page<F>(&self, mut f: F)
    where
        F: FnMut(VirtPage),
    {
        for entry in self.mappings.iter() {
            if let Some(m) = entry {
                if m.flags.contains(PageFlags::USER) {
                    f(m.page);
                }
            }
        }
    }

    /// user mapping（flags に USER が付いている mapping）を論理状態から全て消す。
    ///
    /// 注意:
    /// - これは「論理 AddressSpace の掃除」だけ。
    /// - 実ページテーブルの unmap は arch 側で別途実行すること。
    pub fn clear_user_mappings(&mut self) {
        for entry in self.mappings.iter_mut() {
            if let Some(m) = entry {
                if m.flags.contains(PageFlags::USER) {
                    *entry = None;
                }
            }
        }
    }
}
