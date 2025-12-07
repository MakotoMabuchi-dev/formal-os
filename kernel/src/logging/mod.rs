// src/logging/mod.rs
//
// VGA を使った最小ログ基盤。
// - info / error で文字列を表示
// - info_u64 で整数値を 10 進数として表示
//
// 数値変換は標準ライブラリに頼らず、自前で u64 → ASCII 変換を行う。

mod vga;

pub fn init() {
    vga::init();
}

/// 情報ログ（文字列）
pub fn info(msg: &str) {
    vga::write_line("[INFO] ");
    vga::write_line(msg);
}

/// エラーログ（文字列）
pub fn error(msg: &str) {
    vga::write_line("[ERROR] ");
    vga::write_line(msg);
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

    // 値行
    let mut buf = [0u8; 21]; // u64 は最大 20 桁 + 余裕1
    let s = u64_to_decimal(value, &mut buf);
    vga::write_line("[INFO] ");
    vga::write_line(s);
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
        // buf[last..] = "0"
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

    // buf[i..] に有効な数字列が入っている
    unsafe { core::str::from_utf8_unchecked(&buf[i..]) }
}
