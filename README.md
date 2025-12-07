# formal-os

A pre-formal-verification microkernel written in Rust.  
Boots via bootloader v0.9 + bootimage on QEMU x86_64.

This project intentionally keeps the kernel small, structured, and strictly layered
so that it can evolve into a formally verifiable OS design.

---

## Current Features (as of 2025-02)

### ✔ Boot Infrastructure
- Uses `bootloader` v0.9 and `bootimage` to build a bootable image.
- Runs on QEMU x86_64 in 64-bit long mode.
- VGA text-mode output with a minimal logging backend.

### ✔ Kernel Architecture Overview

```
KernelState
 ├─ phys_mem              : PhysicalMemoryManager
 ├─ tick_count            : OS logical time
 ├─ time_ticks            : simplified timer counter
 ├─ activity              : KernelActivity (Idle / UpdatingTimer / AllocatingFrame)
 ├─ tasks[]               : simple Task objects (TaskId + TaskState)
 ├─ current_task          : index of currently Running task
 ├─ ready_queue[]         : ring buffer of Ready tasks
 ├─ event_log[]           : abstract execution trace
```

### ✔ Pure State Machine Design (Formal-Friendly)

- `next_activity_and_action()`  
  → **pure function** representing the kernel's state transition  
  (no side effects)

- `tick()`  
  → executes side effects *based on* the next transition  
  (timer update, frame allocation, scheduling)

### ✔ Task Scheduling

- Tasks have states: **Ready / Running**
- A **ReadyQueue** (ring buffer) implements a minimal round-robin scheduler
- Each tick performs:
    1. pure kernel activity transition
    2. timer/frame side effects
    3. **task scheduling:** Running → ReadyQueue → Running

### ✔ Abstract Event Log

The kernel records "logical events" (not tied to VGA output):

- `TickStarted(n)`
- `TimerUpdated(n)`
- `FrameAllocated`
- `TaskSwitched(TaskId)`
- `TaskStateChanged(TaskId, State)`
- `ReadyQueued(TaskId)`
- `ReadyDequeued(TaskId)`

This trace is later dumped with:

```
=== KernelState Event Log Dump ===
EVENT: TickStarted
EVENT: TaskSwitched
…
```

This structure is intentionally designed for future **formal verification** (e.g., TLA+).

---

## Build & Run

### Requirements

```
rustup component add rust-src
cargo install bootimage
```

### Build (kernel + bootloader)

```
cargo bootimage -p kernel --target x86_64-formal-os-local.json
```

Boot image will be created at:

```
target/x86_64-formal-os-local/debug/bootimage-kernel.bin
```

### Run on QEMU

```
qemu-system-x86_64   -drive format=raw,file=target/x86_64-formal-os-local/debug/bootimage-kernel.bin   -m 512M   -serial stdio
```

### Scripts

```
scripts/build-kernel.sh
scripts/run-qemu-debug.sh
```

---

## Roadmap (Planned)

- Add **runtime_ticks** per task (toward fair scheduling)
- Add **Blocked** state → enable wait queues
- Introduce **SystemCallHandling** for user/task interaction
- Define **memory mapping abstraction** (Mapper trait)
- Export abstract event log to host via serial

---

## License

MIT
