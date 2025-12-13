// kernel/src/panic.rs
//
// no_std カーネル用 panic ハンドラ。
// - 挙動は「ログ出力 → CPU 停止」に固定する。
// - Rust バージョン差に引きずられないよう、message の文字列化は行わない。

use core::panic::PanicInfo;

use crate::{arch, logging};

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    logging::error("kernel panic");

    // 文字列化はここではしない（logging が整ったら拡張）
    let _ = info.message();

    if let Some(loc) = info.location() {
        logging::error("panic location");
        logging::error(loc.file());
        logging::info_u64("line", loc.line() as u64);
        logging::info_u64("column", loc.column() as u64);
    } else {
        logging::error("panic location: (unknown)");
    }

    arch::halt_loop()
}
