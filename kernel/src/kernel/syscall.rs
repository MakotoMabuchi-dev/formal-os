// kernel/src/kernel/syscall.rs
//
// syscall 境界（最小）
// - IPC syscall + mem_demo 用 PageMap/PageUnmap syscall
// - IPC reply は payload を返す（last_reply）
// - PageMap/PageUnmap は戻り値コードを返す（last_syscall_ret）
//
// トレース（feature で切替）
// - ipc_trace_syscall: syscall 境界の trace（kind/msg/task/ep を出す）
// - ipc_trace_paths:   “fast/slow/delivered/blocked” 等の経路（ipc.rs 側）
//
// 設計方針:
// - logging 側に新 API を要求しない（info / info_u64 のみで完結）
// - TaskId / EndpointId は newtype 前提でも OK（ここでは中身にアクセスするだけ）
// - no_std 前提で “ヒープ確保なし” で出せる形にする（固定文字列 + u64）
// - syscall の戻り値（mem 操作結果）と IPC reply を混線させない
//   * mem 系: last_syscall_ret
//   * IPC   : last_reply

use super::{EndpointId, KernelState, LogEvent};

use crate::mem::address_space::AddressSpaceKind;
use crate::mem::addr::VirtPage;
use crate::mem::paging::{MemAction, PageFlags};

// dead_partner_test を有効にしたときだけ kill_reason を使う
#[cfg(feature = "dead_partner_test")]
use super::TaskKillReason;

#[cfg(feature = "dead_partner_test")]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "dead_partner_test")]
static DEAD_PARTNER_TEST_FIRED: AtomicBool = AtomicBool::new(false);

// syscall 戻り値（最小）
// 0 を OK に固定しておくとデバッグが楽
const SYSCALL_OK: u64 = 0;
const SYSCALL_ERR_ALREADY_MAPPED: u64 = 1;
const SYSCALL_ERR_NOT_MAPPED: u64 = 2;
const SYSCALL_ERR_CAPACITY: u64 = 3;
const SYSCALL_ERR_ARCH_FAILED: u64 = 10;
const SYSCALL_ERR_BAD_ASPACE: u64 = 11;

#[derive(Clone, Copy)]
pub enum Syscall {
    // ---- IPC ----
    IpcRecv { ep: EndpointId },
    IpcSend { ep: EndpointId, msg: u64 },
    IpcReply { ep: EndpointId, msg: u64 },

    // ---- Mem demo 用（Step3: syscall 戻り値は last_syscall_ret）----
    PageMap { page: VirtPage, flags: PageFlags },
    PageUnmap { page: VirtPage },
}

impl KernelState {
    /// 現在タスクの pending_syscall があれば取り出して実行する。
    pub(super) fn handle_pending_syscall_if_any(&mut self) {
        let idx = self.current_task;
        if idx >= self.num_tasks {
            return;
        }

        if self.tasks[idx].state == super::TaskState::Dead {
            self.tasks[idx].pending_syscall = None;
            return;
        }

        let tid = self.tasks[idx].id;

        if let Some(sc) = self.tasks[idx].pending_syscall.take() {
            self.push_event(LogEvent::SyscallIssued { task: tid });
            self.handle_syscall(sc);
        }
    }

