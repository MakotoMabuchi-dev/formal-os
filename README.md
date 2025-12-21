# formal-os

A **pre-formal-verification microkernel prototype** written in Rust.  
Boots via `bootloader` v0.9 + `bootimage`, designed to run cleanly under QEMU x86_64.

This project intentionally keeps the kernel **small, layered, and analyzable**.
The long-term goal is to evolve this into a **formally verifiable operating system**
in the spirit of seL4-like designs, while remaining practical to experiment with.

---

## Current Status (2025-12)

This repository represents a **working prototype** with:

- Real x86_64 paging enabled (with safety guards)
- Multiple logical address spaces with independent PML4 roots
- A formal-friendly kernel state machine
- Explicit fail-stop behavior for invariant violations
- Instrumented execution traces suitable for later formal modeling

The system is *not* yet a user-mode OS, but a **verification-oriented kernel laboratory**.

---

## Boot Infrastructure

- Uses `bootloader` v0.9 and `bootimage`.
- Runs in 64-bit long mode under QEMU x86_64.
- Serial + VGA logging for early boot and kernel diagnostics.
- Custom target specification: `x86_64-formal-os-local.json`.

---

## Kernel Core (Formal-Friendly Design)

The kernel is structured around a **pure state machine**:

- `next_activity_and_action()`
  - A *pure* transition function (no side effects).
  - Computes the next `KernelActivity` and required `KernelAction`.

- `tick()`
  - Executes side effects derived from the above transition.
  - Updates kernel state.
  - Emits abstract events.
  - Checks invariants after every tick.

This separation is deliberate and mirrors patterns used in
formally verified systems.

---

## Task Scheduling

- Task states:
  - `Ready`
  - `Running`
  - `Blocked`
- Priority-based scheduling.
- Fixed quantum (time slice).
- Synthetic blocking (`Sleep`) for demonstration.
- Ready / Wait queues:
  - Implemented as **fixed-size arrays + length**
  - Ordering is intentionally abstracted (verification-friendly).

---

## Memory & Address Spaces

### Architecture-independent (spec layer)

- `MemAction` represents abstract memory operations:
  - `Map { page, frame, flags }`
  - `Unmap { page }`
- Each task owns a logical `AddressSpace`:
  - `(VirtPage ↔ PhysFrame, flags)` mappings
- Safety rules enforced:
  - No double-map
  - No unmap of unmapped pages
  - Bounded capacity
- Violations cause **fail-stop panic** by design.

### Hardware-backed paging (x86_64)

- Uses `OffsetPageTable` for real page-table manipulation.
- Kernel address space (Task0):
  - Executes real map/unmap
  - Validates with actual memory read/write tests.
- User address spaces:
  - Each has its **own PML4 root frame**
  - Kernel high-half entries are copied into user PML4
  - Low-half remains empty for isolation

#### CR3 Switching Safety

Because the kernel currently runs in the **low-half**, CR3 switching is gated:

- `configure_cr3_switch_safety(code_addr, stack_addr)`
- Real CR3 writes are enabled *only if*:
  - Code and stack are both in `KERNEL_SPACE_START..`
- When unsafe:
  - CR3 switching is disabled
  - Page tables are still modified *off-CR3*
  - Results are verified using page-table translation checks

This allows progress without risking undefined execution.

---

## IPC (Prototype)

- Endpoint-based synchronous IPC:
  - `send`
  - `recv`
  - `reply`
- Tasks block with explicit `BlockedReason`:
  - `IpcRecv`
  - `IpcSend`
  - `IpcReply`
- IPC behavior is fully logged and invariant-checked.
- Invalid IPC (via `evil_ipc` feature) is tolerated and must **not panic**.

---

## Evil Tests (Negative Testing)

Feature-gated tests intentionally violate invariants to confirm fail-stop behavior:

- `evil_double_map`
  - Attempts to map the same virtual page twice.
  - Must panic with `AlreadyMapped`.

- `evil_unmap_not_mapped`
  - Attempts to unmap a non-mapped page.
  - Must panic with `NotMapped`.

- `evil_ipc`
  - Issues IPC calls with invalid endpoints.
  - Must *not* panic.

These tests are critical for validating kernel invariants.

---

## Event Log & Observability

All significant logical events are recorded:

- Scheduling
- State transitions
- Memory actions
- IPC operations
- Timer updates

Example:

```
=== KernelState Event Log Dump ===
...
=== AddressSpace Dump (per task) ===
...
=== Endpoint Dump ===
...
```

This trace-oriented design is intended for later translation
into formal models (TLA+, Coq, etc.).

---

## Build & Run

### Requirements

```
rustup component add rust-src
cargo install bootimage
```

### Build

- default (no features)
  - `./scripts/build-kernel.sh`

- IPC trace
  - `FEATURES="ipc_trace_paths" ./scripts/build-kernel.sh`

- IPC demo (reproducible) + trace
  - `FEATURES="ipc_demo_single_slow ipc_trace_paths" ./scripts/build-kernel.sh`

- PF demo
  - `FEATURES="pf_demo" ./scripts/build-kernel.sh`

- quick local checks
  - `./scripts/ci-check.sh`


### Run (QEMU)

```
qemu-system-x86_64 \
  -drive format=raw,file=target/x86_64-formal-os-local/debug/bootimage-kernel.bin \
  -m 512M \
  -serial stdio
```

Or via scripts:

```
scripts/build-kernel.sh
scripts/run-qemu-debug.sh
```

---

## Roadmap

### 1. Clarify Address Space Separation
- Use distinct demo virtual pages per task.
- Continue translation-based verification without CR3 switching.

### 2. High-half Kernel Placement
- Relocate kernel code and stack to high-half.
- Enable *real* CR3 switching safely.

### 3. User-mode Execution
- Enter ring3 for user tasks.
- Minimal syscall ABI.

### 4. Capability-oriented IPC
- Endpoint rights and access control.
- Prepare for formal modeling.

### 5. Formal Specification
- TLA+ model of `KernelState` and `tick()`.
- Invariant and liveness checking.

---

## License

MIT
