#!/usr/bin/env bash
# scripts/ci-check.sh
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET_JSON="x86_64-formal-os-local.json"
LOG_DIR="logs"
mkdir -p "${LOG_DIR}"

# 0: build only / 1: build + runtime smoke
CI_RUN="${CI_RUN:-1}"

# ------------------------------------------------------------
# Helper: build only
# ------------------------------------------------------------
build_only() {
  local features="$1"
  local label="$2"

  echo "[ci] build: ${label}"
  FEATURES="${features}" ./scripts/build-kernel.sh >/dev/null
}

# ------------------------------------------------------------
# Helper: run command with timeout (portable)
# - prefers gtimeout (coreutils)
# - falls back to python timeout (macOS OK)
# ------------------------------------------------------------
run_with_timeout() {
  local timeout_sec="$1"
  shift
  if command -v gtimeout >/dev/null 2>&1; then
    gtimeout "${timeout_sec}" "$@"
    return $?
  fi

  # macOS standard python is available in most environments; try python3 then python
  if command -v python3 >/dev/null 2>&1; then
    python3 - <<'PY' "$timeout_sec" "$@"
import subprocess, sys, time

timeout = int(sys.argv[1])
cmd = sys.argv[2:]

p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
start = time.time()
try:
    while True:
        if p.poll() is not None:
            sys.exit(p.returncode)
        if time.time() - start > timeout:
            try:
                p.terminate()
                time.sleep(0.5)
            except Exception:
                pass
            try:
                if p.poll() is None:
                    p.kill()
            except Exception:
                pass
            sys.exit(124)
        # drain a bit to avoid pipe fill
        try:
            _ = p.stdout.readline()
        except Exception:
            pass
except KeyboardInterrupt:
    try:
        p.kill()
    except Exception:
        pass
    sys.exit(130)
PY
    return $?
  fi

  if command -v python >/dev/null 2>&1; then
    python - <<'PY' "$timeout_sec" "$@"
import subprocess, sys, time

timeout = int(sys.argv[1])
cmd = sys.argv[2:]

p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
start = time.time()
try:
    while True:
        if p.poll() is not None:
            sys.exit(p.returncode)
        if time.time() - start > timeout:
            try:
                p.terminate()
                time.sleep(0.5)
            except Exception:
                pass
            try:
                if p.poll() is None:
                    p.kill()
            except Exception:
                pass
            sys.exit(124)
        try:
            _ = p.stdout.readline()
        except Exception:
            pass
except KeyboardInterrupt:
    try:
        p.kill()
    except Exception:
        pass
    sys.exit(130)
PY
    return $?
  fi

  echo "[ci] ERROR: neither gtimeout nor python is available for timeout handling"
  return 127
}

# ------------------------------------------------------------
# Helper: run qemu and assert "no crash"
# ------------------------------------------------------------
run_qemu_assert() {
  local features="$1"
  local label="$2"
  local timeout_sec="${3:-15}"

  local ts
  ts="$(date +'%Y%m%d-%H%M%S')"
  local log_file="${LOG_DIR}/ci_${ts}_${label}.log"

  echo "[ci] run: ${label} (features='${features}', timeout=${timeout_sec}s)"
  echo "[ci] log: ${log_file}"

  set +e
  FEATURES="${features}" run_with_timeout "${timeout_sec}" ./scripts/run-qemu-debug.sh > "${log_file}" 2>&1
  local rc=$?
  set -e

  # timeout 終了は許容（124）
  if [[ "${rc}" -ne 0 && "${rc}" -ne 124 && "${rc}" -ne 137 ]]; then
    echo "[ci] ERROR: qemu returned non-zero (rc=${rc})"
    tail -n 80 "${log_file}"
    exit 1
  fi

  # --- NG パターン検出 ---
  if grep -qE "INVARIANT VIOLATION" "${log_file}"; then
    echo "[ci] ERROR: invariant violation detected"
    grep -nE "INVARIANT VIOLATION" "${log_file}" | head -n 60
    exit 1
  fi

  if grep -qE "panic|PANIC|stack trace" "${log_file}"; then
    echo "[ci] ERROR: panic detected"
    grep -nE "panic|PANIC|stack trace" "${log_file}" | head -n 80
    exit 1
  fi

  if grep -qE "\[EXC\] #DF|\[EXC\] #GP|\[EXC\] #PF unguarded" "${log_file}"; then
    echo "[ci] ERROR: fatal exception detected"
    grep -nE "\[EXC\] #DF|\[EXC\] #GP|\[EXC\] #PF unguarded" "${log_file}" | head -n 80
    exit 1
  fi

  # --- 進捗チェック ---
  if ! grep -qE "KernelState::tick\(\)" "${log_file}"; then
    echo "[ci] ERROR: tick did not run (KernelState::tick not found)"
    tail -n 120 "${log_file}"
    exit 1
  fi

  # dump は timeout で切れることがあるので WARN 扱い
  if ! grep -qE "=== KernelState Event Log Dump ===" "${log_file}"; then
    echo "[ci] WARN: event dump not found (may be cut by timeout) - OK"
  fi

  echo "[ci] run: ${label} OK"
}

echo "[ci] 1) build matrix (fast)"
build_only "" "no-features"
build_only "ipc_trace_paths" "trace-only"
build_only "ipc_demo_single_slow ipc_trace_paths" "demo+trace"
build_only "pf_demo" "pf_demo"
build_only "endpoint_close_test" "endpoint_close_test"
build_only "dead_partner_test" "dead_partner_test"
build_only "evil_double_map" "evil_double_map"
build_only "evil_unmap_not_mapped" "evil_unmap_not_mapped"

echo "[ci] 2) runtime smoke (slow but high value)"
if [[ "${CI_RUN}" == "1" ]]; then
  run_qemu_assert "" "run_no_features" 12
  run_qemu_assert "ipc_demo_single_slow" "run_ipc_demo_single_slow" 12
  run_qemu_assert "pf_demo" "run_pf_demo" 12
  run_qemu_assert "dead_partner_test" "run_dead_partner_test" 12
  run_qemu_assert "endpoint_close_test" "run_endpoint_close_test" 12
  run_qemu_assert "evil_unmap_not_mapped" "run_evil_unmap_not_mapped" 12
  run_qemu_assert "evil_double_map" "run_evil_double_map" 12
else
  echo "[ci] runtime smoke skipped (CI_RUN=0)"
fi

echo "[ci] OK"
