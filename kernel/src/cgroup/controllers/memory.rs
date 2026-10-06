/// Memory Controller - 基于 Linux 7.0-rc5 mm/memcontrol.c
///
/// 实现 cgroup v2 内存控制，包括：
/// - memory.current/peak/min/low/high/max：用量与限额
/// - memory.events：low/high/max/oom/oom_kill/oom_group_kill 事件计数
/// - memory.oom.group：0/1；置位时 memory.max 越限 OOM 清理整组子树
/// - memory.stat：详细统计信息
/// - memory.swap.current/max/peak/events：swap 用量与限额
///
/// 参照：
/// - Linux: mm/memcontrol.c (6218行)
/// - 结构：struct mem_cgroup (include/linux/memcontrol.h:202)
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use system_error::SystemError;

use crate::{
    arch::MMArch,
    cgroup::{
        core::CgroupNode,
        subsys::{CfType, CfTypeFlags, CgroupSubsys, CgroupSubsysId, CgroupSubsysState, CssFlags},
    },
    libs::spinlock::SpinLock,
    mm::{page::PageReclaimer, MemoryManagementArch},
};
const PAGE_SIZE: usize = MMArch::PAGE_SIZE;

/// Serializes hierarchical charge transactions.  Holding this lock across the
/// limit checks and updates makes a charge all-or-nothing for the whole CSS
/// ancestry, rather than only for the leaf CSS.
///
/// 中断纪律（issue #28，全序图 `kernel/src/cgroup/LOCK_ORDER.md` §1.1）：
/// 本锁及其内层的 `MemoryCss::inner`/`flags` 一律 `lock_irqsave()` 获取——
/// 硬中断的页帧释放路径会经 `mm::memcg::memcg_free_uncharge` 单独取得
/// 本锁族（free 两段式：PAGE_OWNERS 内只摘归属，放锁后才进到这里），
/// IRQ-on 持有者会被同 CPU 硬中断重进非重入 CAS 自旋锁而死锁。
/// 临界区内禁止睡眠/分配/回调页分配器/获取调度器锁；与 PAGE_OWNERS
/// 互不嵌套（不变式 I3）。
static MEMORY_CHARGE_LOCK: SpinLock<()> = SpinLock::new(());

/// Memory 控制器状态
///
/// 对应 Linux: struct mem_cgroup
#[derive(Debug)]
pub struct MemoryCss {
    /// 父 CSS 弱引用
    parent: Option<Weak<dyn CgroupSubsysState>>,
    /// 所属 cgroup 节点的弱引用
    cgroup: Weak<CgroupNode>,
    /// 本 CSS 诞生时的节点 id 快照（代际校验）。
    ///
    /// 节点 id 在 `CgroupRoot` 内单调递增且 rmdir 永不复用：页面归属
    /// 表记录 (Weak\<CSS\>, node_id) 对，释页时两重校验同时通过才
    /// 结算计费，归属记录因此可以用弱引用——被删组的 CSS 不再被
    /// 每帧强引用钉住（issue #30）。
    node_id: usize,
    /// rmdir 拆除标记。
    ///
    /// `css_offline` 置位：残余计费已结算回祖先链、本 CSS 清零；
    /// 此后新计费一律跳过（不计本 CSS 也不计祖先），释页同样跳过，
    /// 祖先计数才不会被残余页面的归还二次回退。
    offline: AtomicBool,
    /// 状态标志
    flags: SpinLock<CssFlags>,
    /// 内存计数器
    inner: SpinLock<MemoryCssInner>,
    /// Set when a charge crossed `memory.high` on this CSS; cleared by the
    /// fault-path throttle (`mm::memcg::memcg_handle_over_high`).  This keeps
    /// the charge path free of sleeping/reclaim work while still throttling
    /// the cgroup's own tasks on their next fault (Linux: the memcg throttle
    /// runs in `mem_cgroup_handle_over_high()`, not in `try_charge`).
    high_trip: AtomicBool,
}

