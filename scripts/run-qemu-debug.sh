#!/usr/bin/env bash
set -eu

KERNEL=/Users/makoto/RustroverProjects/formal-os/target/x86_64-unknown-none/debug/kernel
QEMU=~/src/qemu/build/qemu-system-x86_64-unsigned

"$QEMU" \
  -kernel "$KERNEL" \
  -m 512M \
  -serial mon:stdio \
  -display none \
  -no-reboot \
  -no-shutdown \
  -d guest_errors,int
