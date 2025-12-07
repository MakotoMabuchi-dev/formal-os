#!/usr/bin/env bash
set -euo pipefail

# リポジトリルートに移動
cd "$(dirname "$0")/.."

TARGET_JSON="x86_64-formal-os-local.json"

echo "[*] building kernel bootimage (target = ${TARGET_JSON})..."
cargo bootimage -p kernel --target "${TARGET_JSON}"

echo "[*] build finished."
