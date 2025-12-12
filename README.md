# formal-os

A **pre-formal-verification microkernel prototype** written in Rust.  
Boots via `bootloader` v0.9 + `bootimage`, designed to run cleanly under QEMU x86_64.

The project intentionally keeps the kernel small, layered, and analyzable.  
The long-term goal is to evolve this into a **formally verifiable operating system**.

---

## Current Features (as of 2025-12)

### Boot Infrastructure
- Uses `bootloader` v0.9 and `bootimage` to construct a bootable image.
- Runs on QEMU x86_64 in 64-bit long mode.
- Provides a minimal VGA/serial logging backend.

### Kernel Core (Formal-Friendly State Machine)
- `next_activity_and_action()` is a **pure transition function** (no side effects).
- `tick()` executes the derived side effects and records an abstract trace.
- Invariant checks (`debug_check_invariants`) are used to detect design violations early.

### Task Scheduling
- Task states: `Ready`, `Running`, `Blocked`.
- Priority-based selection with a quantum (time slice) per task.
- Synthetic blocking/waking for demonstration.
- Ready/Wait queues are implemented as **fixed-size arrays + length** (order intentionally abstracted).

### Memory / Address Spaces
#### Architecture-independent (spec layer)
- `MemAction` represents abstract mapping operations:  
  `Map { page, frame, flags }` / `Unmap { page }`
- Each task has its own logical `AddressSpace` storing mappings (`VirtPage ↔ PhysFrame` + flags).
- Safety checks prevent double-mapping, unmapping unmapped pages, and capacity overflow.
- AddressSpace contents can be dumped at shutdown.

#### Hardware-backed paging (x86_64)
- Uses `OffsetPageTable` for real page table operations when enabled.
- `Task0` (kernel AS) performs a real map/unmap demo and validates it with a memory read/write test.
- **User AddressSpaces now have real PML4 roots**:
  - Each user AS receives `root_page_frame = Some(...)`.
  - User PML4 is initialized by copying the kernel PML4 **high-half (entries 256..512)** while keeping low-half empty.
- Because the kernel is currently running in the low-half (no high-half kernel placement yet), **real CR3 switching is gated**:
  - `configure_cr3_switch_safety(code_addr, stack_addr)` enables real CR3 writes only if both addresses are in `KERNEL_SPACE_START..`.
  - When unsafe, switching remains logical/observational only.
- To keep progress while CR3 switching is gated, the kernel can still:
  - Apply mapping operations directly to **arbitrary PML4 roots** (without switching CR3).
  - Verify results using a page-table translation check (`translate: OK / NONE`).

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
 ├─ ready_queue[] + rq_len  : Ready tasks (order abstracted)
 ├─ wait_queue[]  + wq_len  : Blocked tasks (order abstracted)
 ├─ address_spaces[]        : AddressSpace per task (Kernel/User)
 ├─ mem_demo_mapped[]       : demo flag for each task's mapping state
 ├─ mem_demo_frame[]        : per-task demo frame (allocated once, then reused)
 ├─ event_log[]             : abstract execution trace
```

---

## State Machine Design (Formal-Friendly)

The kernel core follows a strictly separated structure:

- `next_activity_and_action()`  
  Pure transition function with **no side effects**.  
  Defines the next `KernelActivity` and required `KernelAction`.

- `tick()`  
  Executes all side effects derived from the above transition (timer update, frame allocation, memory actions, scheduling),
  and records abstract events.

This separation mirrors techniques used in kernels that emphasize formal reasoning.

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
- `MemActionApplied { task, address_space, action }`

Additionally, when applying mappings to a non-current root page table, the kernel can log:

- page-table translation results (`translate: OK` / `translate: NONE`) for verification without switching CR3.

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

## Roadmap (Near-term)

### 1) Make address-space differences clearer (still safe without CR3 switching)
- Use distinct demo virtual pages per task (e.g. Task1 vs Task2).
- Keep verifying via translation checks.

### 2) High-half kernel placement (enables real CR3 switching safely)
- Move kernel code/stack to the high-half layout defined in `mem/layout.rs`.
- Enable real CR3 switching and validate per-task user mappings with real memory access.

### 3) User-mode execution & syscalls
- Transition to ring3 user tasks.
- Provide a minimal syscall interface (yield, IPC primitives).

### 4) IPC + capability-oriented access control
- Introduce endpoints, blocking send/recv/reply.
- Evolve AddressSpace operations into a rights/capability model.

### 5) Formal specs
- Write a TLA+ model of `KernelState` and `tick()` transitions.
- Check invariants and key liveness properties.

---

## License

MIT
