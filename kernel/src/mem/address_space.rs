// kernel/src/mem/address_space.rs
//
// 役割:
// - 論理アドレス空間（プロセスやカーネルのメモリ空間）を表現する。
// - どの仮想ページがどの物理フレームにどの権限でマップされているかを保持する。
// - kind で「カーネル用かユーザ用か」を明示し、将来の権限チェックのベースにする。
//
// 方針:
// - ここは “論理モデル” なので実ページテーブル操作は行わない。
// - ただしプロトタイプ段階でも危険な誤用（User が kernel-space を map 等）は早めに弾く。

use crate::mem::addr::{PhysFrame, VirtPage, PAGE_SIZE};
use crate::mem::layout::KERNEL_SPACE_START;
use crate::mem::paging::{MemAction, PageFlags};

/// アドレス空間の種類（役割）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressSpaceKind {
    /// カーネル専用のアドレス空間（カーネルコードやカーネル用スタックなど）
    Kernel,
    /// ユーザプロセス用のアドレス空間
    User,
}

/// 1つの仮想ページに対するマッピング情報。
#[derive(Clone, Copy)]
pub struct Mapping {
    pub page:  VirtPage,
    pub frame: PhysFrame,
    pub flags: PageFlags,
}

/// 管理可能なマッピング数上限（デモ用の固定長）。
const MAX_MAPPINGS: usize = 64;

/// 複数のタスクから共有されることもある論理アドレス空間。
pub struct AddressSpace {
    /// このアドレス空間がカーネル用かユーザ用か。
    pub kind: AddressSpaceKind,
    /// このアドレス空間に対応する L4 PageTable の物理フレーム（将来 CR3 に使う）。
    pub root_page_frame: Option<PhysFrame>,
    /// 仮想ページ→物理フレームのマッピング情報。
    mappings: [Option<Mapping>; MAX_MAPPINGS],
}

/// AddressSpace 更新時のエラー種別。
#[derive(Clone, Copy, Debug)]
pub enum AddressSpaceError {
    /// すでに同じ page がマップされているのに、Map が来た。
    AlreadyMapped,
    /// その page はマップされていないのに、Unmap が来た。
    NotMapped,
    /// 登録できるスロットがもう残っていない。
    CapacityExceeded,

    /// User AS が kernel-space に map しようとした。
    UserMappingInKernelSpace,
    /// User AS の map なのに USER フラグが無い。
    UserMappingMissingUserFlag,
    /// Kernel AS の map なのに USER フラグが付いている。
    KernelMappingHasUserFlag,
}

impl AddressSpace {
    /// カーネル用アドレス空間を新規に作成する。
    pub fn new_kernel() -> Self {
        AddressSpace {
            kind: AddressSpaceKind::Kernel,
            root_page_frame: None,
            mappings: [None; MAX_MAPPINGS],
        }
    }

    /// ユーザ用アドレス空間を新規に作成する。
    pub fn new_user() -> Self {
        AddressSpace {
            kind: AddressSpaceKind::User,
            root_page_frame: None,
            mappings: [None; MAX_MAPPINGS],
        }
    }

    /// 汎用的に kind を指定して新規作成したい場合はこちら。
    #[allow(dead_code)]
    pub fn new_with_kind(kind: AddressSpaceKind) -> Self {
        AddressSpace {
            kind,
            root_page_frame: None,
            mappings: [None; MAX_MAPPINGS],
        }
    }

    fn validate_map(&self, page: VirtPage, flags: PageFlags) -> Result<(), AddressSpaceError> {
        let virt_addr = page.number * PAGE_SIZE;

        match self.kind {
            AddressSpaceKind::User => {
                if virt_addr >= KERNEL_SPACE_START {
                    return Err(AddressSpaceError::UserMappingInKernelSpace);
                }
                if !flags.contains(PageFlags::USER) {
                    return Err(AddressSpaceError::UserMappingMissingUserFlag);
                }
            }
            AddressSpaceKind::Kernel => {
                if flags.contains(PageFlags::USER) {
                    return Err(AddressSpaceError::KernelMappingHasUserFlag);
                }
            }
        }

        Ok(())
    }

    /// このアドレス空間に対して MemAction (Map/Unmap) を適用する。
    pub fn apply(&mut self, action: MemAction) -> Result<(), AddressSpaceError> {
        match action {
            MemAction::Map { page, frame, flags } => {
                self.validate_map(page, flags)?;

                // すでに同じ page が存在していないかチェック
                for entry in self.mappings.iter() {
                    if let Some(m) = entry {
                        if m.page == page {
                            return Err(AddressSpaceError::AlreadyMapped);
                        }
                    }
                }

                // 空きスロットに登録
                for entry in self.mappings.iter_mut() {
                    if entry.is_none() {
                        *entry = Some(Mapping { page, frame, flags });
                        return Ok(());
                    }
                }

                Err(AddressSpaceError::CapacityExceeded)
            }
            MemAction::Unmap { page } => {
                // 対応するマッピングを探して削除
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

    /// 現在のマッピング数（デバッグ用）。
    pub fn mapping_count(&self) -> usize {
        self.mappings.iter().filter(|m| m.is_some()).count()
    }

    /// 各マッピングに対してクロージャを適用する（ダンプ等に使用）。
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
