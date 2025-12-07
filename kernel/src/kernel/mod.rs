// src/kernel/mod.rs
//
// KernelState に「活動状態（KernelActivity）」を追加し、
// tick() が状態遷移を起こす “小さなカーネル状態機械” を構築する。

use bootloader::BootInfo;
use crate::{arch, logging};
use crate::mm::PhysicalMemoryManager;

/// カーネルが現在行っている“活動”
///
/// これは将来、スケジューラ・スレッド・割り込みハンドラへ発展するための
/// ― 最も小さい OS 状態機械の核 ― になる。
#[derive(Clone, Copy)]
pub enum KernelActivity {
    Idle,
    AllocatingFrame,
}

/// カーネル全体の状態。
pub struct KernelState {
    phys_mem: PhysicalMemoryManager,
    tick_count: u64,
    should_halt: bool,
    activity: KernelActivity,     // ★ 新しい状態変数
}

impl KernelState {
    pub fn new(boot_info: &'static BootInfo) -> Self {
        let phys_mem = PhysicalMemoryManager::new(boot_info);
        KernelState {
            phys_mem,
            tick_count: 0,
            should_halt: false,
            activity: KernelActivity::AllocatingFrame, // ★ 最初は「フレーム割り当て状態」から開始
        }
    }

    pub fn bootstrap(&mut self) {
        logging::info("KernelState::bootstrap()");

        for _ in 0..5 {
            match self.phys_mem.allocate_frame() {
                Some(_) => logging::info("allocated usable frame (bootstrap)"),
                None => {
                    logging::error("no more frames in bootstrap");
                    self.should_halt = true;
                    break;
                }
            }
        }
    }

    /// OS が tick ごとに状態遷移を行う。
    ///
    /// activity の内容によって処理が変わり、
    /// tick の最後に activity を別の状態へ変更する。
    pub fn tick(&mut self) {
        if self.should_halt {
            return;
        }

        self.tick_count += 1;

        logging::info("KernelState::tick()");
        // ★ tick 回数を数値として表示
        logging::info_u64(" tick_count", self.tick_count);

        match self.activity {
            KernelActivity::Idle => {
                logging::info(" activity = Idle (nothing to do)");
                self.activity = KernelActivity::AllocatingFrame;
            }

            KernelActivity::AllocatingFrame => {
                logging::info(" activity = AllocatingFrame (allocating)");

                match self.phys_mem.allocate_frame() {
                    Some(_) => logging::info(" allocated usable frame (tick)"),
                    None => {
                        logging::error(" no more usable frames; halting later");
                        self.should_halt = true;
                    }
                }

                self.activity = KernelActivity::Idle;
            }
        }
    }

    pub fn should_halt(&self) -> bool {
        self.should_halt
    }
}

pub fn start(boot_info: &'static BootInfo) {
    logging::info("kernel::start()");

    let mut kstate = KernelState::new(boot_info);

    kstate.bootstrap();

    let max_ticks = 20;
    for _ in 0..max_ticks {
        if kstate.should_halt() {
            logging::info("KernelState requested halt; stop ticking");
            break;
        }
        kstate.tick();
    }

    arch::halt_loop();
}
