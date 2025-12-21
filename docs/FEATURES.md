# FEATURES（運用ルール）

このプロジェクトは **「通常は evil を一切入れない」** を基本方針とする。
実験用の挙動変更は feature で明示し、再現性・観測性を担保する。

## 1) feature 分類（今後この枠組みを崩さない）

### product（通常運用）
- 目的: 通常動作。安全側に倒す。
- ルール:
    - `default = []` を維持
    - product を壊す可能性がある変更は入れない

### demo（再現用シナリオ）
- 目的: 仕様/不具合の再現や観察のために、挙動を意図的に固定する。
- ルール:
    - 「ログが取りやすい」「再現性が高い」方向に制約してよい
    - 本番品質を意図していないが、状態破壊は避ける

### trace（観測）
- 目的: 観測性（ログ）を追加する。
- ルール:
    - **挙動を変えない（ログのみ）**
    - 例外: 計測のための最小限のカウンタ更新は可（意味が変わらない範囲）

### evil（破壊的テスト）
- 目的: 前提崩れ・不正状態を意図的に作り、fail-safe を確認する。
- ルール:
    - panic ではなくログ＋returnで壊し方を固定（状態破壊を避ける）
    - `evil_*` を通常運用で有効化しない

## 2) 現在の feature 一覧（正規）

### evil（破壊的テスト）
- `evil_double_map`
- `evil_unmap_not_mapped`
- `evil_ipc`

### demo（再現）
- `pf_demo`
- `ipc_demo_single_slow`
    - 目的: IPC の slow send を 1 回に固定し、以後はノイズの少ない状態で観測する

### trace（観測）
- `ipc_trace_paths`
    - 目的: send/recv/reply が fast/slow のどちらで処理されたかを必ずログに出す

## 3) 推奨ビルド（公式）

### 通常ビルド（feature なし）
- `./scripts/build-kernel.sh`

### IPC 観測（trace のみ）
- `FEATURES="ipc_trace_paths" ./scripts/build-kernel.sh`

### IPC 再現（demo + trace）
- `FEATURES="ipc_demo_single_slow ipc_trace_paths" ./scripts/build-kernel.sh`

### PF デモ
- `FEATURES="pf_demo" ./scripts/build-kernel.sh`

## 4) 禁止事項
- product（通常運用）に、trace/demo/evil の挙動を暗黙に混入させない
- feature の意味を曖昧にしない（名前と実態を一致させる）
- trace が挙動を変える設計にしない
