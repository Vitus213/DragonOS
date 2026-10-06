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
//! - Ownership is recorded as a *weak* reference plus the owner CSS's
//!   generation (`node_id`), not a strong `Arc`: a cgroup deleted via
//!   rmdir must be reclaimable even while its pages are still alive
//!   (issue #30).  Releasing a frame whose owner CSS is gone uncharges
//!   nothing — `MemoryCss::css_offline` already rolled the residual
//!   usage back off the ancestor chain at rmdir time.
//! - The hook paths never sleep, shrink or kill: they can run under the
//!   page-manager lock.  The sleeping parts — the `memory.high` throttle
//!   and the `memory.max` OOM kill — run on the fault path
//!   ([`memcg_handle_over_high`] and the drain hooked into
//!   `oom::pagefault_out_of_memory`), like Linux
//!   `mem_cgroup_handle_over_high()` and the memcg OOM handler.
//! # 锁序与中断纪律（issue #28，全序图见 `kernel/src/cgroup/LOCK_ORDER.md`）
//!
//! `LockedFrameAllocator::free` 每次释放都会进入 [`memcg_free_uncharge`]
//! 与 `uncharge_css → MemoryCss::uncharge`，而页帧释放会发生在硬中断
//! 上下文里（驱动 IRQ 处理路径）。本模块锁族的纪律：
//!
//! ```text
//! 嵌套方向（外 → 内）。修复后 PAGE_OWNERS 与 MEMORY_CHARGE_LOCK 互不
//! 嵌套（free 路径持 PAGE_OWNERS 时只做归属摘取，放锁后才 uncharge），
//! 唯一保留的嵌套边是 MEMORY_CHARGE_LOCK → MemoryCss::inner：
//!
//!   PAGE_OWNERS(irqsave)                  独立获取；锁内零分配、零嵌套
//!   MEMORY_CHARGE_LOCK(irqsave)           独立获取，或 ↓ 嵌套
//!     → MemoryCss::inner(irqsave)
//!   PENDING_MAX_OOM(irqsave)              独立获取；锁内零嵌套
//! ```
//!
//! - `PAGE_OWNERS` 一律 `lock_irqsave()`（含一次性初始化），临界区内只
//!   做归属槽位的 take/填值，绝不跨 uncharge 持有、绝不在其下分配内存
//!   （分配可能触发 slab 补帧 → 同 CPU 重进本锁）。free 路径的两段式
//!   见 [`memcg_free_uncharge`]。
//! - `MEMORY_CHARGE_LOCK` 族（含 `MemoryCss::inner`）与 `PENDING_MAX_OOM`
//!   全仓一律 `lock_irqsave()`：free 钩子在硬中断里单独获取该锁族，若
//!   允许任何 IRQ-on 持有者存在，持锁 task 被同 CPU 硬中断打断、IRQ
//!   重进同一把非重入 CAS 自旋锁即永久锁死（本卡修复的原始缺陷）。
//! - memcg 锁族从不持锁回调页分配器，也从不持锁进入调度器锁
//!   （pi_lock/rq_lock/task_lock/freezer task_lock）；freezer 与 OOM
//!   的进程侧锁链不含任何 memcg 锁，两侧无交叉边。全部交叉点的
//!   逐一论证见 LOCK_ORDER.md。

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    cgroup::{
        controllers::memory::{ChargeToken, MemoryCss},
        core::CgroupNode,
        subsys::{CgroupSubsysId, CgroupSubsysState},
    },
    libs::spinlock::SpinLock,
    mm::{
        allocator::page_frame::PageFrameCount, page::PageReclaimer, MemoryManagementArch, PhysAddr,
    },
    process::{ProcessManager, RawPid},
    time::{sleep::nanosleep, PosixTimeSpec},
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
///
/// 归属记录是弱引用令牌（`ChargeToken`：`Weak<CSS>` + 代际号），不是
/// 强 `Arc`（issue #30）：被 rmdir 的组的 MemoryCss 连同父链必须在
/// 其最后一个强引用（在线节点 subsys 槽随 `clear_css` 释放）消失后
/// 即可回收，即使它的页面还长期存活——旧实现每帧强 `Arc` 持有 CSS，
/// 已删组泄漏正比于已删组数 × 页面存活期。令牌解析失败（CSS 已回收
/// 或代际不符）时释页跳过结算：对应的 usage 已在 `css_offline` 结算
/// 回祖先链，再扣即双扣。
///
/// 中断纪律（issue #28）：一律 `lock_irqsave()` 获取（获取点共 3 处，
/// 均在本模块：init、alloc 侧 record、free 侧批摘取循环）；
/// 临界区内只做槽位 take/填值与 `FREE_RUN_BATCH` 栈批写入，绝不获取
/// 任何其他锁、绝不分配内存、绝不跨 `uncharge_token` 持有。页帧释放在
/// 硬中断里可达，任何 IRQ-on 持有该锁的窗口都等于把同 CPU 重入死锁
/// 留给下一次驱动释放。
static PAGE_OWNERS: SpinLock<Option<Vec<Option<ChargeToken>>>> = SpinLock::new(None);