#[derive(Debug)]
struct MemoryCssInner {
    /// 当前内存用量（页数）
    usage: u64,
    /// 峰值用量（页数）
    peak: u64,
    /// memory.min 限额（页数，None = 0）
    min: Option<u64>,
    /// memory.low 限额（页数，None = 0）
    low: Option<u64>,
    /// memory.high 限额（页数，None = max）
    high: Option<u64>,
    /// memory.max 限额（页数，None = 无限制）
    max: Option<u64>,
    /// swap 用量（页数）
    swap_usage: u64,
    /// swap 峰值（页数）
    swap_peak: u64,
    /// memory.swap.high 限额
    swap_high: Option<u64>,
    /// memory.swap.max 限额
    swap_max: Option<u64>,
    /// memory.events 计数器
    events: MemoryEvents,
    /// memory.oom.group：是否杀死整个组
    oom_group: bool,
}

#[derive(Debug, Default)]
struct MemoryEvents {
    low: AtomicU64,
    high: AtomicU64,
    max: AtomicU64,
    oom: AtomicU64,
    oom_kill: AtomicU64,
    oom_group_kill: AtomicU64,
}

/// 一次成功计费的归属令牌：所有者弱引用 + 代际校验号。
///
/// 页面归属表（`mm::memcg::PAGE_OWNERS`）每帧存一份本令牌。用弱引用
/// 而非 `Arc` 是 issue #30 泄漏修复的核心：被 rmdir 的组的 CSS 一旦
/// 失去节点 subsys 槽的最后强引用即可回收，即使它的页面还活着；
/// `node_id` 提供第二重校验——节点 id 在 `CgroupRoot` 内单调递增、
/// 永不复用，upgrade 成功且代际匹配时令牌必然指向计费当时那个 CSS
/// （同地址复活的旧对象不可能代际相同）。
///
/// 释页方只需要令牌，计费方（新分配）只需要当前任务的在线 CSS，
/// 两侧经令牌解耦、互不持有强引用（保持一期"释放方/计费方解耦"）。
#[derive(Debug, Clone)]
pub struct ChargeToken {
    owner: Weak<dyn CgroupSubsysState>,
    node_id: usize,
}

impl ChargeToken {
    /// 从计费成功的 CSS 铸造令牌。
    pub fn new(css: &Arc<dyn CgroupSubsysState>) -> Self {
        let node_id = css
            .as_any()
            .downcast_ref::<MemoryCss>()
            .map(|memcg| memcg.node_id())
            .unwrap_or(0);
        Self {
            owner: Arc::downgrade(css),
            node_id,
        }
    }

    /// 代际号：purge 与归属校验的身份判据（每节点恰有一个 MemoryCss，
    /// id 全局唯一且不复用，故 id 相等 ⇔ CSS 同一）。
    pub fn node_id(&self) -> usize {
        self.node_id
    }

    /// run 合并判据：同一所有者即同一段连续归属。
    pub fn same_owner(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.owner, &other.owner)
    }

    /// 解析令牌：所有者仍存活且代际匹配时返回 CSS 强引用。
    ///
    /// `None` 表示 CSS 已随 cgroup 回收（或其强引用恰在此刻消失），
    /// 释页侧据此跳过结算——对应计费已在 `css_offline` 结算时从祖先
    /// 链回退，再扣即双扣。
    pub fn resolve(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        if self.node_id == 0 {
            return None;
        }
        let css = self.owner.upgrade()?;
        let memcg = css.as_any().downcast_ref::<MemoryCss>()?;
        (memcg.node_id() == self.node_id).then_some(css)
    }
}

