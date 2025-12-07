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

# ログディレクトリとファイル名
LOG_DIR="logs"
mkdir -p "${LOG_DIR}"
TS="$(date +'%Y%m%d-%H%M%S')"
LOG_FILE="${LOG_DIR}/qemu_${TS}.log"

echo "[*] launching QEMU with ${BOOTIMAGE}..."
echo "[*] logging output to ${LOG_FILE}"

# QEMU のシリアル出力をコンソールに表示しつつ、ログファイルにも保存
qemu-system-x86_64 \
  -drive format=raw,file="${BOOTIMAGE}" \
  -m 512M \
  -serial stdio | tee "${LOG_FILE}"