/// Leaf memory CSS token of the most recent charge refused by a
/// `memory.max` limit.  The refusal is retained until the fault path
/// consumes it; concurrent refusals are serialized instead of
/// overwriting one another.
///
/// issue #30：槽内存弱引用令牌而非强 `Arc<CSS>`——拒绝滞留多久都不
/// 再钉住 CSS 与其父链；rmdir 时 `css_offline` 主动清除指向本 CSS 的
/// 槽位（[`memcg_forget_css`]），排水侧对已回收/已拆除的令牌走
/// fail-closed 分支，绝不再 `expect` panic。
///
/// 中断纪律（issue #28）：与 memcg charge 锁族同级，一律 `lock_irqsave()`；
/// 锁内只 clone/take 一个 `Option<ChargeToken>`，零嵌套。
static PENDING_MAX_OOM: SpinLock<Option<ChargeToken>> = SpinLock::new(None);

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

    let mut owners: Vec<Option<ChargeToken>> = Vec::new();
    if owners.try_reserve_exact(max_pfn).is_err() {
        log::warn!(
            "memcg: cannot reserve page ownership map for {} frames, accounting disabled",
            max_pfn
        );
        return;
    }
    owners.resize(max_pfn, None);
    // 统一使用 lock_irqsave()（见本模块文档锁序节）：PAGE_OWNERS 所有
    // 获取点必须使用同一中断纪律，否则持 lock() IRQ-on 的 CPU 被硬中断
    // 打断、IRQ 再取同一把非重入 CAS 自旋锁即同 CPU 永久锁死。
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
            record_frame_owners(start, pages, &ChargeToken::new(&css));
            Ok(())
        }
        Err(err) => {
            // Keep the first refusal until the fault path consumes it.
            // Later concurrent refusals are coalesced instead of replacing
            // the CSS that identified the active OOM scope.
            //
            // 竞态协议（issue #30）：存入前先在本锁内复核 offline——
            // `css_offline` 的拆除序列是"置 offline 位 → 取本锁清
            // pending → 结算残量"。若本临界区排在清除之前，那次存入
            // 会被清除带走；若排在之后，offline 位必然可见，本行放弃
            // 存入——两种交错都不可能在 rmdir 之后留下悬挂 pending。
            //
            // irqsave 纪律与 memcg 锁族一致（见模块头）：本锁只保护
            // pending 槽，绝不在此临界区获取其他锁。
            let mut pending = PENDING_MAX_OOM.lock_irqsave();
            if pending.is_none() && !memcg.is_offline() {
                *pending = Some(ChargeToken::new(&css));
            }
            Err(err)
        }
    }
}

/// Uncharge `pages` frames starting at `start` from the CSS that owns
/// them.  Called by `LockedFrameAllocator::free`.
///
/// 两段式（issue #28；完整锁序论证见模块头与
/// `kernel/src/cgroup/LOCK_ORDER.md`）：
/// 1. `PAGE_OWNERS`（irqsave）临界区内只做一件事——把连续同归属的页帧
///    摘取（`slot.take()`）合并成 runs 装入栈上批数组
///    （[`take_owner_runs`]）；临界区内零堆分配——分配可能经 slab 补帧
///    回调 `LockedFrameAllocator::allocate`/`free` 重进本锁；
/// 2. 放锁之后逐段调用 [`uncharge_token`] 完成层级 uncharge 事务。
///
/// 为什么必须"先放锁、再 uncharge"：原实现在持 `PAGE_OWNERS` 期间调用
/// `uncharge_css → MemoryCss::uncharge → MEMORY_CHARGE_LOCK`，新增了
/// PAGE_OWNERS → MEMORY_CHARGE_LOCK 嵌套边；而 charge 侧是
/// MEMORY_CHARGE_LOCK（单独获取、放锁）→ record_frame_owners 取
/// PAGE_OWNERS（单独获取）。同一锁族两个方向、且 free 路径在硬中断里
/// 可达：叠加 `record_frame_owners` 旧实现的 IRQ-on `lock()`，硬中断落
/// 在 charge 侧任一持锁段时，被中断 task 持有的 MEMORY_CHARGE_LOCK 被
/// IRQ 的帧释放路径重进——非重入 CAS 自旋锁同 CPU 永久锁死，并拖死
/// 同链等待者（本卡修复的原始缺陷）。修复后两锁互不嵌套，全锁族统一
/// irqsave 纪律，同 CPU 重入与跨 CPU 成环两条路径同时被封死。
///
/// 批与批之间允许其他 CPU 对同区间的 charge/free 插队：buddy 单次释放
/// 的块由同一次 `record_frame_owners` 整段记名，常态一批摘完（1 个
/// run）；归属交错的病态输入由续扫批兜底——正确性只依赖"每个锁段摘
/// 走的 runs 必在放锁后才 uncharge"，不依赖批大小。
///
/// Never sleeps, never allocates.
pub fn memcg_free_uncharge(start: PhysAddr, pages: u64) {
    let first = start.data() >> MMArch::PAGE_SHIFT;
    let end = first.saturating_add(pages as usize);
    let mut pos = first;
    loop {
        let mut batch: [(Option<ChargeToken>, u64); FREE_RUN_BATCH] =
            core::array::from_fn(|_| (None, 0));
        let (used, resume) = {
            let mut guard = PAGE_OWNERS.lock_irqsave();
            let Some(map) = guard.as_mut() else {
                return;
            };
            take_owner_runs(map, pos, end, ChargeToken::same_owner, &mut batch)
        };
        // 锁已释放：以下只获取 MEMORY_CHARGE_LOCK 锁族（irqsave），
        // 不再触碰 PAGE_OWNERS。
        for slot in batch.iter_mut().take(used) {
            if let Some(owner) = slot.0.take() {
                uncharge_token(&owner, slot.1);
            }
        }
        match resume {
            None => return,
            Some(next) => pos = next,
        }
    }
}