impl MemoryCss {
    /// `cgroup` 与 `node_id` 必须来自同一在线节点：CSS 诞生时记下
    /// 节点 id 作为代际号，节点本身只保留弱引用。在线期间的节点
    /// 存活保证来自节点自身的 subsys 槽与在线注册表（`remove_child`
    /// 先 `css_offline` 结算、再 `clear_css`），CSS 不需要反向强引用
    /// 节点，杜绝 CSS↔Node 的 Arc 循环。
    pub fn new(
        parent: Option<Arc<dyn CgroupSubsysState>>,
        cgroup: Weak<CgroupNode>,
        node_id: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            parent: parent.map(|p| Arc::downgrade(&p) as Weak<dyn CgroupSubsysState>),
            cgroup,
            node_id,
            offline: AtomicBool::new(false),
            flags: SpinLock::new(CssFlags::default()),
            inner: SpinLock::new(MemoryCssInner {
                usage: 0,
                peak: 0,
                min: None,
                low: None,
                high: None,
                max: None,
                swap_usage: 0,
                swap_peak: 0,
                swap_high: None,
                swap_max: None,
                events: MemoryEvents::default(),
                oom_group: false,
            }),
            high_trip: AtomicBool::new(false),
        })
    }

    /// 本 CSS 诞生时的节点 id（代际号）。
    pub(crate) fn node_id(&self) -> usize {
        self.node_id
    }

    /// 是否已被 rmdir 拆除：残余计费已结算回祖先、本 CSS 清零，
    /// 新计费与残余释页都不再触碰任何计数器。
    pub(crate) fn is_offline(&self) -> bool {
        self.offline.load(Ordering::Acquire)
    }

    /// Visit every memory CSS ancestor without allocating on the charge path.
    fn for_each_ancestor(&self, mut visit: impl FnMut(&MemoryCss)) {
        let mut parent = self.parent();
        while let Some(css) = parent {
            parent = css.parent();
            if let Some(memcg) = css.as_any().downcast_ref::<MemoryCss>() {
                visit(memcg);
            }
        }
    }

    pub(crate) fn high_limit_exceeded(&self) -> bool {
        let mut exceeded = {
            let inner = self.inner.lock_irqsave();
            inner.high.is_some_and(|high| inner.usage > high)
        };
        self.for_each_ancestor(|memcg| {
            if exceeded {
                return;
            }
            let inner = memcg.inner.lock_irqsave();
            exceeded = inner.high.is_some_and(|high| inner.usage > high);
        });
        exceeded
    }
    fn uncharge_chain(&self, pages: u64) {
        if let Some(parent) = self.parent() {
            if let Some(memcg) = parent.as_any().downcast_ref::<MemoryCss>() {
                memcg.uncharge_chain(pages);
            }
        }
        let mut inner = self.inner.lock_irqsave();
        inner.usage = inner.usage.saturating_sub(pages);
    }

    /// 沿父链逐级回退 `pages`（不含自身）。与 `uncharge_chain` 的
    /// 区别：只动祖先——供 rmdir 残余计费结算使用（叶子由调用方
    /// 在同一临界区内清零）。必须在持有 `MEMORY_CHARGE_LOCK` 时调用，
    /// 与任何在途 charge/uncharge 事务互斥。
    fn uncharge_ancestors(&self, pages: u64) {
        if let Some(parent) = self.parent() {
            if let Some(memcg) = parent.as_any().downcast_ref::<MemoryCss>() {
                let mut inner = memcg.inner.lock_irqsave();
                inner.usage = inner.usage.saturating_sub(pages);
                drop(inner);
                memcg.uncharge_ancestors(pages);
            }
        }
    }
    /// Take and clear the `memory.high` trip flag set by the last refused
    /// over-high charge (see [`Self::high_trip`]).
    pub(crate) fn take_high_trip(&self) -> bool {
        self.high_trip.swap(false, Ordering::Relaxed)
    }

    /// Whether this CSS's usage is still at or above its own `memory.max`.
    pub(crate) fn max_exceeded_now(&self) -> bool {
        let inner = self.inner.lock_irqsave();
        inner.max.is_some_and(|max| inner.usage >= max)
    }
    /// Count a `memory.events` `oom` event (scoped OOM machinery entered).
    pub(crate) fn note_memcg_oom(&self) {
        self.inner
            .lock_irqsave()
            .events
            .oom
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count a `memory.events` `oom_kill` event (victim selected and killed).
    pub(crate) fn note_memcg_oom_kill(&self) {
        self.inner
            .lock_irqsave()
            .events
            .oom_kill
            .fetch_add(1, Ordering::Relaxed);
    }

    /// 尝试对指定页数计费
    ///
    /// 对 leaf 与全部祖先 CSS 做事务式检查与更新：先在全局事务锁下检查
    /// 每一层的 `memory.max`，任何一层超限则完全不更新（无需回滚），
    /// 全部通过后才逐层累加 usage 并更新 peak。对应 Linux 的
    /// `try_charge`/`commit_charge`。
    ///
    /// 本函数运行在页分配器内部（可能持有 page manager 锁），因此绝不
    /// 睡眠、不回收、不执行 OOM 选择：
    /// - 越过 `memory.high` 时只设置 trip 标志并唤醒回收线程，节流由
    ///   缺页路径的 `mm::memcg::memcg_handle_over_high()` 完成；
    /// - 被 `memory.max` 拒绝时由调用方（`mm::memcg`）记录 pending，
    ///   由 `oom::pagefault_out_of_memory` 排水到 `oom.rs` 状态机执行
    ///   cgroup 范围内的 OOM kill。
    pub fn try_charge(&self, pages: u64) -> Result<(), SystemError> {
        // rmdir 之后本 CSS 已注销：残余计费已结算回祖先，绝不能再把
        // 用量塞回一个没有 cgroup 的计数器（也不计祖先——释页侧按
        // 同一门控跳过，两侧对称才不会撕裂祖先计数）。快速路径在
        // 锁外读；提交前在 MEMORY_CHARGE_LOCK 内复核（见下），与
        // css_offline 的"置位→取锁→取残量"顺序共同封死竞态。
        if self.is_offline() {
            return Ok(());
        }
        let mut high_exceeded = false;
        let mut max_exceeded = false;

        {
            let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();

            // 复核：css_offline 持同一把锁结算残量；若在上面的锁外
            // 检查之后才置位，此处必然观察到——本笔计费不会在结算
            // 之后残留成无人认领的 usage。
            if self.is_offline() {
                return Ok(());
            }

            // Check every limit before changing any usage. The global
            // transaction lock prevents another charger from consuming the
            // capacity between these checks and the updates below.
            {
                let inner = self.inner.lock_irqsave();
                if inner
                    .max
                    .is_some_and(|max| inner.usage.saturating_add(pages) > max)
                {
                    inner.events.max.fetch_add(1, Ordering::Relaxed);
                    max_exceeded = true;
                }
            }
            if !max_exceeded {
                self.for_each_ancestor(|memcg| {
                    if max_exceeded {
                        return;
                    }
                    let inner = memcg.inner.lock_irqsave();
                    if inner
                        .max
                        .is_some_and(|max| inner.usage.saturating_add(pages) > max)
                    {
                        inner.events.max.fetch_add(1, Ordering::Relaxed);
                        max_exceeded = true;
                    }
                });
            }
            if !max_exceeded {
                {
                    let mut inner = self.inner.lock_irqsave();
                    inner.usage = inner.usage.saturating_add(pages);
                    if inner.usage > inner.peak {
                        inner.peak = inner.usage;
                    }
                    if inner.high.is_some_and(|high| inner.usage > high) {
                        inner.events.high.fetch_add(1, Ordering::Relaxed);
                        high_exceeded = true;
                        // Trip this CSS so its own tasks throttle on their
                        // next fault; repeated trips only re-set the flag.
                        self.high_trip.store(true, Ordering::Relaxed);
                    }
                }
                self.for_each_ancestor(|memcg| {
                    let mut inner = memcg.inner.lock_irqsave();
                    inner.usage = inner.usage.saturating_add(pages);
                    if inner.usage > inner.peak {
                        inner.peak = inner.usage;
                    }
                    if inner.high.is_some_and(|high| inner.usage > high) {
                        inner.events.high.fetch_add(1, Ordering::Relaxed);
                        high_exceeded = true;
                        memcg.high_trip.store(true, Ordering::Relaxed);
                    }
                });
            }
        }

        if max_exceeded {
            // The charge was never applied to any CSS, so there is nothing
            // to roll back. The refusal itself (and the scoped OOM kill) is
            // handled by the caller on the fault path.
            return Err(SystemError::ENOMEM);
        }
        if high_exceeded {
            // memory.high is a throttle, not an allocation failure: ask the
            // background reclaimer to trim caches. This is allocation-free
            // and lock-hierarchy-safe, so it may run from the charge path.
            PageReclaimer::wakeup_claim_thread();
        }
        Ok(())
    }

    /// 释放指定页数的计费
    pub fn uncharge(&self, pages: u64) {
        // 与 try_charge 对称的门控：孤儿 CSS（rmdir 时残量已结算回
        // 祖先并清零）上的释页对应"已随结算归还"的计费，此处必须
        // 整体跳过，否则祖先 usage 被二次减扣。
        if self.is_offline() {
            return;
        }
        let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();
        // 锁内复核：`css_offline` 持同一把锁结算残量。若置位发生在
        // 上面的快速检查与本行之间，那笔计费已经计入残量、由结算
        // 归还给祖先——此处再走链就是双扣，必须跳过。
        if self.is_offline() {
            return;
        }

        // Recursing to the root before decrementing the leaf gives the
        // exact inverse order of the charge transaction.
        self.uncharge_chain(pages);
    }

    /// 读取当前用量（字节）
    pub fn current(&self) -> u64 {
        let inner = self.inner.lock_irqsave();
        inner.usage * PAGE_SIZE as u64
    }

    /// 读取峰值用量（字节）
    pub fn peak(&self) -> u64 {
        let inner = self.inner.lock_irqsave();
        inner.peak * PAGE_SIZE as u64
    }

    /// 设置 memory.min（字节）
    pub fn set_min(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.min = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    /// 设置 memory.low（字节）
    pub fn set_low(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.low = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    /// 设置 memory.high（字节）
    pub fn set_high(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();
        let mut inner = self.inner.lock_irqsave();
        inner.high = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    /// 设置 memory.max（字节）
    pub fn set_max(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();
        let mut inner = self.inner.lock_irqsave();
        inner.max = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }
    pub fn set_swap_high(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.swap_high = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }

    pub fn set_swap_max(&self, bytes: Option<u64>) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        inner.swap_max = bytes.map(|b| b / PAGE_SIZE as u64);
        Ok(())
    }
    pub fn min(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .min
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn low(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .low
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn high(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .high
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn max(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .max
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn swap_current(&self) -> u64 {
        self.inner.lock_irqsave().swap_usage * PAGE_SIZE as u64
    }

    pub fn swap_peak(&self) -> u64 {
        self.inner.lock_irqsave().swap_peak * PAGE_SIZE as u64
    }

    pub fn swap_high(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .swap_high
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    pub fn swap_max(&self) -> Option<u64> {
        self.inner
            .lock_irqsave()
            .swap_max
            .map(|pages| pages * PAGE_SIZE as u64)
    }

    /// 读取 memory.events
    pub fn events(&self) -> String {
        let inner = self.inner.lock_irqsave();
        format!(
            "low {}\nhigh {}\nmax {}\noom {}\noom_kill {}\noom_group_kill {}\n",
            inner.events.low.load(Ordering::Relaxed),
            inner.events.high.load(Ordering::Relaxed),
            inner.events.max.load(Ordering::Relaxed),
            inner.events.oom.load(Ordering::Relaxed),
            inner.events.oom_kill.load(Ordering::Relaxed),
            inner.events.oom_group_kill.load(Ordering::Relaxed)
        )
    }

    /// 读取 memory.oom.group（Linux: `memcg->oom_group`）
    pub fn oom_group(&self) -> bool {
        self.inner.lock_irqsave().oom_group
    }

    /// 设置 memory.oom.group（Linux: `memory_oom_group_write`）
    pub fn set_oom_group(&self, enabled: bool) -> Result<(), SystemError> {
        self.inner.lock_irqsave().oom_group = enabled;
        Ok(())
    }

    /// `memory.oom.group` 事件计数。对应 Linux 的 MEMCG_OOM_GROUP_KILL：
    /// 一次越限 OOM 触发整组清理时递增一次（而不是每个被杀任务一次）。
    pub(crate) fn note_memcg_oom_group_kill(&self) {
        self.inner
            .lock_irqsave()
            .events
            .oom_group_kill
            .fetch_add(1, Ordering::Relaxed);
    }

    /// 对应 Linux `mem_cgroup_get_oom_group` 的层级遍历：从 victim 的 memory
    /// CSS 沿父链向上，直到（并包含）OOM 域 CSS `domain` 为止，返回路径上
    /// **最高一层**置位 `memory.oom.group` 的 CSS；没有任何一层置位则返回
    /// `None`。若 victim 的链在到达 `domain` 前就终止（victim 已迁出越限
    /// 子树），与 Linux 相同忽略 `memory.oom.group`，避免误杀域外任务。
    /// 根 CSS 在 cgroup2 文件面上不暴露 memory.oom.group（NotOnRoot），
    /// 因此遍历域内的置位者必然属于用户子树。
    pub(crate) fn find_oom_group(
        victim: &Arc<dyn CgroupSubsysState>,
        domain: &Arc<dyn CgroupSubsysState>,
    ) -> Option<Arc<dyn CgroupSubsysState>> {
        let mut found: Option<Arc<dyn CgroupSubsysState>> = None;
        let mut current = Some(victim.clone());
        while let Some(css) = current {
            if css
                .as_any()
                .downcast_ref::<MemoryCss>()
                .is_some_and(|memcg| memcg.oom_group())
            {
                // 持续向上覆盖，留下的即最高层置位者。
                found = Some(css.clone());
            }
            if Arc::ptr_eq(&css, domain) {
                return found;
            }
            current = css.parent();
        }
        None
    }

    /// 读取 memory.stat（简化版）
    pub fn stat(&self) -> String {
        let inner = self.inner.lock_irqsave();
        format!(
            "anon {}\nfile 0\nkernel 0\npagetables 0\nslab 0\nsock 0\n\
             file_mapped 0\nfile_dirty 0\nfile_writeback 0\n\
             inactive_anon 0\nactive_anon {}\ninactive_file 0\nactive_file 0\n\
             unevictable 0\nslab_reclaimable 0\nslab_unreclaimable 0\n\
             pgfault 0\npgmajfault 0\n",
            inner.usage * PAGE_SIZE as u64,
            inner.usage * PAGE_SIZE as u64
        )
    }
}

impl CgroupSubsysState for MemoryCss {
    fn subsys_id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Memory
    }

    fn cgroup_node(&self) -> Option<Arc<CgroupNode>> {
        // fail-closed（issue #30）：先查 offline——`css_offline` 之后
        // 节点归属即注销，即使节点 Arc 因拆除竞态暂存也不返回。
        // 旧实现 `upgrade().expect("cgroup node dropped")` 在缺页排水
        // 路径上 panic；调用方只可能拿到 Some/None 两种显式结果。
        if self.is_offline() {
            return None;
        }
        self.cgroup.upgrade()
    }

    fn parent(&self) -> Option<Arc<dyn CgroupSubsysState>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    fn flags(&self) -> CssFlags {
        *self.flags.lock_irqsave()
    }

    fn set_flags(&self, flags: CssFlags) {
        *self.flags.lock_irqsave() = flags;
    }

    fn css_online(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn css_offline(&self) -> Result<(), SystemError> {
        // rmdir 拆除点（issue #30）。顺序即协议：
        // 1) 先置 offline 位：`try_charge`/`uncharge` 的锁内复核、
        //    `cgroup_node` 与 mm::memcg 存入 pending 前的复核全部
        //    以本位为准；
        // 2) 再清 pending OOM：解绑指向本 CSS 的离组强引用——滞留
        //    的 pending 会让下一次缺页排水引用已删除的节点。与 1)
        //    合起来封死"拒绝→置位→存入"竞态：存入侧在 PENDING 锁
        //    内复核 offline 且置位先于清 pending，任何存入要么发生
        //    在清锁之前（被本步清掉），要么观察到 offline 而放弃；
        // 3) 最后结算残余计费：叶子清零与祖先回退在同一计费事务锁
        //    临界区内完成，try_charge/uncharge 也在该锁内复核
        //    offline——结算后既不残留新计费，残余释页也不会双扣，
        //    维持 usage(x) == Σ{x 在线子树内在线 CSS 计费} 不变式。
        //    结算后本 CSS 的计数冻结在 0：它只作为页面归属表弱引用
        //    令牌的"代际墓碑"存在，最后一个强引用（节点 subsys 槽
        //    的克隆）随 clear_css 消失即可回收。
        self.offline.store(true, Ordering::Release);
        crate::mm::memcg::memcg_forget_css(self);
        let residual = {
            let _charge_guard = MEMORY_CHARGE_LOCK.lock_irqsave();
            let mut inner = self.inner.lock_irqsave();
            let residual = inner.usage;
            if residual > 0 {
                inner.usage = 0;
                self.uncharge_ancestors(residual);
            }
            residual
        };
        if residual > 0 {
            log::warn!(
                "memcg offline: settled {} bytes of residual charge back to ancestors",
                residual * PAGE_SIZE as u64
            );
        }
        Ok(())
    }

    fn can_fork(
        &self,
        _task: &Arc<crate::process::ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // Memory controller 不阻止 fork
        Ok(())
    }

    fn fork(&self, _task: &Arc<crate::process::ProcessControlBlock>) {}

    fn exit(&self, _task: &Arc<crate::process::ProcessControlBlock>) {
        // 进程退出时，mm 子系统会调用 uncharge
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Memory 控制器定义
#[derive(Debug)]
pub struct MemoryController;

impl MemoryController {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl CgroupSubsys for MemoryController {
    fn id(&self) -> CgroupSubsysId {
        CgroupSubsysId::Memory
    }

    fn name(&self) -> &'static str {
        "memory"
    }

    fn css_alloc(
        &self,
        parent: Option<&Arc<dyn CgroupSubsysState>>,
        cgroup: &Arc<CgroupNode>,
    ) -> Result<Arc<dyn CgroupSubsysState>, SystemError> {
        // 代际号与弱引用取自同一在线节点（issue #30）
        Ok(MemoryCss::new(
            parent.cloned(),
            Arc::downgrade(cgroup),
            cgroup.id(),
        ))
    }

    fn css_free(&self, _css: &Arc<dyn CgroupSubsysState>) {}

    fn dfl_cftypes(&self) -> Vec<CfType> {
        vec![
            CfType::new("memory.current")
                .with_read(memory_current_read)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.peak")
                .with_read(memory_peak_read)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.min")
                .with_read(memory_min_read)
                .with_write(memory_min_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.low")
                .with_read(memory_low_read)
                .with_write(memory_low_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.high")
                .with_read(memory_high_read)
                .with_write(memory_high_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.max")
                .with_read(memory_max_read)
                .with_write(memory_max_write)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.stat")
                .with_read(memory_stat_read)
                .with_flags(CfTypeFlags::new()),
            CfType::new("memory.oom.group")
                .with_read(memory_oom_group_read)
                .with_write(memory_oom_group_write)
                .with_flags(CfTypeFlags::new()),
        ]
    }
}

// ========== 文件读写函数 ==========

fn memory_current_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(format!("{}\n", mem.current()))
}

fn memory_peak_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(format!("{}\n", mem.peak()))
}

fn memory_min_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(format!(
        "{}\n",
        inner.min.map(|p| p * PAGE_SIZE as u64).unwrap_or(0)
    ))
}

fn memory_min_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "0" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_min(bytes)
}

fn memory_low_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(format!(
        "{}\n",
        inner.low.map(|p| p * PAGE_SIZE as u64).unwrap_or(0)
    ))
}

fn memory_low_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "0" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_low(bytes)
}

fn memory_high_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(match inner.high {
        Some(pages) => format!("{}\n", pages * PAGE_SIZE as u64),
        None => "max\n".to_string(),
    })
}

fn memory_high_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "max" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_high(bytes)
}

fn memory_max_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let inner = mem.inner.lock_irqsave();
    Ok(match inner.max {
        Some(pages) => format!("{}\n", pages * PAGE_SIZE as u64),
        None => "max\n".to_string(),
    })
}

fn memory_max_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let trimmed = buf.trim();
    let bytes = if trimmed == "max" {
        None
    } else {
        Some(trimmed.parse::<u64>().map_err(|_| SystemError::EINVAL)?)
    };
    mem.set_max(bytes)
}

/// 解析 memory.oom.group 的写入值。对应 Linux
/// `memory_oom_group_write`：`kstrtoint` 后仅接受 0/1，其它一律 -EINVAL。
pub fn parse_oom_group_value(buf: &str) -> Result<bool, SystemError> {
    match buf.trim() {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(SystemError::EINVAL),
    }
}

fn memory_oom_group_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(format!("{}\n", mem.oom_group() as u8))
}

fn memory_oom_group_write(css: &Arc<dyn CgroupSubsysState>, buf: &str) -> Result<(), SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    let enabled = parse_oom_group_value(buf)?;
    mem.set_oom_group(enabled)
}

fn memory_events_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(mem.events())
}

fn memory_stat_read(css: &Arc<dyn CgroupSubsysState>) -> Result<String, SystemError> {
    let mem = css
        .as_any()
        .downcast_ref::<MemoryCss>()
        .ok_or(SystemError::EINVAL)?;
    Ok(mem.stat())
}

/// 初始化 Memory 控制器
pub fn init_memory_controller() {
    let controller = MemoryController::new();
    crate::cgroup::subsys::register_subsys(controller);
}
