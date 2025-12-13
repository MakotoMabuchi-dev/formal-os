// src/logging/vga.rs
//
// VGA テキストモード(0xb8000)への最小限出力。
// - init(): Writer を初期化
// - write_line(): 文字列＋改行
//
// 目的:
// - まずは「画面に出る」ことを最優先にした簡易実装。
// - 高級なフォーマットや色付けは後回し。

use core::fmt::{self, Write};
use spin::Mutex;
use volatile::Volatile;

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

#[derive(Clone, Copy)]
#[repr(u8)]
enum Color {
    Black = 0x0,
    LightGray = 0x7,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ScreenChar {
    ascii_character: u8,
    color_code: u8,
}

#[repr(transparent)]
struct Buffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}

struct Writer {
    col: usize,
    color_code: u8,
    buffer: &'static mut Buffer,
}

impl Writer {
    fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            byte => {
                if self.col >= BUFFER_WIDTH {
                    self.new_line();
                }
                let row = BUFFER_HEIGHT - 1;
                let col = self.col;
                self.buffer.chars[row][col].write(ScreenChar {
                    ascii_character: byte,
                    color_code: self.color_code,
                });
                self.col += 1;
            }
        }
    }

    fn new_line(&mut self) {
        for row in 1..BUFFER_HEIGHT {
            for col in 0..BUFFER_WIDTH {
                let ch = self.buffer.chars[row][col].read();
                self.buffer.chars[row - 1][col].write(ch);
            }
        }
        self.clear_row(BUFFER_HEIGHT - 1);
        self.col = 0;
    }

    fn clear_row(&mut self, row: usize) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: self.color_code,
        };
        for col in 0..BUFFER_WIDTH {
            self.buffer.chars[row][col].write(blank);
        }
    }
}

impl Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            self.write_byte(b);
        }
        Ok(())
    }
}

static WRITER: Mutex<Option<Writer>> = Mutex::new(None);

pub fn init() {
    let writer = Writer {
        col: 0,
        color_code: (Color::LightGray as u8) | ((Color::Black as u8) << 4),
        buffer: unsafe { &mut *(0xb8000 as *mut Buffer) },
    };
    *WRITER.lock() = Some(writer);
}

pub fn write_line(s: &str) {
    if let Some(ref mut w) = *WRITER.lock() {
        let _ = w.write_str(s);
        let _ = w.write_str("\n");
    }
}
