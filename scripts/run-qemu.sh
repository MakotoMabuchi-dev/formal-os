#!/usr/bin/env bash
set -eu

# プロジェクトのルートからの相対パス想定
KERNEL=target/x86_64-unknown-none/debug/kernel

# 自分でビルドした QEMU のパスに合わせる
QEMU=~/src/qemu/build/qemu-system-x86_64

# デバッグ用オプションも最初から入れておくと便利
# -s: gdb server を :1234 で待ち受け
# -S: 起動直後に CPU を停止（gdb でアタッチしてから run）
"$QEMU" \
  -kernel "$KERNEL" \
  -m 512M \
  -serial mon:stdio \
  -display none \
  -no-reboot \
  -s -S
