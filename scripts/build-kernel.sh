#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET_JSON="x86_64-formal-os-local.json"

FEATURES="${FEATURES:-}"

echo "[*] building kernel bootimage (target = ${TARGET_JSON})..."
if [ -n "${FEATURES}" ]; then
  echo "[*] features: ${FEATURES}"
  cargo bootimage -p kernel --target "${TARGET_JSON}" --features "${FEATURES}"
else
  cargo bootimage -p kernel --target "${TARGET_JSON}"
fi

echo "[*] build finished."
