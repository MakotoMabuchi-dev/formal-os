// kernel/src/logging/mod.rs
//
// ログ出力基盤。
// - VGA テキストモード + シリアル(COM1) の両方に出力する。
// - 解析・比較しやすいよう、可能な限り「1行=1レコード」に寄せる。

mod vga;
mod serial;

pub fn init() {
    vga::init();
    serial::init();
}

/// 情報ログ（文字列）
pub fn info(msg: &str) {
    vga::write_prefixed_line("[INFO] ", msg);
    serial::write_prefixed_line("[INFO] ", msg);
}

/// エラーログ（文字列）
pub fn error(msg: &str) {
    vga::write_prefixed_line("[ERROR] ", msg);
    serial::write_prefixed_line("[ERROR] ", msg);
}

/// 情報ログ（整数）
///
/// - 基本は key-value 形式で 1 行に出す。
pub fn info_u64(label: &str, value: u64) {
    info_kv(label, value);
}

/// key-value 形式の情報ログ（u64）
pub fn info_kv(key: &str, value: u64) {
    let mut buf = [0u8; 21]; // u64 は最大 20 桁
    let s = u64_to_decimal(value, &mut buf);

    if key.is_empty() {
        vga::write_str("[INFO] ");
        vga::write_line(s);

        serial::write_str("[INFO] ");
        serial::write_line(s);
        return;
    }

    vga::write_str("[INFO] ");
    vga::write_str(key);
    vga::write_str(" = ");
    vga::write_line(s);

    serial::write_str("[INFO] ");
    serial::write_str(key);
    serial::write_str(" = ");
    serial::write_line(s);
}

/// u64 を 10 進数の ASCII 文字列に変換する。
fn u64_to_decimal(mut value: u64, buf: &mut [u8; 21]) -> &str {
    if value == 0 {
        let last = buf.len() - 1;
        buf[last] = b'0';
        return unsafe { core::str::from_utf8_unchecked(&buf[last..]) };
    }

    let mut i = buf.len();
    while value > 0 {
        let digit = (value % 10) as u8;
        i -= 1;
        buf[i] = b'0' + digit;
        value /= 10;
    }

    unsafe { core::str::from_utf8_unchecked(&buf[i..]) }
}
