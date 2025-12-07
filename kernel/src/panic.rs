// src/panic.rs
//
// no_std カーネル用の panic ハンドラ。
// - フォーマル検証を意識し、挙動は「ログ出力 → CPU 停止」に固定する。
// - 具体的なログ内容は最小限にとどめる。

use core::panic::PanicInfo;
use crate::{arch, logging};

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    logging::error("kernel panic");

    // PanicInfo からメッセージが取れそうなら簡易に表示（失敗しても致命的にはならないように）
    if let Some(message) = info.payload().downcast_ref::<&str>() {
        logging::error(message);
    }

    arch::halt_loop()
}
