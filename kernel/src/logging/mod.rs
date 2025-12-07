// src/logging/mod.rs
//
// ログ出力基盤。
// - VGA テキストモード + シリアル(COM1) の両方に出力する。
// - info / error / info_u64 を通じて、画面・ログファイル両方向で観測可能。
//   （QEMU では `-serial stdio` によりシリアル出力がホストに流れ、
//     run-qemu-debug.sh で tee によって logs/*.log に保存される）

mod vga;
mod serial;

pub fn init() {
    vga::init();
    serial::init();
}

/// 情報ログ（文字列）
pub fn info(msg: &str) {
    vga::write_line("[INFO] ");
    vga::write_line(msg);

    serial::write_line("[INFO] ");
    serial::write_line(msg);
}

/// エラーログ（文字列）
pub fn error(msg: &str) {
    vga::write_line("[ERROR] ");
    vga::write_line(msg);

    serial::write_line("[ERROR] ");
    serial::write_line(msg);
}

/// 情報ログ（整数）
///
/// - ラベルと値をそれぞれ 1 行ずつ表示する。
///   例: info_u64(" tick_count", 3) の場合
///     [INFO]
///      tick_count
///     [INFO]
///      3
pub fn info_u64(label: &str, value: u64) {
    // ラベル行
    vga::write_line("[INFO] ");
    vga::write_line(label);

    serial::write_line("[INFO] ");
    serial::write_line(label);

    // 値行
    let mut buf = [0u8; 21]; // u64 は最大 20 桁 + 余裕1
    let s = u64_to_decimal(value, &mut buf);

    vga::write_line("[INFO] ");
    vga::write_line(s);

    serial::write_line("[INFO] ");
    serial::write_line(s);
}

/// u64 を 10 進数の ASCII 文字列に変換する。
///
/// - buf は一時的な作業バッファ（呼び出し元のスタックに置かれる）
/// - 返り値は buf の一部を &str として見たもの（呼び出し元のスコープ内でのみ有効）
fn u64_to_decimal(mut value: u64, buf: &mut [u8; 21]) -> &str {
    // 0 は特別扱い
    if value == 0 {
        let last = buf.len() - 1;
        buf[last] = b'0';
        return unsafe { core::str::from_utf8_unchecked(&buf[last..]) };
    }

    // 下位桁から逆順に詰めていく
    let mut i = buf.len();
    while value > 0 {
        let digit = (value % 10) as u8;
        i -= 1;
        buf[i] = b'0' + digit;
        value /= 10;
    }

    unsafe { core::str::from_utf8_unchecked(&buf[i..]) }
}
