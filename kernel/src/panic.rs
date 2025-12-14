// kernel/src/panic.rs
//
// no_std カーネル用 panic ハンドラ。
// - 挙動は「緊急出力（ロック無し） → CPU 停止」に固定する。
// - user CR3 中でも落ちないよう、VGA や logging を使わない。
// - 二重 panic は即停止（再入で #DF になりやすい）
// - Rust バージョン差に引きずられないよう、message の文字列化は行わない。
// - 重要: loc.file() は low-half 側に置かれる可能性があるため出力しない（再入防止）。

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

use crate::arch;

static PANIC_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

// ─────────────────────────────────────────────
// 緊急出力（ロック無し）
// - QEMU debugcon(0xE9) と COM1(0x3F8)
// ─────────────────────────────────────────────

fn emergency_write_byte(b: u8) {
    unsafe {
        // QEMU debugcon
        Port::<u8>::new(0xE9).write(b);

        // COM1
        let mut lsr = Port::<u8>::new(0x3FD);
        let mut data = Port::<u8>::new(0x3F8);
        for _ in 0..10_000 {
            if (lsr.read() & 0x20) != 0 {
                break;
            }
        }
        data.write(b);
    }
}

fn emergency_write_str(s: &str) {
    for b in s.bytes() {
        emergency_write_byte(b);
    }
}

fn emergency_write_hex_u64(v: u64) {
    emergency_write_str("0x");
    for i in (0..16).rev() {
        let n = ((v >> (i * 4)) & 0xF) as u8;
        let c = if n < 10 { b'0' + n } else { b'a' + (n - 10) };
        emergency_write_byte(c);
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    interrupts::disable();

    // 二重 panic は即停止（再入すると #DF になりやすい）
    if PANIC_IN_PROGRESS.swap(true, Ordering::AcqRel) {
        emergency_write_str("[PANIC] re-entered => halt\n");
        return arch::halt_loop();
    }

    emergency_write_str("[PANIC] kernel panic\n");

    // message の文字列化はしない（方針維持）
    let _ = info.message();

    // loc.file() は出さない（user CR3 中に low-half を読んで再入しやすい）
    if let Some(loc) = info.location() {
        emergency_write_str("[PANIC] location line=");
        emergency_write_hex_u64(loc.line() as u64);
        emergency_write_str(" col=");
        emergency_write_hex_u64(loc.column() as u64);
        emergency_write_str("\n");
    } else {
        emergency_write_str("[PANIC] location unknown\n");
    }

    arch::halt_loop()
}
