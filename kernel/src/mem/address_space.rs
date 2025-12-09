// kernel/src/mem/address_space.rs
//
// 役割:
// - 仮想アドレス空間（どの仮想ページがどの物理フレームにマップされているか）を
//   純粋な Rust のデータ構造として管理する。
// - 「同じページを二重に Map していないか」「未マップのページを Unmap していないか」などを
//   チェックする。
// - KernelState からは、この AddressSpace を通じて論理的な整合性を確認する。
// やらないこと:
// - 実際のページテーブル（PTE）の書き換えは行わない（それは arch::paging が担当）。

use crate::mem::addr::{PhysFrame, VirtPage};
use crate::mem::paging::{MemAction, PageFlags};

/// 1つのアドレス空間の中で管理する最大マッピング数。
/// - デモ用なので 64 個に固定している（必要になったら増やせる）。
const MAX_MAPPINGS: usize = 64;

/// 1つの仮想ページに対するマッピング情報。
#[derive(Clone, Copy)]
pub struct Mapping {
    pub page: VirtPage,
    pub frame: PhysFrame,
    pub flags: PageFlags,
}

/// アドレス空間全体を表すシンプルな管理構造。
/// - 固定長配列＋Option で実装しているだけのシンプルなもの。
#[derive(Clone, Copy)]
pub struct AddressSpace {
    mappings: [Option<Mapping>; MAX_MAPPINGS],
}

/// AddressSpace 更新時のエラー種別。
#[derive(Clone, Copy, Debug)]
pub enum AddressSpaceError {
    /// すでに同じ page がマップされているのに、Map が来た。
    AlreadyMapped,
    /// その page はマップされていないのに、Unmap が来た。
    NotMapped,
    /// 配列がいっぱいで、これ以上マッピングを追加できない。
    CapacityExceeded,
}

impl AddressSpace {
    /// からのアドレス空間を作る。
    pub fn new() -> Self {
        AddressSpace {
            mappings: [None; MAX_MAPPINGS],
        }
    }

    /// 現在のアドレス空間に MemAction を適用する。
    ///
    /// - Map { page, frame, flags }:
    ///     - すでにその page が登録されていればエラー（AlreadyMapped）
    ///     - 空きスロットがあれば追加
    /// - Unmap { page }:
    ///     - 対応する page が見つかれば削除
    ///     - 見つからなければエラー（NotMapped）
    pub fn apply(&mut self, action: MemAction) -> Result<(), AddressSpaceError> {
        match action {
            MemAction::Map { page, frame, flags } => {
                // 1. すでに同じ page が存在していないかチェック
                for entry in self.mappings.iter() {
                    if let Some(m) = entry {
                        if m.page == page {
                            return Err(AddressSpaceError::AlreadyMapped);
                        }
                    }
                }

                // 2. 空きスロットに追加
                for entry in self.mappings.iter_mut() {
                    if entry.is_none() {
                        *entry = Some(Mapping { page, frame, flags });
                        return Ok(());
                    }
                }

                // 3. 空きが無かった
                Err(AddressSpaceError::CapacityExceeded)
            }
            MemAction::Unmap { page } => {
                // 対応する page を探して削除
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

    /// 現在のマッピング数を数える（デバッグ／統計用）。
    pub fn mapping_count(&self) -> usize {
        let mut count = 0;
        for entry in self.mappings.iter() {
            if entry.is_some() {
                count += 1;
            }
        }
        count
    }

    /// すべてのマッピングに対してコールバックを呼ぶ。
    ///
    /// - KernelState::dump_events() などから、ログ出力に使う。
    /// - AddressSpace 自体は logging を知らないままにしておき、
    ///   「列挙だけを提供する」ことで責務分離する。
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
