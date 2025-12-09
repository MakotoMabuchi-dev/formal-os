# formal-os

A pre-formal-verification microkernel written in Rust.  
Boots via `bootloader` v0.9 + `bootimage`, designed to run cleanly under QEMU x86_64.

The project intentionally keeps the kernel small, layered, and analyzable.  
The long-term goal is to evolve this into a formally verifiable operating system.

---

## Current Features (as of 2025-02)

### Boot Infrastructure
- Uses `bootloader` v0.9 and `bootimage` to construct a bootable image.
- Runs on QEMU x86_64 in 64-bit long mode.
- Provides a minimal VGA/serial logging backend.

---

## Kernel Architecture Overview

```
KernelState
 ├─ phys_mem                : PhysicalMemoryManager
 ├─ activity                : KernelActivity (Idle / UpdatingTimer / AllocatingFrame / MappingDemoPage)
 ├─ tick_count              : logical time
 ├─ time_ticks              : simplified timer
 ├─ tasks[]                 : Task objects (TaskId / TaskState / priority)
 ├─ current_task            : index of running task
 ├─ ready_queue[]           : Ready tasks (ring buffer)
 ├─ wait_queue[]            : Blocked tasks (ring buffer)
 ├─ address_spaces[]        : logical AddressSpace per task
 ├─ mem_demo_mapped[]       : demo flag for each task's mapping state
 ├─ event_log[]             : abstract execution trace
```

---

## State Machine Design (Formal-Friendly)

The kernel core follows a strictly separated structure:

- `next_activity_and_action()`  
  Pure transition function with **no side effects**.  
  Defines the next `KernelActivity` and required `KernelAction`.

- `tick()`  
  Executes all side effects derived from the above transition (timer update, frame allocation, memory actions, scheduling), and records abstract events.

This separation mirrors techniques used in formally verified kernels such as seL4.

---

## Task Scheduling

- Task states: `Ready`, `Running`, `Blocked`.
- Round-robin scheduling with per-task priorities.
- Quantum accounting per task (time slice).
- Periodic synthetic blocking/waking for demonstration.
- ReadyQueue and WaitQueue implemented as fixed-size ring buffers.

---

## Memory Model (Abstract AddressSpace)

The kernel implements a clean, architecture-independent memory model:

- `MemAction` represents abstract mapping operations:  
  `Map { page, frame, flags }` / `Unmap { page }`
- Each task has its own `AddressSpace`, storing logical mappings  
  (`VirtPage ↔ PhysFrame` + flags).
- Integrity checks prevent double-mapping or unmapping unmapped pages.
- `MemActionApplied { task, action }` is recorded in the event log.
- All AddressSpace contents can be dumped at shutdown.

This model provides a verifiable “specification layer” decoupled from hardware page tables.

---

## Event Log

All logical events generated during execution are recorded:

- `TickStarted(n)`
- `TimerUpdated(n)`
- `FrameAllocated`
- `TaskSwitched(TaskId)`
- `TaskStateChanged(TaskId, State)`
- `ReadyQueued(TaskId)`
- `ReadyDequeued(TaskId)`
- `WaitQueued(TaskId)`
- `WaitDequeued(TaskId)`
- `MemActionApplied { task, action }`

The resulting trace can be inspected via:

```
=== KernelState Event Log Dump ===
...
=== AddressSpace Dump (per task) ===
...
```

This design enables later modeling with TLA+, Coq, or other verification tools.

---

## Build & Run

### Requirements

```
rustup component add rust-src
cargo install bootimage
```

### Build

```
cargo bootimage -p kernel --target x86_64-formal-os-local.json
```

Generates the image:

```
target/x86_64-formal-os-local/debug/bootimage-kernel.bin
```

### Run under QEMU

```
qemu-system-x86_64 \
  -drive format=raw,file=target/x86_64-formal-os-local/debug/bootimage-kernel.bin \
  -m 512M \
  -serial stdio
```

Or use the provided scripts:

```
scripts/build-kernel.sh
scripts/run-qemu-debug.sh
```

---

## Roadmap

- Implement hardware-backed page table mapping (`OffsetPageTable`).
- Provide real virtual memory per task (CR3 switching).
- Introduce user-mode execution and system call interface.
- Serialize event logs externally for formal analysis.
- Develop formal specifications (TLA+ model of kernel transitions).
- Expand AddressSpace into a full capability- or rights-based memory system.

---

## License

MIT
