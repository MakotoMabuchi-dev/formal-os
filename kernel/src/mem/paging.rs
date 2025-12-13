// kernel/src/mem/paging.rs
//
// 役割:
// - ページ単位の抽象操作（Map/Unmap）と属性フラグを定義する。
// - arch 依存のページテーブル操作は arch::paging 側で行う。
// 設計方針:
// - kernel 側は MemAction を発行するだけにして、unsafe/実処理は arch に閉じ込める。

use crate::mem::addr::{PhysFrame, VirtPage};

bitflags::bitflags! {
    /// ページ属性（まだ最低限）
    ///
    /// - PRESENT: ページが有効
    /// - WRITABLE: 書き込み可能
    /// - USER: ユーザ空間からアクセス可能
    /// - NO_EXEC: 実行禁止（NX bit 相当）
    #[derive(Clone, Copy, Debug)]
    pub struct PageFlags: u64 {
        const PRESENT  = 1 << 0;
        const WRITABLE = 1 << 1;
        const USER     = 1 << 2;
        const NO_EXEC  = 1 << 63;
    }
}

/// ページ単位のメモリ操作を表現する抽象イベント。
#[derive(Clone, Copy, Debug)]
pub enum MemAction {
    Map {
        page: VirtPage,
        frame: PhysFrame,
        flags: PageFlags,
    },
    Unmap {
        page: VirtPage,
    },
}

impl MemAction {
    /// Map を作るヘルパ（呼び出し側の見通しを良くする）
    pub const fn map(page: VirtPage, frame: PhysFrame, flags: PageFlags) -> Self {
        MemAction::Map { page, frame, flags }
    }

    /// Unmap を作るヘルパ
    pub const fn unmap(page: VirtPage) -> Self {
        MemAction::Unmap { page }
    }
}
