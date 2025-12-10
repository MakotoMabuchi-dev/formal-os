// kernel/src/mem/address_space.rs
//
// 役割:
// - 論理的なアドレス空間（どの仮想ページがどの物理フレームにマップされているか）を表す。
// - フォーマル検証しやすいように、ページ単位のMappingを固定長配列で保持する。
// - 「このアドレス空間に対して MemAction(Map/Unmap) を適用するとどうなるか」を純粋関数として表現する。
// - root_page_frame は、このアドレス空間に対応する L4 ページテーブルの物理フレーム。
//   （※ 現時点では Task0 のカーネル空間用だけが設定される）

use crate::mem::addr::{PhysFrame, VirtPage};
use crate::mem::paging::{MemAction, PageFlags};

/// 1つのアドレス空間の中で管理する最大マッピング数。
const MAX_MAPPINGS: usize = 64;

/// 1つの仮想ページに対するマッピング情報。
#[derive(Clone, Copy)]
pub struct Mapping {
    pub page: VirtPage,
    pub frame: PhysFrame,
    pub flags: PageFlags,
}

/// アドレス空間全体を表す構造。
#[derive(Clone, Copy)]
pub struct AddressSpace {
    pub root_page_frame: Option<PhysFrame>,          // L4 page table の物理フレーム
    mappings: [Option<Mapping>; MAX_MAPPINGS],       // 論理的なページマップ
}

/// AddressSpace 更新時のエラー種別。
#[derive(Clone, Copy, Debug)]
pub enum AddressSpaceError {
    AlreadyMapped,
    NotMapped,
    CapacityExceeded,
}

impl AddressSpace {
    /// 空のアドレス空間を作成する。
    pub fn new() -> Self {
        AddressSpace {
            root_page_frame: None,
            mappings: [None; MAX_MAPPINGS],
        }
    }

    /// このアドレス空間に MemAction を適用する（論理的な状態更新）。
    pub fn apply(&mut self, action: MemAction) -> Result<(), AddressSpaceError> {
        match action {
            MemAction::Map { page, frame, flags } => {
                // すでに同じ page が存在していないかチェック
                for entry in self.mappings.iter() {
                    if let Some(m) = entry {
                        if m.page == page {
                            return Err(AddressSpaceError::AlreadyMapped);
                        }
                    }
                }

                // 空きスロットに追加
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

    /// 登録されているマッピング数（デバッグ / 可視化用）
    pub fn mapping_count(&self) -> usize {
        self.mappings.iter().filter(|m| m.is_some()).count()
    }

    /// すべてのマッピングに対してコールバックを呼び出す（デバッグ / 可視化用）。
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
}
