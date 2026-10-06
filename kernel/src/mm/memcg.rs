//! Memory cgroup integration — per-frame charge hooks.
//!
//! Audit result (issue #3): the charge *subject* is `MemoryCss`
//! (`kernel/src/cgroup/controllers/memory.rs`), which owns the usage
//! counters, the leaf+ancestor transaction (`try_charge`) and the
//! hierarchy-wide release (`uncharge`), mirroring Linux
//! `mm/memcontrol.c` `try_charge`/`commit_charge`/`uncharge`.  This
//! module is only the integration layer between the physical frame
//! allocator and that controller:
//!
//! - [`memcg_alloc_charge`] / [`memcg_free_uncharge`] are called by the
//!   arch `LockedFrameAllocator` on every frame allocation/free, which
//!   covers all RSS accounting points (mm fault, mmap COW, fork copy,
//!   exit release) plus kernel memory (page tables, slab, DMA).
//! - Charge ownership is recorded per frame (Linux: `page->memcg`), so a
//!   free always uncharges the exact CSS that was charged, independently
//!   of which task performs the free.  Task migration (`cgroup.procs`
//!   write) therefore leaves existing charges with the old memcg and
//!   only new charges follow the new css_set, matching Linux.
//! - The hook paths never sleep, shrink or kill: they can run under the
//!   page-manager lock.  The sleeping parts — the `memory.high` throttle
//!   and the `memory.max` OOM kill — run on the fault path
//!   ([`memcg_handle_over_high`] and the drain hooked into
//!   `oom::pagefault_out_of_memory`), like Linux
//!   `mem_cgroup_handle_over_high()` and the memcg OOM handler.

use alloc::{
    sync::Arc,
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    cgroup::{
        controllers::memory::MemoryCss,
        core::CgroupNode,
        subsys::{CgroupSubsysId, CgroupSubsysState},
    },
    libs::spinlock::SpinLock,
    mm::{
        MemoryManagementArch, PhysAddr, allocator::page_frame::PageFrameCount, page::PageReclaimer,
    },
    process::{ProcessManager, RawPid},
    time::{PosixTimeSpec, sleep::nanosleep},
};

/// `memory.high` throttle bounds: pages reclaimed per synchronous round,
/// sleep between rounds, and a hard cap on rounds so a fault never spins
/// indefinitely on a cgroup pinned above its high limit.
const MEMORY_HIGH_RECLAIM_BATCH: usize = 32;
const MEMORY_HIGH_WAIT_NS: i64 = 1_000_000;
const MEMORY_HIGH_MAX_ROUNDS: u32 = 8;

/// Per-frame charge ownership, indexed by page frame number (Linux:
/// `page->memcg`).  Frames without an owner were allocated while memcg
/// accounting was unavailable (early boot, interrupt context, OOM-victim
/// bypass) and are never uncharged.
static PAGE_OWNERS: SpinLock<Option<Vec<Option<Arc<dyn CgroupSubsysState>>>>> = SpinLock::new(None);

/// Leaf memory CSS of the most recent charge refused by a `memory.max`
/// limit.  The refusal is retained until the fault path consumes it;
/// concurrent refusals are serialized instead of overwriting one another.
static PENDING_MAX_OOM: SpinLock<Option<Arc<dyn CgroupSubsysState>>> = SpinLock::new(None);

/// Size the per-frame ownership map from the physical memory map.
///
/// Called once from `mm_init()` after the frame allocator and the kernel
/// heap exist.  Failure is non-fatal: memcg accounting stays disabled
/// (no owners are recorded, so charges and uncharges stay consistent).
pub fn memcg_page_owners_init() {
    use crate::mm::memblock::mem_block_manager;

    let mut max_pfn: usize = 0;
    let mgr = mem_block_manager();
    for index in 0..mgr.total_initial_memory_regions() {
        if let Some(area) = mgr.get_initial_memory_region(index) {
            let end_pfn = area.area_end_aligned().data() >> MMArch::PAGE_SHIFT;
            max_pfn = max_pfn.max(end_pfn);
        }
    }
    if max_pfn == 0 {
        log::warn!("memcg: no physical memory areas, page accounting disabled");
        return;
    }

    let mut owners: Vec<Option<Arc<dyn CgroupSubsysState>>> = Vec::new();
    if owners.try_reserve_exact(max_pfn).is_err() {
        log::warn!(
            "memcg: cannot reserve page ownership map for {} frames, accounting disabled",
            max_pfn
        );
        return;
    }
    owners.resize(max_pfn, None);
    *PAGE_OWNERS.lock_irqsave() = Some(owners);
    log::info!("memcg: page ownership map covers {} frames", max_pfn);
}

