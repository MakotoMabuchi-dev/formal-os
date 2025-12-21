#!/usr/bin/env bash
# scripts/ci-check.sh
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET_JSON="x86_64-formal-os-local.json"

echo "[ci] 1) build: no features"
FEATURES="" ./scripts/build-kernel.sh >/dev/null

echo "[ci] 2) build: trace only (ipc_trace_paths)"
FEATURES="ipc_trace_paths" ./scripts/build-kernel.sh >/dev/null

echo "[ci] 3) build: demo + trace (ipc_demo_single_slow ipc_trace_paths)"
FEATURES="ipc_demo_single_slow ipc_trace_paths" ./scripts/build-kernel.sh >/dev/null

echo "[ci] 4) build: pf_demo"
FEATURES="pf_demo" ./scripts/build-kernel.sh >/dev/null

echo "[ci] OK"
