// src/mm/mod.rs
//
// 物理メモリ管理の入り口。
// - ブートローダから渡された BootInfo::memory_map をもとに、
//   「Usable」な物理フレームを順番に返すだけの最小アロケータ。
// - unsafe は BootInfo を受け取ってフレーム列挙器に変換する箇所に局所化する。
// - フォーマル検証の対象になりやすいよう、状態は構造体 + カウンタに閉じ込める。

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
/// - 状態: `memory_map`（不変入力）と `next`（何番目まで使ったか）
/// - これはほぼ純粋ロジックなので、フォーマル検証の対象にしやすい。
struct BootInfoFrameAllocator {
    memory_map: &'static MemoryMap,
    next: usize,
}

impl BootInfoFrameAllocator {
    /// MemoryMap からフレームアロケータを構築する。
    ///
    /// # Safety
    /// - 渡された memory_map がブートローダによって正しく初期化されていること。
    /// - memory_map 上で `Usable` とマークされているフレームが、他で
    ///   すでに利用中でないこと（本アロケータ以外から触らないこと）。
    pub unsafe fn new(memory_map: &'static MemoryMap) -> Self {
        BootInfoFrameAllocator {
            memory_map,
            next: 0,
        }
    }

    /// memory_map 内の "Usable" な領域から、4KiB ごとの物理フレームを列挙する。
    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> {
        // 1. usable な領域だけを残す
        let regions = self.memory_map.iter();
        let usable_regions = regions
            .filter(|r| r.region_type == MemoryRegionType::Usable);

        // 2. 各領域を [start_addr, end_addr) のアドレス範囲に変換
        let addr_ranges = usable_regions
            .map(|r| r.range.start_addr()..r.range.end_addr());

        // 3. 4KiB ごとのフレーム先頭アドレスに分解
        let frame_addresses = addr_ranges.flat_map(|r| r.step_by(4096));

        // 4. 物理アドレス → PhysFrame 型に変換
        frame_addresses.map(|addr| {
            PhysFrame::containing_address(PhysAddr::new(addr))
        })
    }

    /// 次の usable フレームを 1 つ返す。
    ///
    /// - self.next の値だけ usable_frames() をスキップし、そのフレームを返す。
    /// - 返したら self.next を 1 増やすことで、同じフレームを二度返さない。
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let mut frames = self.usable_frames();
        let frame = frames.nth(self.next)?;
        self.next += 1;
        Some(frame)
    }
}