    fn handle_syscall(&mut self, sc: Syscall) {
        let task_index = self.current_task;
        if task_index >= self.num_tasks {
            return;
        }

        let tid = self.tasks[task_index].id;

        // ------------------------------------------------------------
        // Step1: Kernel task の IPC syscall は無視（fail-safe）
        // ------------------------------------------------------------
        {
            let as_idx = self.tasks[task_index].address_space_id.0;
            let is_kernel = as_idx < self.num_tasks
                && self.address_spaces[as_idx].kind == AddressSpaceKind::Kernel;

            if is_kernel {
                match sc {
                    Syscall::IpcRecv { ep }
                    | Syscall::IpcSend { ep, .. }
                    | Syscall::IpcReply { ep, .. } => {
                        crate::logging::error("syscall: kernel task IPC is forbidden (ignored at syscall boundary)");
                        crate::logging::info_u64("task_id", tid.0);
                        crate::logging::info_u64("ep_id", ep.0 as u64);
                        return;
                    }
                    _ => {}
                }
            }
        }

        // NOTE: 「Handled」は実行開始の観測点として使っている（現状のログ設計に合わせる）
        self.push_event(LogEvent::SyscallHandled { task: tid });

        match sc {
            // ------------------------------------------------------------
            // IPC
            // ------------------------------------------------------------
            Syscall::IpcRecv { ep } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Recv, tid, ep, None);

                self.ipc_recv(ep);

                // ------------------------------------------------------------
                // dead_partner_test:
                //   receiver（Task3: id=3）が recv した直後に 1 回だけ kill して、
                //   sender（reply waiter）が DEAD partner rescue されることを検証する。
                // ------------------------------------------------------------
                #[cfg(feature = "dead_partner_test")]
                {
                    if tid.0 == 3 && !DEAD_PARTNER_TEST_FIRED.swap(true, Ordering::SeqCst) {
                        crate::logging::error("dead_partner_test: kill receiver right after IpcRecv");
                        crate::logging::info_u64("killed_task_id", tid.0);
                        crate::logging::info_u64("ep_id", ep.0 as u64);

                        self.kill_task(
                            task_index,
                            TaskKillReason::UserPageFault {
                                addr: 0,
                                err: 0,
                                rip: 0,
                            },
                        );

                        return;
                    }
                }
            }

