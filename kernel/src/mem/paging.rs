// kernel/src/mem/paging.rs

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
        /// ページが有効かどうか
        const PRESENT = 1 << 0;
        /// 書き込み可能かどうか
        const WRITABLE = 1 << 1;
        /// ユーザ空間からアクセス可能かどうか
        const USER = 1 << 2;
        /// 実行禁止（NX bit 相当）
        const NO_EXEC = 1 << 63;
    }
}

/// ページ単位のメモリ操作を表現する抽象イベント。
///
/// - Map: 「この仮想ページを、この物理フレームに、この属性でマップしたい」
/// - Unmap: 「この仮想ページのマッピングを解除したい」
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