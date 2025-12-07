// src/arch/cpu.rs
// CPU 命令ラッパ。unsafe は最小限。

pub fn halt_loop() -> ! {
    loop {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }
}