            Syscall::IpcSend { ep, msg } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Send, tid, ep, Some(msg));

                self.ipc_send(ep, msg);
            }

            Syscall::IpcReply { ep, msg } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Reply, tid, ep, Some(msg));

                self.ipc_reply(ep, msg);
            }

            // ------------------------------------------------------------
            // Mem demo（PageMap / PageUnmap）
            // - 戻り値は last_syscall_ret に格納（IPC reply と混線させない）
            // - ログ出力は user_program 側の責務（ここでは “値を置く” だけ）
            // ------------------------------------------------------------
            Syscall::PageMap { page, flags } => {
                let ret = self.syscall_page_map(task_index, tid, page, flags);
                self.set_last_syscall_ret_for_current(ret);
            }

            Syscall::PageUnmap { page } => {
                let ret = self.syscall_page_unmap(task_index, tid, page);
                self.set_last_syscall_ret_for_current(ret);
            }
        }
    }

    /// user/kernel を問わず「現在タスクの AddressSpace」に Map を適用する
    fn syscall_page_map(
        &mut self,
        task_index: usize,
        tid: super::TaskId,
        page: VirtPage,
        flags: PageFlags,
    ) -> u64 {
        if task_index >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let as_idx = self.tasks[task_index].address_space_id.0;
        if as_idx >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        // demo は「タスクごとに固定 frame を使い回す」前提（ヒープ無し）
        let frame = match self.get_or_alloc_demo_frame(task_index) {
            Some(f) => f,
            None => {
                crate::logging::error("syscall: PageMap failed (no frame)");
                crate::logging::info_u64("task_id", tid.0);
                return SYSCALL_ERR_ARCH_FAILED;
            }
        };

        let mem_action = MemAction::Map { page, frame, flags };

        // 論理状態（AddressSpace）に反映
        let apply_res = {
            let aspace = &mut self.address_spaces[as_idx];
            aspace.apply(mem_action)
        };

        let logical_ret = match apply_res {
            Ok(()) => SYSCALL_OK,
            Err(crate::mem::address_space::AddressSpaceError::AlreadyMapped) => SYSCALL_ERR_ALREADY_MAPPED,
            Err(crate::mem::address_space::AddressSpaceError::NotMapped) => SYSCALL_ERR_NOT_MAPPED,
            Err(crate::mem::address_space::AddressSpaceError::CapacityExceeded) => SYSCALL_ERR_CAPACITY,
        };

        // すでに論理でコケたなら、物理は触らない
        if logical_ret != SYSCALL_OK {
            return logical_ret;
        }

        // 物理状態（PT）に反映
        let kind = self.address_spaces[as_idx].kind;
        match kind {
            AddressSpaceKind::Kernel => match unsafe { crate::arch::paging::apply_mem_action(mem_action, &mut self.phys_mem) } {
                Ok(()) => SYSCALL_OK,
                Err(_e) => SYSCALL_ERR_ARCH_FAILED,
            },

            AddressSpaceKind::User => {
                let root = match self.address_spaces[as_idx].root_page_frame {
                    Some(r) => r,
                    None => return SYSCALL_ERR_BAD_ASPACE,
                };
                match unsafe { crate::arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem) } {
                    Ok(()) => SYSCALL_OK,
                    Err(_e) => SYSCALL_ERR_ARCH_FAILED,
                }
            }
        }
    }

    /// user/kernel を問わず「現在タスクの AddressSpace」から Unmap を適用する
    fn syscall_page_unmap(
        &mut self,
        task_index: usize,
        _tid: super::TaskId,
        page: VirtPage,
    ) -> u64 {
        if task_index >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let as_idx = self.tasks[task_index].address_space_id.0;
        if as_idx >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let mem_action = MemAction::Unmap { page };

        // 論理状態（AddressSpace）から削除
        let apply_res = {
            let aspace = &mut self.address_spaces[as_idx];
            aspace.apply(mem_action)
        };

        let logical_ret = match apply_res {
            Ok(()) => SYSCALL_OK,
            Err(crate::mem::address_space::AddressSpaceError::AlreadyMapped) => SYSCALL_ERR_ALREADY_MAPPED,
            Err(crate::mem::address_space::AddressSpaceError::NotMapped) => SYSCALL_ERR_NOT_MAPPED,
            Err(crate::mem::address_space::AddressSpaceError::CapacityExceeded) => SYSCALL_ERR_CAPACITY,
        };

        if logical_ret != SYSCALL_OK {
            return logical_ret;
        }

        // 物理状態（PT）も削除
        let kind = self.address_spaces[as_idx].kind;
        match kind {
            AddressSpaceKind::Kernel => match unsafe { crate::arch::paging::apply_mem_action(mem_action, &mut self.phys_mem) } {
                Ok(()) => SYSCALL_OK,
                Err(_e) => SYSCALL_ERR_ARCH_FAILED,
            },

            AddressSpaceKind::User => {
                let root = match self.address_spaces[as_idx].root_page_frame {
                    Some(r) => r,
                    None => return SYSCALL_ERR_BAD_ASPACE,
                };
                match unsafe { crate::arch::paging::apply_mem_action_in_root(mem_action, root, &mut self.phys_mem) } {
                    Ok(()) => SYSCALL_OK,
                    Err(_e) => SYSCALL_ERR_ARCH_FAILED,
                }
            }
        }
    }
}

#[cfg(feature = "ipc_trace_syscall")]
#[derive(Clone, Copy)]
enum TraceKind {
    Recv,
    Send,
    Reply,
}

#[cfg(feature = "ipc_trace_syscall")]
fn trace_ipc(kind: TraceKind, tid: super::TaskId, ep: EndpointId, msg: Option<u64>) {
    match kind {
        TraceKind::Recv => crate::logging::info("ipc_trace kind=ipc_recv"),
        TraceKind::Send => crate::logging::info("ipc_trace kind=ipc_send"),
        TraceKind::Reply => crate::logging::info("ipc_trace kind=ipc_reply"),
    }

    crate::logging::info_u64("task_id", tid.0);
    crate::logging::info_u64("ep_id", ep.0 as u64);

    if let Some(m) = msg {
        crate::logging::info_u64("msg", m);
    }
}