/// Resolve the memory CSS that new allocations of the current task are
/// charged to (the leaf CSS of the task's cgroup).
fn current_memcg_css() -> Option<Arc<dyn CgroupSubsysState>> {
    if !ProcessManager::initialized() {
        return None;
    }
    // Interrupt context cannot charge: it must not trip the OOM/high
    // bookkeeping of an arbitrary interrupted task, and it may run with
    // preemption disabled.  Such frames simply stay unowned.
    if crate::exception::interrupt_context::in_interrupt() {
        return None;
    }
    let pcb = ProcessManager::current_pcb();
    let node = pcb.task_cgroup_node();
    node.css(CgroupSubsysId::Memory)
}

/// Charge `pages` frames starting at `start` to the current task's memory
/// hierarchy and record their ownership.
///
/// Called by `LockedFrameAllocator::allocate`/`allocate_below` after the
/// raw allocation succeeded and after the inner allocator lock was
/// released.  On refusal the caller must return the frames to the buddy
/// allocator *without* uncharging — which is what happens automatically
/// because refused frames never get an owner recorded.
pub fn memcg_alloc_charge(start: PhysAddr, pages: u64) -> Result<(), SystemError> {
    // OOM victims get reserve access (Linux TIF_MEMDIE): exit paths must
    // be able to allocate while tearing down the mm that triggered the
    // kill.  Such frames stay unowned and are not uncharged.
    if crate::mm::oom::current_is_oom_victim() {
        return Ok(());
    }
    let Some(css) = current_memcg_css() else {
        return Ok(());
    };
    let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() else {
        return Ok(());
    };

    match memcg.try_charge(pages) {
        Ok(()) => {
            record_frame_owners(start, pages, &css);
            Ok(())
        }
        Err(err) => {
            // Keep the first refusal until the fault path consumes it.
            // Later concurrent refusals are coalesced instead of replacing
            // the CSS that identified the active OOM scope.
            let mut pending = PENDING_MAX_OOM.lock_irqsave();
            if pending.is_none() {
                *pending = Some(css.clone());
            }
            Err(err)
        }
    }
}

/// Uncharge `pages` frames starting at `start` from the CSS that owns
/// them.  Called by `LockedFrameAllocator::free`.
///
/// Consecutive frames with the same owner are released as one hierarchy
/// transaction.  This performs no allocation and never sleeps.
pub fn memcg_free_uncharge(start: PhysAddr, pages: u64) {
    let mut guard = PAGE_OWNERS.lock_irqsave();
    let Some(map) = guard.as_mut() else {
        return;
    };

    let first = start.data() >> MMArch::PAGE_SHIFT;
    if first >= map.len() {
        return;
    }
    let last = (first + pages as usize).min(map.len());

    let mut run_owner: Option<Arc<dyn CgroupSubsysState>> = None;
    let mut run_len: u64 = 0;
    for slot in &mut map[first..last] {
        let owner = slot.take();
        let same_run = match (&run_owner, &owner) {
            (Some(current), Some(new)) => Arc::ptr_eq(current, new),
            _ => false,
        };
        if same_run {
            run_len += 1;
        } else {
            if let Some(current) = run_owner.take() {
                uncharge_css(&current, run_len);
            }
            match owner {
                Some(next) => {
                    run_owner = Some(next);
                    run_len = 1;
                }
                None => run_len = 0,
            }
        }
    }
    if let Some(current) = run_owner {
        uncharge_css(&current, run_len);
    }
}

fn uncharge_css(css: &Arc<dyn CgroupSubsysState>, pages: u64) {
    if pages == 0 {
        return;
    }
    if let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() {
        memcg.uncharge(pages);
    }
}

