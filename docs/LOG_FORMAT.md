# LOG フォーマット（安定化）

本書は解析用に、ログ形式を「壊さない契約」として固定する。

## 1) 基本方針
- ログは **キー固定**、値は原則 **u64**
- 解析対象（IPC トレース）は **できるだけ 1 行**で完結させる
- trace feature は挙動を変えずに、観測のみを追加する

## 2) IPC トレース（ipc_trace_paths）

### 2.1 ベース行（常に出す）
[INFO] ipc_trace kind=ipc_send|ipc_recv|ipc_reply
[INFO] task_id_hash = <u64>
[INFO] ep_id_hash = <u64>
[INFO] msg = <u64>          # send/reply のみ（recv では省略可）

- task_id_hash / ep_id_hash は、ID を直接出せない/出したくない場合の安定化手段。
- msg は必要なときのみ（recv では msg が存在しないケースがある）。

### 2.2 経路行（paths：fast/slow/delivered）

send:
[INFO] ipc_trace_paths send=fast|slow

recv:
[INFO] ipc_trace_paths recv=fast|slow

reply:
[INFO] ipc_trace_paths reply=delivered

（将来、deliver 失敗理由も追加するなら reply=none などを足してよいが、既存は壊さない）

## 3) Event Log（KernelState Event Log Dump）
- これはデバッグ/説明用の高レベルログ
- IPC の意味理解は event log、性能/経路は ipc_trace を使う
