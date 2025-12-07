#!/usr/bin/env bash
set -euo pipefail

# リポジトリルートに移動
cd "$(dirname "$0")/.."

TARGET_JSON="x86_64-formal-os-local.json"
TARGET_DIR="target/x86_64-formal-os-local/debug"
BOOTIMAGE="${TARGET_DIR}/bootimage-kernel.bin"

echo "[*] building kernel bootimage (target = ${TARGET_JSON})..."
cargo bootimage -p kernel --target "${TARGET_JSON}"

if [ ! -f "${BOOTIMAGE}" ]; then
    echo "[-] bootimage not found: ${BOOTIMAGE}"
    exit 1
fi

echo "[*] launching QEMU with ${BOOTIMAGE}..."
qemu-system-x86_64 \
  -drive format=raw,file="${BOOTIMAGE}" \
  -m 512M \
  -serial stdio