/// 一次 `PAGE_OWNERS` 锁段带走的 run 批容量。buddy 单次释放是
/// `record_frame_owners` 写入的连续段（常态 1 run）；8 槽覆盖一切
/// 现实形态，满批后续扫兜底。
const FREE_RUN_BATCH: usize = 8;

/// 纯逻辑摘取段：把 `[pos, min(end, map.len()))` 内连续同归属（`same`
/// 判定）的页帧归属从槽位 `take()` 出来并合并成 runs，按序写入 `batch`。
/// 批槽用尽时不再摘取，返回 `Some(首个未摘槽位下标)`（该下标之后区间
/// 原封未动；调用方 uncharge 本批后从这里续扫，游标严格前进——能返回
/// `Some` 的前提是批已装满，即至少摘走过一个 run）。区间处理完返回
/// `None`。返回 `(本批装入的 run 数, 续扫点?)`。
///
/// 泛型 owner + `same` 判定使宿主单测无需真实锁/PCB/中断即可直接验证
/// 摘取、归并、缝隙跳过、钳制与分批续扫语义（见模块尾 tests）。
/// 不变式（锁序纪律要求，见模块头）：只在调用者已持有的 `PAGE_OWNERS`
/// 临界区内运行；不获取任何锁、不分配内存、不触碰 memcg 计数。
fn take_owner_runs<T, S>(
    map: &mut Vec<Option<T>>,
    pos: usize,
    end: usize,
    same: S,
    batch: &mut [(Option<T>, u64)],
) -> (usize, Option<usize>)
where
    S: Fn(&T, &T) -> bool,
{
    let last = end.min(map.len());
    let mut count = 0usize;
    let mut run_owner: Option<T> = None;
    let mut run_len: u64 = 0;
    let mut i = pos;
    while i < last {
        let continues = match (&run_owner, &map[i]) {
            (Some(current), Some(next)) => same(current, next),
            _ => false,
        };
        if continues {
            map[i].take();
            run_len += 1;
            i += 1;
            continue;
        }
        // run 边界（换主或无主缝隙）：结算已打开的 run。
        if let Some(current) = run_owner.take() {
            batch[count] = (Some(current), run_len);
            count += 1;
        }
        if count >= batch.len() {
            // 批满且 map[i] 未摘：放锁 uncharge 本批后从 i 续扫。
            return (count, Some(i));
        }
        match map[i].take() {
            Some(next) => {
                run_owner = Some(next);
                run_len = 1;
            }
            // 无主缝隙：只消费槽位，不占批槽。
            None => run_len = 0,
        }
        i += 1;
    }
    if let Some(current) = run_owner.take() {
        batch[count] = (Some(current), run_len);
        count += 1;
    }
    (count, None)
}

/// 锁外结算一段归属：令牌解析成功才 uncharge。
///
/// `resolve()` 返回 `None`（CSS 已随 rmdir 回收）时跳过——该帧对应
/// 的计费已在 `css_offline` 结算时回退给祖先链，再扣即双扣；这正是
/// 弱引用归属与残余结算必须配对出现的原因（issue #30）。
fn uncharge_token(token: &ChargeToken, pages: u64) {
    if pages == 0 {
        return;
    }
    let Some(css) = token.resolve() else {
        return;
    };
    if let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() {
        memcg.uncharge(pages);
    }
}

