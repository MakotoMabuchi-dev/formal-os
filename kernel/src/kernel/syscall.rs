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
//
// ★整理（テスト分離）:
// - dead_partner_test 等の “テスト注入” は demo/ 側に集約し、syscall 境界から排除する。

use super::{EndpointId, KernelState, LogEvent};

use crate::mem::address_space::AddressSpaceKind;
use crate::mem::addr::VirtPage;
use crate::mem::paging::{MemAction, PageFlags};

const SYSCALL_OK: u64 = 0;
const SYSCALL_ERR_ALREADY_MAPPED: u64 = 1;
const SYSCALL_ERR_NOT_MAPPED: u64 = 2;
const SYSCALL_ERR_CAPACITY: u64 = 3;
const SYSCALL_ERR_ARCH_FAILED: u64 = 10;
const SYSCALL_ERR_BAD_ASPACE: u64 = 11;

#[derive(Clone, Copy)]
pub enum Syscall {
    IpcRecv { ep: EndpointId },
    IpcSend { ep: EndpointId, msg: u64 },
    IpcReply { ep: EndpointId, msg: u64 },

    PageMap { page: VirtPage, flags: PageFlags },
    PageUnmap { page: VirtPage },
}

impl KernelState {
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

        // kernel task の IPC syscall は禁止
        {
            let as_idx = self.tasks[task_index].address_space_id.0;
            let is_kernel = as_idx < self.num_tasks && self.address_spaces[as_idx].kind == AddressSpaceKind::Kernel;

            if is_kernel {
                match sc {
                    Syscall::IpcRecv { ep } | Syscall::IpcSend { ep, .. } | Syscall::IpcReply { ep, .. } => {
                        crate::logging::error("syscall: kernel task IPC is forbidden (ignored at syscall boundary)");
                        crate::logging::info_u64("task_id", tid.0);
                        crate::logging::info_u64("ep_id", ep.0 as u64);
                        return;
                    }
                    _ => {}
                }
            }
        }

        self.push_event(LogEvent::SyscallHandled { task: tid });

        match sc {
            Syscall::IpcRecv { ep } => {
                #[cfg(feature = "ipc_trace_syscall")]
                trace_ipc(TraceKind::Recv, tid, ep, None);

                self.ipc_recv(ep);

                // テスト注入（dead_partner_test 等）は demo 側に集約
                crate::kernel::demo::on_after_ipc_recv(self, task_index, tid, ep);
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

    fn syscall_page_map(&mut self, task_index: usize, tid: super::TaskId, page: VirtPage, flags: PageFlags) -> u64 {
        if task_index >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let as_idx = self.tasks[task_index].address_space_id.0;
        if as_idx >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let frame = match self.get_or_alloc_demo_frame(task_index) {
            Some(f) => f,
            None => {
                crate::logging::error("syscall: PageMap failed (no frame)");
                crate::logging::info_u64("task_id", tid.0);
                return SYSCALL_ERR_ARCH_FAILED;
            }
        };

        let mem_action = MemAction::Map { page, frame, flags };

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

        match self.address_spaces[as_idx].kind {
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

    fn syscall_page_unmap(&mut self, task_index: usize, _tid: super::TaskId, page: VirtPage) -> u64 {
        if task_index >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let as_idx = self.tasks[task_index].address_space_id.0;
        if as_idx >= self.num_tasks {
            return SYSCALL_ERR_BAD_ASPACE;
        }

        let mem_action = MemAction::Unmap { page };

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

        match self.address_spaces[as_idx].kind {
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

fn mailbox_decode(sysno: u64, a0: u64, a1: u64, _a2: u64) -> Option<Syscall> {
    let ep = EndpointId(a0 as usize);
    match sysno {
        10 => Some(Syscall::IpcRecv { ep }),
        11 => Some(Syscall::IpcSend { ep, msg: a1 }),
        12 => Some(Syscall::IpcReply { ep, msg: a1 }),
        _ => None,
    }
}

/// ring3 mailbox dispatcher
pub fn mailbox_dispatch(ks: &mut KernelState, sysno: u64, a0: u64, a1: u64, a2: u64) -> u64 {
    let ring3_task_index: usize = 1;

    match sysno {
        1 => return a0.wrapping_add(a1).wrapping_add(a2),
        2 => return ks.tick_count,
        30 => {
            ks.tick();
            return ks.tick_count;
        }
        31 => {
            if ring3_task_index < ks.num_tasks {
                let v = ks.tasks[ring3_task_index].last_reply.unwrap_or(0);
                ks.tasks[ring3_task_index].last_reply = None;
                return v;
            }
            return 0;
        }
        _ => {}
    }

    let is_ipc_sysno = matches!(sysno, 10 | 11 | 12);

    if is_ipc_sysno {
        if ring3_task_index < ks.num_tasks && ks.tasks[ring3_task_index].state != super::TaskState::Dead {
            ks.current_task = ring3_task_index;
        }

        if let Some(sc) = mailbox_decode(sysno, a0, a1, a2) {
            let tid = ks.tasks[ks.current_task].id;
            ks.push_event(LogEvent::SyscallIssued { task: tid });
            ks.handle_syscall(sc);
        }
        return 0;
    }

    let prev_task = ks.current_task;

    let ret = if let Some(sc) = mailbox_decode(sysno, a0, a1, a2) {
        let tid = ks.tasks[ks.current_task].id;
        ks.push_event(LogEvent::SyscallIssued { task: tid });
        ks.handle_syscall(sc);
        0
    } else {
        0
    };

    ks.current_task = prev_task;
    ret
}
