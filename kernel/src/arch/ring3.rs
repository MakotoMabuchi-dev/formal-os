// kernel/src/arch/ring3.rs
//
// 役割:
// - ring0 から ring3 へ入る最小 glue（iretq）を提供する。
// - unsafe asm はここに閉じ込め、上位は「RIP/RSP/selector を渡すだけ」にする。
//
// やること:
// - user_cs/user_ss を使って iretq フレームを構築して ring3 に遷移
//
// やらないこと:
// - syscall/sysret の MSR 設定（まずは int 0x80 で MVP）
// - ユーザ空間のローダ（今は固定バイト列でOK）
//
// 設計方針:
// - MVP では ring3 へ入る時に IF=0 にして外部 IRQ による事故を避ける。
//   （int 0x80 は IF=0 でも動く）
// - 戻りは int 0x80 handler 側で停止する。

/// ring3 用の RFLAGS を作る。
/// - bit1 は常に 1（予約ビット）
/// - MVP では IF=0（外部割り込み無効）にして安定化
#[inline(always)]
fn rflags_user_mvp() -> u64 {
    1u64 << 1 // 0x2
}

/// ring3 へ遷移する（戻らない想定）。
///
/// - user_rip: ring3 の RIP
/// - user_rsp: ring3 の RSP（16byte align 推奨）
/// - user_cs:  user code selector（RPL=3 を含む）
/// - user_ss:  user data selector（RPL=3 を含む）
pub unsafe fn enter_user_mode_iretq(
    user_rip: u64,
    user_rsp: u64,
    user_cs: u16,
    user_ss: u16,
) -> ! {
    let rflags = rflags_user_mvp();

    core::arch::asm!(
    // iretq フレーム: SS, RSP, RFLAGS, CS, RIP
    "push {ss}",
    "push {rsp}",
    "push {rflags}",
    "push {cs}",
    "push {rip}",
    "iretq",
    ss = in(reg) (user_ss as u64),
    rsp = in(reg) user_rsp,
    rflags = in(reg) rflags,
    cs = in(reg) (user_cs as u64),
    rip = in(reg) user_rip,
    options(noreturn)
    );
}
