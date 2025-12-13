// src/mm/mod.rs
//
// 物理メモリ管理の入り口。
// - ブートローダから渡された BootInfo::memory_map をもとに、
//   「Usable」な物理フレームを順番に返すだけの最小アロケータ。
// - unsafe は BootInfo を受け取ってフレーム列挙器に変換する箇所に局所化する。
// - フォーマル検証の対象になりやすいよう、状態は構造体 + カウンタに閉じ込める。
//
// 追加の設計意図（性能）:
// - allocate_frame() を O(1) で動かす（毎回 nth で先頭から走査しない）
// - 低スペック環境でも “フレーム確保回数が増えるほど遅くなる” 事態を避ける

use bootloader::BootInfo;
use bootloader::bootinfo::{MemoryMap, MemoryRegionType};
use x86_64::structures::paging::PhysFrame;
use x86_64::PhysAddr;

/// カーネル側から見える「物理メモリマネージャ」。
/// - 外部 API はすべて safe にする。
/// - 内部で BootInfoFrameAllocator を使ってフレームを順番に返す。
pub struct PhysicalMemoryManager {
    inner: BootInfoFrameAllocator,
}

impl PhysicalMemoryManager {
    /// BootInfo から PhysicalMemoryManager を構築する。
    ///
    /// # 設計上の前提
    /// - カーネル全体で PhysicalMemoryManager は 1 インスタンスのみ保持すること。
    /// - 他のコードが BootInfo::memory_map に基づいて同じフレームを直接触らないこと
    ///   （ダブルアロケーションを防ぐため）。
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let memory_map: &'static MemoryMap = &boot_info.memory_map;

        // BootInfo 自体はブートローダ側の責務で正しく構築されている前提とし、
        // その「信頼境界との橋渡し」をこの unsafe に局所化する。
        let inner = unsafe { BootInfoFrameAllocator::new(memory_map) };

        PhysicalMemoryManager { inner }
    }

    /// 次の利用可能な物理フレームを 1 つ確保する。
    /// - 成功: Some(PhysFrame)
    /// - これ以上 usable なフレームが無い: None
    pub fn allocate_frame(&mut self) -> Option<PhysFrame> {
        self.inner.allocate_frame()
    }
}

/// BootInfo の MemoryMap から usable なフレームを順番に返すアロケータ。
///
/// - 状態: memory_map（不変入力）と「今どのUsable領域のどこまで配ったか」
/// - これはほぼ純粋ロジックなので、フォーマル検証の対象にしやすい。
///
/// 重要: O(n^2) になりがちな nth(skip) を避けるため、
/// 「次に返す物理アドレス」を保持して前進する。
struct BootInfoFrameAllocator {
    memory_map: &'static MemoryMap,

    // 次に見る memory_map のインデックス
    region_index: usize,

    // 現在の Usable region の [cur_addr, cur_end) を保持
    cur_addr: u64,
    cur_end: u64,

    // 有効な region を指しているか
    has_region: bool,
}

impl BootInfoFrameAllocator {
    /// MemoryMap からフレームアロケータを構築する。
    ///
    /// # Safety
    /// - 渡された memory_map がブートローダによって正しく初期化されていること。
    /// - memory_map 上で `Usable` とマークされているフレームが、他で
    ///   すでに利用中でないこと（本アロケータ以外から触らないこと）。
    pub unsafe fn new(memory_map: &'static MemoryMap) -> Self {
        let mut me = BootInfoFrameAllocator {
            memory_map,
            region_index: 0,
            cur_addr: 0,
            cur_end: 0,
            has_region: false,
        };

        // 最初の usable region をセット
        me.advance_to_next_usable_region();
        me
    }

    /// 4KiB アライン（切り上げ）
    #[inline]
    fn align_up_4k(x: u64) -> u64 {
        const MASK: u64 = 4096 - 1;
        (x + MASK) & !MASK
    }

    /// 次の usable region を探して、(cur_addr, cur_end) を更新する。
    fn advance_to_next_usable_region(&mut self) {
        self.has_region = false;

        while self.region_index < self.memory_map.len() {
            let region = &self.memory_map[self.region_index];
            self.region_index += 1;

            if region.region_type != MemoryRegionType::Usable {
                continue;
            }

            let start = region.range.start_addr();
            let end = region.range.end_addr();

            // start を 4KiB に切り上げ、end は [start, end) のまま扱う
            let cur = Self::align_up_4k(start);

            if cur >= end {
                continue;
            }

            self.cur_addr = cur;
            self.cur_end = end;
            self.has_region = true;
            return;
        }
    }

    /// 次の usable フレームを 1 つ返す。
    ///
    /// - 1回の呼び出しで O(1) を狙う（region を跨ぐときだけスキャンが走る）
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        loop {
            if !self.has_region {
                return None;
            }

            if self.cur_addr + 4096 <= self.cur_end {
                let addr = self.cur_addr;
                self.cur_addr += 4096;
                return Some(PhysFrame::containing_address(PhysAddr::new(addr)));
            }

            // region の終端に達したので、次の usable region を探す
            self.advance_to_next_usable_region();
        }
    }
}
