// kernel/src/mem/address_space.rs
//
// 役割:
// - 論理アドレス空間（プロセスやカーネルのメモリ空間）を表現する。
// - どの仮想ページがどの物理フレームにどの権限でマップされているかを保持する。
// - kind で「カーネル用かユーザ用か」を明示し、将来の権限チェックのベースにする。
//
// 設計方針（プロトタイプ）:
// - Kernel: VirtPage は “実仮想アドレス / PAGE_SIZE” のページ番号として扱う。
// - User: VirtPage は “user slot 内 offset / PAGE_SIZE” のページ番号として扱う。
//   実仮想アドレスは arch::paging 側で USER_SPACE_BASE を加算して作る。
//   （low-half カーネルの間はこの設計が安全で、high-half 移行時も定数変更で吸収できる）
//
// やらないこと（今は）:
// - 複雑な VMA 管理や demand paging
// - ユーザ/カーネルの完全分離（high-half 化後に段階導入）
//

use crate::mem::addr::{PhysFrame, VirtPage};
use crate::mem::paging::{MemAction, PageFlags};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressSpaceKind {
    Kernel,
    User,
}

#[derive(Clone, Copy)]
pub struct Mapping {
    pub page:  VirtPage,
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

    #[allow(dead_code)]
    pub fn new_with_kind(kind: AddressSpaceKind) -> Self {
        AddressSpace {
            kind,
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
}