fn record_frame_owners(start: PhysAddr, pages: u64, owner: &Arc<dyn CgroupSubsysState>) {
    let mut guard = PAGE_OWNERS.lock_irqsave();
    let Some(map) = guard.as_mut() else {
        return;
    };
    let first = start.data() >> MMArch::PAGE_SHIFT;
    if first >= map.len() {
        return;
    }
    let last = (first + pages as usize).min(map.len());
    for slot in &mut map[first..last] {
        *slot = Some(owner.clone());
    }
}

/// `memory.high` throttle, mirroring Linux `mem_cgroup_handle_over_high()`
/// which runs on the fault path instead of inside the charge.
///
/// Bounded: at most [`MEMORY_HIGH_MAX_ROUNDS`] synchronous reclaim rounds,
/// each with a progress check; sleeps only happen when a round reclaimed
/// nothing.  Never called from allocator context, so no allocator or
/// page-manager lock can be held.
pub fn memcg_handle_over_high() {
    let Some(css) = current_memcg_css() else {
        return;
    };
    let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() else {
        return;
    };
    // Only throttle when a charge on this hierarchy actually tripped high;
    // the flag keeps unrelated faults lock-free in the common case.
    if !memcg.take_high_trip() {
        return;
    }

    let mut rounds = MEMORY_HIGH_MAX_ROUNDS;
    while rounds > 0 && memcg.high_limit_exceeded() {
        PageReclaimer::wakeup_claim_thread();
        let progress = PageReclaimer::shrink_list(PageFrameCount::new(MEMORY_HIGH_RECLAIM_BATCH));
        if progress.reclaimed == 0 {
            // No forward progress this round: back off briefly instead of
            // hammering the same LRU.
            let _ = nanosleep(PosixTimeSpec::new(0, MEMORY_HIGH_WAIT_NS));
        }
        rounds -= 1;
    }
}

/// Drain a pending `memory.max` OOM request through the `oom.rs` state
/// machine, with victim selection scoped to the cgroup subtree whose
/// limit was exceeded.
///
/// Returns `Some(outcome)` when the memcg OOM path acted (kill issued and
/// memory released, or the current task is the victim); `None` lets the
/// caller fall back to the global OOM path.
pub(crate) fn drain_pending_memcg_oom(
    ctx: crate::mm::oom::OomContext,
) -> Option<crate::mm::oom::OomOutcome> {
    use crate::mm::oom::{self, OomOutcome};

    let leaf = PENDING_MAX_OOM.lock_irqsave().take()?;

    // The refusal may already have been relieved (a kill from another
    // charger, task exits, or a raised limit).
    let exceeded = find_max_exceeded(&leaf)?;

    let pids = collect_subtree_tasks(&exceeded.cgroup());
    if pids.is_empty() {
        return None;
    }

    if let Some(memcg) = exceeded.as_any().downcast_ref::<MemoryCss>() {
        memcg.note_memcg_oom();
    }
    let outcome = oom::scoped_out_of_memory(ctx, pids);
    match outcome {
        OomOutcome::Retry | OomOutcome::CurrentTaskKilled => {
            if let Some(memcg) = exceeded.as_any().downcast_ref::<MemoryCss>() {
                memcg.note_memcg_oom_kill();
            }
            Some(outcome)
        }
        // Nothing killable inside the offending subtree: fall through to
        // the global OOM path (same behaviour as its own NoVictim).
        OomOutcome::NoVictim => None,
    }
}

/// Walk the charge chain from `leaf` towards the root and return the
/// first CSS whose `memory.max` is currently exceeded — the scope a kill
/// must come from to relieve the refused charge.
fn find_max_exceeded(leaf: &Arc<dyn CgroupSubsysState>) -> Option<Arc<dyn CgroupSubsysState>> {
    let mut current = Some(leaf.clone());
    while let Some(css) = current {
        if let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() {
            if memcg.max_exceeded_now() {
                return Some(css);
            }
        }
        current = css.parent();
    }
    None
}

/// All task pids in `node` and its descendants.
fn collect_subtree_tasks(node: &Arc<CgroupNode>) -> Vec<RawPid> {
    let mut pids = node.tasks();
    for child in node.children() {
        pids.extend(collect_subtree_tasks(&child));
    }
    pids
}