fn record_frame_owners(start: PhysAddr, pages: u64, owner: &ChargeToken) {
    // irqsave 纪律与 free 侧一致（见模块头锁序节）：本临界区在硬中断
    // 可达的分配路径上（free 一定在 IRQ 下可达，allocate 的调用方如
    // 驱动 probe/DMA 同样可能在 IRQ-off 上下文），统一纪律消除同 CPU
    // 重入窗口。
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

/// rmdir 拆除点清除 pending OOM 槽（issue #30，`MemoryCss::css_offline`
/// 调用）。
///
/// 比较判据用代际号 `node_id`：每个在线节点恰有一个 MemoryCss，节点
/// id 在 `CgroupRoot` 内单调递增且永不复用，故 id 相等 ⇔ 槽内令牌与
/// 本 CSS 同一，不存在跨代际误删；比 `Arc::ptr_eq` 更强的地方在于
/// 令牌是弱引用，无法从 `&MemoryCss` 现场构造可比较的 `Arc`。
/// 只清槽、不碰其他锁（irqsave 纪律见模块头）。
pub fn memcg_forget_css(css: &MemoryCss) {
    let node_id = css.node_id();
    let mut pending = PENDING_MAX_OOM.lock_irqsave();
    if pending
        .as_ref()
        .is_some_and(|token| token.node_id() == node_id)
    {
        *pending = None;
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
/// limit was exceeded, plus the `memory.oom.group` subtree cleanup.
///
/// Linux flow mirrored here (`oom_kill.c::oom_kill_process`): first a
/// single victim is chosen and killed inside the offending subtree; then
/// `mem_cgroup_get_oom_group` walks the victim's memory CSS chain up to
/// the OOM domain and, if any level on that chain has
/// `memory.oom.group`, the *highest* such group is cleaned by killing
/// every killable task in its subtree while the group's
/// `memory.events.oom_group_kill` counter is bumped once per cleanup.
///
/// `None`（与 #30 约定的不变量一致）只意味着「本次没有需要 scoped 处理
/// 的越限」：无挂起请求，或越限已被其它路径解除（`find_max_exceeded`
/// 为空）——此时调用方回退全局路径是正确的。
/// 一旦确定存在越限子树，本函数**必须**返回 `Some`：scoped 结果里
/// 不存在「逃逸全局」这一选项（issue #27 缺陷一）。
///
/// fail-closed（issue #30）：pending 是弱引用令牌，指向的 CSS/节点可能
/// 已被 rmdir 拆除。两条悬挂路径（令牌无法解析、越限 CSS 的节点归属
/// 已注销）都丢弃 pending 并返回 `Retry`——缺页任务重试自己的分配，
/// 其计费若仍被在线组拒绝会重新存入**该组自己**的 pending，下一轮
/// 排水即正常作用域化；绝不会走旧实现的 `expect("cgroup node dropped")`
/// panic 面。
pub(crate) fn drain_pending_memcg_oom(
    ctx: crate::mm::oom::OomContext,
) -> Option<crate::mm::oom::OomOutcome> {
    use crate::mm::oom::{self, OomOutcome};

    // irqsave 纪律（模块头锁序节）：PENDING_MAX_OOM 与 charge 锁族同级。
    // 守卫在本语句末释放——随后的解析与遍历在无锁状态下获取
    // MemoryCss::inner，绝不构成 PENDING → inner 嵌套。
    let pending = PENDING_MAX_OOM.lock_irqsave().take()?;

    // 令牌可能已随 CSS 回收而失效：拒绝的作用域不存在了，丢弃即可。
    let Some(leaf) = pending.resolve() else {
        return Some(OomOutcome::Retry);
    };

    // The refusal may already have been relieved (a kill from another
    // charger, task exits, or a raised limit).
    let exceeded = find_max_exceeded(&leaf)?;

    // 越限 CSS 若在解析后被并发 rmdir：节点归属已注销，作用域消失，
    // 与上面同样丢弃 pending 并让缺页任务重试（fail-closed，issue #30）。
    // 空子树不再回退全局选择：作用域成立时 scoped 必须闭合（issue #27）。
    let Some(node) = exceeded.cgroup_node() else {
        return Some(OomOutcome::Retry);
    };
    if let Some(memcg) = exceeded.as_any().downcast_ref::<MemoryCss>() {
        memcg.note_memcg_oom();
    }
    // issue #27：传入越限子树的节点（作用域），由 oom.rs 每轮选择时
    // 重新收集组内任务；不再传递一次性 pid 快照。#30 后取法为
    // cgroup_node(): Option（悬挂即 fail-closed 返回 Retry，见上）。
    // issue #37：on_kill 回调携带被杀受害者 tgid，做 oom_kill 事件计数
    // 与 memory.oom.group 的组子树清理。`group_cleanup_done` 使清理每次
    // 排水至多触发一轮：状态机跨重试可能击杀多个受害者，不能每次击杀
    // 都重杀全组。
    let mut group_cleanup_done = false;
    let outcome = oom::scoped_out_of_memory(ctx, node, &mut |killed| {
        let Some(killed_tgid) = killed else {
            return;
        };
        if let Some(memcg) = exceeded.as_any().downcast_ref::<MemoryCss>() {
            memcg.note_memcg_oom_kill();
        }
        if group_cleanup_done {
            return;
        }
        group_cleanup_done = true;
        // Linux `mem_cgroup_get_oom_group(victim, oom_domain)`: the group
        // is resolved from the victim's *current* memory CSS chain; a
        // victim that already migrated out of the offending subtree
        // ignores oom.group (see `MemoryCss::find_oom_group`).
        let Some(victim_css) = victim_memcg_css(killed_tgid) else {
            return;
        };
        let Some(group) = MemoryCss::find_oom_group(&victim_css, &exceeded) else {
            return;
        };
        // Linux `oom_kill_memcg_member` scans the group *subtree*
        // (`for_each_mem_cgroup_tree` + `css_task_iter`) and kills every
        // killable task; the main victim above was already SIGKILLed and
        // counted, so the scan skips its tgid.  节点访问走 #30 的
        // fail-closed `cgroup_node()`：CSS 若已 offline（并发 rmdir 拆除
        // 中）返回 None，放弃本轮清理即可（任务计费仍由 CSS 强引用持有）。
        let Some(group_memcg) = group.as_any().downcast_ref::<MemoryCss>() else {
            return;
        };
        let Some(group_node) = group_memcg.cgroup_node() else {
            return;
        };
        kill_oom_group_subtree(&group_node, killed_tgid);
        group_memcg.note_memcg_oom_group_kill();
    });
    match outcome {
        OomOutcome::Retry | OomOutcome::CurrentTaskKilled => Some(outcome),
        // scoped 路径的 no_victim 回调绝不产出 NoVictim（issue #27 防
        // 逃逸不变量）；防御性兜底：即使出现，也按 Retry 处理（charge
        // 重试驱动前进），绝不返回 None 让缺页路径落入全局受害者选择。
        OomOutcome::NoVictim => Some(OomOutcome::Retry),
    }
}

/// The memory CSS the task `tgid` leader is charged to right now
/// (Linux: `mem_cgroup_from_task(victim)`).
fn victim_memcg_css(tgid: RawPid) -> Option<Arc<dyn CgroupSubsysState>> {
    let pcb = ProcessManager::find(tgid)?;
    pcb.task_cgroup_node().css(CgroupSubsysId::Memory)
}

/// Kill every OOM-killable task in the cgroup subtree rooted at `group`
/// (Linux: `mem_cgroup_scan_tasks(oom_group, oom_kill_memcg_member)`; its
/// css-task iteration is exactly our per-node `tasks()` walk).
///
/// `victim_tgid` is the process the OOM state machine just killed: Linux
/// SIGKILLs the main victim *before* the scan, so `task_will_free_mem` is
/// true for it and the member scan skips it; we skip it by tgid to keep
/// the group cleanup idempotent.
fn kill_oom_group_subtree(group: &Arc<CgroupNode>, victim_tgid: RawPid) {
    use crate::arch::ipc::signal::Signal;
    use crate::ipc::signal_types::{SigCode, SigInfo, SigType};
    use crate::process::pid::PidType;

    let pids = crate::mm::oom::collect_subtree_task_pids(group);
    let targets = select_group_kill_targets(pids, victim_tgid, |pid| {
        ProcessManager::find(pid).map(|task| {
            let tgid = task.raw_tgid();
            let leader = ProcessManager::find(tgid).unwrap_or(task);
            // Linux `oom_kill_memcg_member`: init is never killed here,
            // and tasks that pinned `oom_score_adj` to -1000 are
            // protected.  The filter keys on the thread group leader so
            // all threads of one process are treated as one unit.
            GroupKillProbe {
                tgid,
                protected: tgid.data() <= 1
                    || leader.sig_info_irqsave().oom_score_adj()
                        == crate::mm::oom::OOM_SCORE_ADJ_MIN,
            }
        })
    });
    let killed = run_group_kill(&targets, &mut |tgid| {
        let Some(leader) = ProcessManager::find(tgid) else {
            return Err(SystemError::ESRCH);
        };
        let mut info = SigInfo::new(
            Signal::SIGKILL,
            0,
            SigCode::Kernel,
            SigType::Kill {
                pid: RawPid::new(0),
                uid: 0,
            },
        );
        // PidType::TGID 把 SIGKILL 投递给整个线程组，与 Linux 对进程发
        // PIDTYPE_TGID 信号一致；成员线程随组消亡。
        Signal::SIGKILL.send_signal_info_to_pcb(Some(&mut info), leader, PidType::TGID)
    });
    log::error!(
        "memcg oom.group: killed {} task groups in subtree of cgroup '{}'",
        killed,
        group.name()
    );
}

/// `memory.oom.group` 子树扫描对单个候选 pid 的探测结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupKillProbe {
    tgid: RawPid,
    protected: bool,
}

/// [`kill_oom_group_subtree`] 的纯选择内核：把按 cgroup 收集的线程 pid
/// 归并到线程组（每组只杀一次），跳过状态机刚刚杀过的主 victim，
/// 跳过受保护组（init / `oom_score_adj == -1000`），返回待 SIGKILL 的
/// tgid（首次出现顺序）。
///
/// 单独拆出并注入探测闭包，使"越限组子树全部 killable 任务都收到
/// kill、受保护任务不被误杀、victim 不被重复投递"的语义无需真实进程表
/// 即可单元测试（见模块尾 `mod tests`）。
fn select_group_kill_targets(
    pids: impl IntoIterator<Item = RawPid>,
    victim_tgid: RawPid,
    mut probe: impl FnMut(RawPid) -> Option<GroupKillProbe>,
) -> Vec<RawPid> {
    let mut targets = Vec::new();
    for pid in pids {
        let Some(entry) = probe(pid) else {
            continue;
        };
        if entry.tgid == victim_tgid || entry.protected {
            continue;
        }
        if !targets.contains(&entry.tgid) {
            targets.push(entry.tgid);
        }
    }
    targets
}

/// 向每个选中的 tgid 通过 `send` 下发组杀并计数成功次数。选择与投递
/// 之间消失的 tgid（`send` 返回 Err）只告警不计入，与 Linux 忽略单次
/// `oom_kill_memcg_member` 迭代失败一致。
fn run_group_kill(
    targets: &[RawPid],
    send: &mut dyn FnMut(RawPid) -> Result<i32, SystemError>,
) -> usize {
    let mut killed = 0usize;
    for tgid in targets {
        match send(*tgid) {
            Ok(_) => killed += 1,
            Err(err) => log::warn!(
                "memcg oom.group: failed to SIGKILL tgid={:?}: {:?}",
                tgid,
                err
            ),
        }
    }
    killed
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 宿主可运行的归属摘取测例（pm31 切片手法：marker 抽取本 mod +
    /// `take_owner_runs`/`FREE_RUN_BATCH` 到独立文件 `rustc --test`
    /// 执行）。owner 用 `Arc<u32>` 替身、`same` 用 `Arc::ptr_eq`，与
    /// 生产语义一致；这些契约就是锁序修复后 free 路径锁外可测的全部
    /// 行为面（irqsave 纪律与"锁内零嵌套零分配"由获取点静态核查，
    /// 见 `kernel/src/cgroup/LOCK_ORDER.md` §5）。
    fn same_ptr(a: &Arc<u32>, b: &Arc<u32>) -> bool {
        Arc::ptr_eq(a, b)
    }

    fn map_of(owners: &[Option<Arc<u32>>]) -> Vec<Option<Arc<u32>>> {
        owners.to_vec()
    }

    fn batch_new() -> [(Option<Arc<u32>>, u64); FREE_RUN_BATCH] {
        core::array::from_fn(|_| (None, 0))
    }

    fn used_runs(batch: &[(Option<Arc<u32>>, u64)]) -> Vec<(*const u32, u64)> {
        batch
            .iter()
            .filter_map(|(owner, len)| owner.as_ref().map(|o| (Arc::as_ptr(o), *len)))
            .collect()
    }

    #[test]
    fn coalesces_same_owner_and_empties_slots() {
        let a = Arc::new(1u32);
        let b = Arc::new(2u32);
        let mut map = map_of(&[Some(a.clone()), Some(a.clone()), Some(b.clone()), None]);
        let mut batch = batch_new();
        let (used, resume) = take_owner_runs(&mut map, 0, 4, same_ptr, &mut batch);
        assert_eq!(resume, None);
        assert_eq!(used, 2);
        let runs = used_runs(&batch);
        assert_eq!(runs, vec![(Arc::as_ptr(&a), 2), (Arc::as_ptr(&b), 1)]);
        // 摘取语义：区间内归属槽位必须全部清空（二次 free 不重复 uncharge）。
        assert!(map.iter().all(|slot| slot.is_none()));
    }

    #[test]
    fn skips_gaps_and_honors_offset() {
        let a = Arc::new(1u32);
        let b = Arc::new(2u32);
        // [None, None, a, b, None, b]：从 pfn 2 起摘 4 页 →
        // runs = (a,1),(b,1)，尾部无主缝隙不占批槽、不报错。
        let mut map = map_of(&[
            None,
            None,
            Some(a.clone()),
            Some(b.clone()),
            None,
            Some(b.clone()),
        ]);
        let mut batch = batch_new();
        let (used, resume) = take_owner_runs(&mut map, 2, 6, same_ptr, &mut batch);
        assert_eq!(resume, None);
        assert_eq!(used, 3);
        let runs = used_runs(&batch);
        assert_eq!(
            runs,
            vec![
                (Arc::as_ptr(&a), 1),
                (Arc::as_ptr(&b), 1),
                (Arc::as_ptr(&b), 1)
            ]
        );
        // 区间外的缝隙槽位不受影响。
        assert!(map[0].is_none() && map[1].is_none());
    }

    #[test]
    fn clamps_past_map_end_and_rejects_out_of_range_start() {
        let a = Arc::new(1u32);
        let mut map = map_of(&[Some(a.clone()), Some(a.clone())]);
        let mut batch = batch_new();
        // 请求 10 页，钳制到 map 长度 2。
        let (used, resume) = take_owner_runs(&mut map, 0, 10, same_ptr, &mut batch);
        assert_eq!(resume, None);
        assert_eq!(used, 1);
        assert_eq!(used_runs(&batch), vec![(Arc::as_ptr(&a), 2)]);
        // 起点越界：零摘取、无续扫（外层 free 循环据此返回）。
        let mut batch = batch_new();
        let (used, resume) = take_owner_runs(&mut map, 5, 3, same_ptr, &mut batch);
        assert_eq!(used, 0);
        assert_eq!(resume, None);
    }

    #[test]
    fn full_batch_resumes_with_remaining_slots_untouched() {
        // FREE_RUN_BATCH + 2 个单页 run：首批装满 → Some(resume)，
        // resume 之后区间原封未动（外层循环靠它续扫）。
        let owners: Vec<Arc<u32>> = (0..(FREE_RUN_BATCH + 2) as u32).map(Arc::new).collect();
        let mut map: Vec<Option<Arc<u32>>> = owners.iter().cloned().map(Some).collect();

        let total = map.len();
        let mut batch = batch_new();
        let (used, resume) = take_owner_runs(&mut map, 0, total, same_ptr, &mut batch);
        assert_eq!(used, FREE_RUN_BATCH);
        let resume = resume.expect("批满必须返回续扫点");
        assert_eq!(resume, FREE_RUN_BATCH);
        let runs = used_runs(&batch);
        for (i, slot) in runs.iter().enumerate() {
            assert_eq!(*slot, (Arc::as_ptr(&owners[i]), 1));
        }
        assert!(map[..resume].iter().all(|s| s.is_none()));
        for (i, slot) in map[resume..].iter().enumerate() {
            assert!(Arc::ptr_eq(slot.as_ref().unwrap(), &owners[resume + i]));
        }

        // 外层循环语义：放锁 uncharge 本批后从 resume 续扫剩余 run。
        let mut batch = batch_new();
        let (used, resume) = take_owner_runs(&mut map, resume, total, same_ptr, &mut batch);
        assert_eq!(resume, None);
        assert_eq!(used, 2);
        let runs = used_runs(&batch);
        assert_eq!(runs[0], (Arc::as_ptr(&owners[FREE_RUN_BATCH]), 1));
        assert!(map.iter().all(|s| s.is_none()));
    }

    #[test]
    fn interleaved_runs_fill_batch_exactly_without_spurious_resume() {
        // a,b,a,b,… 8 页 8 run：恰好装满且区间扫完 → (8, None)。
        // 批满判定先行返回 Some(len) 的"伪续扫"必须不影响正确性
        // （外层会再以 (0, None) 收敛），但这里验证边界不 panic。
        let a = Arc::new(1u32);
        let b = Arc::new(2u32);
        let mut map = map_of(&[
            Some(a.clone()),
            Some(b.clone()),
            Some(a.clone()),
            Some(b.clone()),
            Some(a.clone()),
            Some(b.clone()),
            Some(a.clone()),
            Some(b.clone()),
        ]);
        let mut batch = batch_new();
        let (used, resume) = take_owner_runs(&mut map, 0, 8, same_ptr, &mut batch);
        assert_eq!(used, 8);
        assert!(map.iter().all(|s| s.is_none()));
        // resume 若存在，必须恰为区间终点（无未摘残留）。
        assert!(resume == None || resume == Some(8));
    }

    /// 锁序回归静态论证的可执行部分：`run` 数超过批容量时（8 run 后
    /// 仍有归属），必须返回续扫点而非静默丢弃——否则将泄漏 uncharge
    /// （计数不归零）。`full_batch_resumes…` 覆盖正向；此处覆盖
    /// "续扫收敛"：模拟外层循环直到 (0, None)。
    #[test]
    fn batched_take_converges_like_free_loop() {
        let owners: Vec<Arc<u32>> = (0..(2 * FREE_RUN_BATCH + 3) as u32).map(Arc::new).collect();
        let mut map: Vec<Option<Arc<u32>>> = owners.iter().cloned().map(Some).collect();
        let total = map.len();
        let mut pos = 0usize;
        let mut uncharged_pages = 0u64;
        let mut batches = 0usize;
        loop {
            let mut batch = batch_new();
            let (used, resume) = take_owner_runs(&mut map, pos, total, same_ptr, &mut batch);
            for slot in batch.iter().take(used) {
                uncharged_pages += slot.1;
            }
            batches += 1;
            match resume {
                None => break,
                Some(next) => {
                    assert!(next > pos, "游标必须严格前进");
                    pos = next;
                }
            }
        }
        assert_eq!(uncharged_pages, total as u64);
        assert_eq!(batches, 3);
        assert!(map.iter().all(|s| s.is_none()));
    }

    /// oom.group=1 越限整组被杀（issue #37）：组内每个进程（线程按
    /// tgid 归并）都必须恰好收到一次派发，顺序按首次出现。
    /// 宿主等价测例见 /tmp/pm37-selftest marker-slice（同一份源码）。
    #[test]
    fn group_kill_targets_every_process_in_subtree_once() {
        // pids: 10(leader),11(10 的线程),10,20,30 —— 三个进程。
        let pids = [10usize, 11, 10, 20, 30]
            .iter()
            .map(|p| RawPid::new(*p))
            .collect::<Vec<_>>();
        let table = [
            (10usize, 10usize),
            (11usize, 10usize),
            (20usize, 20usize),
            (30usize, 30usize),
        ];
        let targets = select_group_kill_targets(pids.clone(), RawPid::new(0), |pid| {
            table
                .iter()
                .find(|(pid_, _)| *pid_ == pid.data())
                .map(|(_, tgid)| GroupKillProbe {
                    tgid: RawPid::new(*tgid),
                    protected: false,
                })
        });
        assert_eq!(
            targets,
            vec![RawPid::new(10), RawPid::new(20), RawPid::new(30)],
            "越限组子树全部 killable 任务都必须被下发 kill"
        );
    }

    /// Linux `oom_kill_memcg_member` 的保护条件：init(tgid<=1) 与
    /// oom_score_adj=-1000 不杀；主 victim 已由状态机杀过，扫描跳过。
    #[test]
    fn group_kill_skips_protected_and_main_victim() {
        let pids = [1usize, 2, 7, 8]
            .iter()
            .map(|p| RawPid::new(*p))
            .collect::<Vec<_>>();
        let table = [
            (1usize, true),
            (2usize, true),
            (7usize, false),
            (8usize, false),
        ];
        let targets = select_group_kill_targets(pids.clone(), RawPid::new(7), |pid| {
            table
                .iter()
                .find(|(pid_, _)| *pid_ == pid.data())
                .map(|(_, protected)| GroupKillProbe {
                    tgid: pid,
                    protected: *protected,
                })
        });
        assert_eq!(
            targets,
            vec![RawPid::new(8)],
            "受保护进程（init / adj=-1000）与主 victim 都不许再杀"
        );
    }

    /// 进程表里已消失的 pid 不参与派发（探测返回 None）。
    #[test]
    fn group_kill_ignores_vanished_pids() {
        let pids = [5usize, 6, 7]
            .iter()
            .map(|p| RawPid::new(*p))
            .collect::<Vec<_>>();
        let targets = select_group_kill_targets(pids, RawPid::new(0), |pid| {
            if pid.data() == 6 {
                None
            } else {
                Some(GroupKillProbe {
                    tgid: pid,
                    protected: false,
                })
            }
        });
        assert_eq!(targets, vec![RawPid::new(5), RawPid::new(7)]);
    }

    /// `run_group_kill` 只统计成功投递；失败只告警不计数。
    #[test]
    fn run_group_kill_counts_only_successes() {
        let targets = [10usize, 11, 12]
            .iter()
            .map(|p| RawPid::new(*p))
            .collect::<Vec<_>>();
        let mut sent = Vec::new();
        let killed = run_group_kill(&targets, &mut |tgid| {
            sent.push(tgid);
            if tgid.data() == 11 {
                Err(SystemError::ESRCH)
            } else {
                Ok(0)
            }
        });
        assert_eq!(sent, targets);
        assert_eq!(killed, 2, "ESRCH 的组不计入成功数");
    }

    /// Linux `memory_oom_group_write`：仅 0/1 有效，其它一律 EINVAL。
    #[test]
    fn oom_group_write_accepts_only_zero_and_one() {
        use crate::cgroup::controllers::memory::parse_oom_group_value;
        assert_eq!(parse_oom_group_value("0"), Ok(false));
        assert_eq!(parse_oom_group_value("1"), Ok(true));
        assert_eq!(parse_oom_group_value("1\n"), Ok(true));
        assert_eq!(parse_oom_group_value(" 0 "), Ok(false));
        for bad in ["2", "-1", "max", "", " ", "0x1", "1a", "0.0", "11"] {
            assert_eq!(
                parse_oom_group_value(bad),
                Err(SystemError::EINVAL),
                "Linux 对 {bad:?} 返回 -EINVAL"
            );
        }
    }
}
