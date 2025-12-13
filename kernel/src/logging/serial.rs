// kernel/src/logging/serial.rs
//
// COM1 (0x3F8) への最小限のシリアル出力。
// - init(): 115200bps, 8N1 に初期化
// - write_str(): 文字列を送信
// - write_line(): 文字列＋改行を送信

use core::fmt;
use x86_64::instructions::port::Port;

static mut SERIAL_INITIALIZED: bool = false;

pub fn init() {
    unsafe {
        if SERIAL_INITIALIZED {
            return;
        }

        let mut port_int_en = Port::<u8>::new(0x3F8 + 1);
        let mut port_line_ctrl = Port::<u8>::new(0x3F8 + 3);
        let mut port_div_low = Port::<u8>::new(0x3F8 + 0);
        let mut port_div_high = Port::<u8>::new(0x3F8 + 1);
        let mut port_fifo_ctrl = Port::<u8>::new(0x3F8 + 2);
        let mut port_modem_ctrl = Port::<u8>::new(0x3F8 + 4);

        port_int_en.write(0x00);

        port_line_ctrl.write(0x80);
        port_div_low.write(0x01);
        port_div_high.write(0x00);

        port_line_ctrl.write(0x03);
        port_fifo_ctrl.write(0xC7);
        port_modem_ctrl.write(0x0B);

        SERIAL_INITIALIZED = true;
    }
}

fn write_byte(byte: u8) {
    unsafe {
        let mut line_status = Port::<u8>::new(0x3F8 + 5);
        let mut data = Port::<u8>::new(0x3F8 + 0);

        while (line_status.read() & 0x20) == 0 {}

        data.write(byte);
    }
}

pub fn write_str(s: &str) {
    for b in s.bytes() {
        write_byte(b);
    }
}

pub fn write_line(s: &str) {
    write_str(s);
    write_str("\r\n");
}

pub fn write_prefixed_line(prefix: &str, msg: &str) {
    write_str(prefix);
    write_line(msg);
}

/// fmt::Write を実装しておくと、将来 format! 系にも使える
pub struct SerialWriter;

impl fmt::Write for SerialWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        write_str(s);
        Ok(())
    }
}
