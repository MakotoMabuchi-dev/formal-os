# IPC 仕様（同期 send/recv/reply）

本書は prototype の IPC（同期）を、フォーマル化の足場として「仕様」と「不変条件」を固定する。

## 0) スコープ
- 対象: `kernel/src/kernel/ipc.rs`
- 形式:
    - Endpoint は `recv_waiter`（単独）と `send_queue`、`reply_queue` を持つ
    - syscall 境界（`syscall.rs`）からのみ `ipc_*` を呼ぶ想定

## 1) 用語
- `send`: 送信（受信者が待っていれば即 deliver）
- `recv`: 受信（送信待ちがいれば即 deliver）
- `reply`: 返信（reply_waiter に deliver）
- `recv_waiter`: Endpoint 上で受信待ちしている 1 タスク（prototype）
- `send_queue`: 送信待ちタスクの集合（順序は抽象化）
- `reply_queue`: 返信待ちタスクの集合（blocked_reason に partner を保持）

## 2) TaskState と BlockedReason（概念）
TaskState:
- Running / Ready / Blocked / Dead

BlockedReason（IPC関連）:
- `IpcRecv { ep }`
- `IpcSend { ep }`
- `IpcReply { partner, ep }`

## 3) 仕様（高レベル）

### 3.1 recv(ep)
- Fastpath: `send_queue` に sender がいれば 1 件 deliver
    - deliver 後:
        - receiver: `last_msg = msg`
        - sender: `BlockedReason::IpcReply { partner = receiver_id, ep }`
        - sender は `reply_queue` に入る
- Slowpath: sender がいなければ
    - receiver を Blocked(IpcRecv) にして `recv_waiter = Some(receiver_idx)`

### 3.2 send(ep, msg)
- Fastpath: `recv_waiter` がいれば 1 件 deliver
    - deliver 後:
        - receiver: Ready に戻し、`last_msg = msg`
        - sender: Blocked(IpcReply { partner = receiver_id, ep }) にして `reply_queue` に入る
- Slowpath: `recv_waiter` がいなければ
    - sender: `pending_send_msg = msg`
    - sender を Blocked(IpcSend) にして `send_queue` に入る

### 3.3 reply(ep, msg)
- `reply_queue` から「partner = current receiver」を待っている sender を 1 件探す
- 見つかれば:
    - sender: `last_reply = msg` をセットして Ready に戻す
- 見つからなければ:
    - 何もしない（fail-safe）

## 4) 不変条件（invariants）
- `recv_waiter` は **同一 endpoint で同時に 1 件のみ**
- `send_queue` / `reply_queue` に同一 idx を重複投入しない
- `reply_queue` の要素 idx は、対応する task が
    - `BlockedReason::IpcReply { partner, ep }` を持つこと（不一致は fail-safe で reject）
- `Dead` な task に deliver しない
    - deliver 対象が Dead ならログを出し、deliver を中止する

## 5) 公平性について
- send_queue / reply_queue の取り出しは swap-remove で順序を抽象化する
- 公平性は後回し（将来の改善点）

## 6) 観測とカウンタ
- `ipc_send_fast/slow`, `ipc_recv_fast/slow`, `ipc_reply_delivered` をカウントする
- trace feature では fast/slow の分岐結果をログに出す（挙動は変えない）
